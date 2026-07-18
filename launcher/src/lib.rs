mod cmd;
pub mod config;
mod docker;
mod error;
pub mod events;
mod mount;
pub mod server;
pub mod validate;

use std::any::Any;
use std::future::Future;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use listenfd::ListenFd;
use nix::sys::socket::{getsockopt, sockopt};
use tokio::net::{UnixListener, UnixStream};
use tracing::info;

use crate::cmd::{log_info, log_warn};
use crate::server::handle_request;

pub use config::Config;
pub use config::PREFIX;
pub use server::AppState;
pub use validate::Validators;

pub const VERSION: &str = include_str!("../../VERSION").trim_ascii();

pub fn version_string() -> String {
    botwork_version::format_full(VERSION, botwork_version::GIT_SHA)
}

const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn run() -> Result<(), String> {
    info!("{PREFIX} botwork-launcher {}", version_string());
    let config = Config::from_env()?;
    let validators = Validators::new(&config.image_allowlist_regex)
        .map_err(|err| format!("invalid BOTWORK_LAUNCHER_IMAGE_ALLOWLIST_REGEX: {err}"))?;
    let listener = match listener_from_activation(&config.socket_path)? {
        Some(listener) => listener,
        None => bind_listener(&config)?,
    };

    log_info(&format!("listening on unix://{}", config.socket_path));

    let broker_socket_path = config.broker_socket_path.clone();
    let state = Arc::new(AppState { config, validators });

    tokio::spawn(events::run_event_loop(broker_socket_path));

    serve_on(listener, state).await
}

pub async fn serve_on(listener: UnixListener, state: Arc<AppState>) -> Result<(), String> {
    log_info("accept loop ready");

    loop {
        let (stream, peer_pid) = accept_next_stream(
            &mut || async { listener.accept().await.map(|(stream, _)| stream) },
            state.config.allowed_peer_uid,
            state.config.allowed_peer_gid,
        )
        .await;
        spawn_connection_task(stream, Arc::clone(&state), peer_pid);
    }
}

fn listener_from_activation(socket_path: &str) -> Result<Option<UnixListener>, String> {
    listener_from_listenfd(ListenFd::from_env(), socket_path)
}

/// Adopt a systemd-passed unix socket through `listenfd`.
///
/// `listenfd::take_unix_listener` does the heavy lifting we used to do
/// hand-rolled with libc:
///
/// * confirms the fd is open and is a socket (via `fstat`),
/// * confirms the family is `AF_UNIX` and the type is `SOCK_STREAM`,
/// * sets `FD_CLOEXEC`,
/// * hands back a `std::os::unix::net::UnixListener` we then promote to
///   tokio.
///
/// What `listenfd` does **not** check, and we still do:
///
/// * the LISTEN_FDS count is exactly 1 — production systemd units pass
///   exactly one socket, anything else is a misconfig we want to fail
///   loud at startup;
/// * the inherited fd is in `listen(2)` state (`SO_ACCEPTCONN == 1`) —
///   belt-and-braces in case someone sets `LISTEN_FDS` manually with a
///   non-listening fd at position 0.
fn listener_from_listenfd(
    mut listenfd: ListenFd,
    socket_path: &str,
) -> Result<Option<UnixListener>, String> {
    match listenfd.len() {
        0 => {
            log_info(&format!("self-bind: no LISTEN_FDS, binding {socket_path}"));
            Ok(None)
        }
        1 => {
            let std_listener = listenfd
                .take_unix_listener(0)
                .map_err(|err| {
                    format!("systemd socket activation fd 0 is not a unix stream socket: {err}")
                })?
                .ok_or_else(|| "missing activated unix socket descriptor".to_string())?;
            // SO_ACCEPTCONN check used to live next to the AF_UNIX /
            // SOCK_STREAM checks; listenfd covers the latter two but not
            // this one, so keep it explicit.
            let accepting = getsockopt(&std_listener, sockopt::AcceptConn).map_err(|err| {
                format!("failed to inspect activated socket SO_ACCEPTCONN: {err}")
            })?;
            if !accepting {
                return Err("systemd socket activation fd 0 is not a listening socket".to_string());
            }
            std_listener
                .set_nonblocking(true)
                .map_err(|err| format!("failed to set activated socket nonblocking: {err}"))?;
            let fd = std_listener.as_raw_fd();
            let listener = UnixListener::from_std(std_listener)
                .map_err(|err| format!("failed to adopt activated socket: {err}"))?;
            log_info(&format!(
                "socket-activated: using fd {fd} from systemd, path={socket_path}"
            ));
            Ok(Some(listener))
        }
        count => Err(format!(
            "socket activation expected exactly one socket, got {count}"
        )),
    }
}

fn bind_listener(config: &Config) -> Result<UnixListener, String> {
    let socket_path = Path::new(&config.socket_path);
    let parent = socket_path
        .parent()
        .ok_or_else(|| format!("invalid socket path {}", socket_path.display()))?;

    std::fs::create_dir_all(parent).map_err(|err| {
        format!(
            "failed to create socket directory {}: {err}",
            parent.display()
        )
    })?;

    match std::fs::remove_file(socket_path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("failed to unlink {}: {err}", socket_path.display())),
    }

    let listener = UnixListener::bind(socket_path)
        .map_err(|err| format!("failed to bind {}: {err}", socket_path.display()))?;

    let socket_mode = if config.socket_group.is_some() {
        0o660
    } else {
        0o600
    };
    // This socket is the only thing between a local uid and root — do not loosen it.
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(socket_mode))
        .map_err(|err| format!("failed to chmod {}: {err}", socket_path.display()))?;
    if let Some(socket_group) = config.socket_group {
        // This socket is the only thing between a local uid and root — do not loosen it.
        chown_group(socket_path, socket_group)?;
    }
    // Self-bind must never create a world-accessible launcher socket; socket activation has to set
    // SocketMode=0660 and SocketGroup=... in the systemd .socket unit instead.

    Ok(listener)
}

async fn accept_next_stream<F, Fut>(
    accept: &mut F,
    allowed_uid: Option<u32>,
    allowed_gid: Option<u32>,
) -> (UnixStream, Option<u32>)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<UnixStream>>,
{
    loop {
        match accept().await {
            Ok(stream) => {
                let credentials = peer_credentials(&stream);
                let peer_pid = credentials.and_then(|creds| creds.pid);
                if !credentials
                    .map(|creds| peer_is_allowed(creds.uid, creds.gid, allowed_uid, allowed_gid))
                    .unwrap_or(false)
                {
                    let peer = credentials
                        .map(|creds| creds.describe())
                        .unwrap_or_else(|| "uid=unknown gid=unknown pid=unknown".to_string());
                    // This check is belt-and-braces with the socket perms; do not loosen it.
                    log_warn(&format!("rejected unauthorized peer ({peer})"));
                    drop(stream);
                    continue;
                }
                log_info(&format!(
                    "accepted connection (peer_pid={})",
                    peer_pid_label(peer_pid)
                ));
                return (stream, peer_pid);
            }
            Err(err) => {
                log_info(&format!("accept error: {err}"));
            }
        }
    }
}

fn spawn_connection_task(stream: UnixStream, state: Arc<AppState>, peer_pid: Option<u32>) {
    let peer_pid = peer_pid_label(peer_pid);
    let join = tokio::spawn(async move {
        let io = TokioIo::new(stream);
        let service = service_fn(move |request| handle_request(request, Arc::clone(&state)));
        let mut builder = http1::Builder::new();
        builder.timer(TokioTimer::new());
        builder.header_read_timeout(HEADER_READ_TIMEOUT);
        builder.keep_alive(false);
        if let Err(err) = builder.serve_connection(io, service).await {
            log_info(&format!("connection error (peer_pid={peer_pid}): {err}"));
        }
    });

    tokio::spawn(async move {
        match join.await {
            Ok(()) => {}
            Err(err) if err.is_panic() => {
                log_info(&format!(
                    "connection task panicked: {}",
                    panic_payload(err.into_panic())
                ));
            }
            Err(err) => {
                log_info(&format!("connection task join error: {err}"));
            }
        }
    });
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct PeerCredentials {
    uid: u32,
    gid: u32,
    pid: Option<u32>,
}

impl PeerCredentials {
    fn describe(self) -> String {
        format!(
            "uid={} gid={} pid={}",
            self.uid,
            self.gid,
            peer_pid_label(self.pid)
        )
    }
}

fn peer_credentials(stream: &UnixStream) -> Option<PeerCredentials> {
    // `nix::sys::socket::sockopt::PeerCredentials` issues
    // `getsockopt(SO_PEERCRED)` and returns a `UnixCredentials` wrapper.
    // tokio's `UnixStream` implements `AsFd`, so the call is fully safe.
    let creds = getsockopt(stream, sockopt::PeerCredentials).ok()?;
    let raw_pid = creds.pid();
    let pid = if raw_pid > 0 {
        u32::try_from(raw_pid).ok()
    } else {
        None
    };
    Some(PeerCredentials {
        uid: creds.uid(),
        gid: creds.gid(),
        pid,
    })
}

fn peer_is_allowed(
    peer_uid: u32,
    peer_gid: u32,
    allowed_uid: Option<u32>,
    allowed_gid: Option<u32>,
) -> bool {
    // UID and GID are independent allowlist knobs; matching either configured identity is enough.
    allowed_uid.is_some_and(|uid| uid == peer_uid) || allowed_gid.is_some_and(|gid| gid == peer_gid)
}

fn chown_group(path: &Path, gid: u32) -> Result<(), String> {
    // `std::os::unix::fs::chown` is stable since 1.73 and wraps `chown(2)`
    // with `None` standing in for the libc "leave unchanged" sentinel.
    std::os::unix::fs::chown(path, None, Some(gid))
        .map_err(|err| format!("failed to chown {} to group {gid}: {err}", path.display()))
}

fn peer_pid_label(peer_pid: Option<u32>) -> String {
    peer_pid
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn panic_payload(payload: Box<dyn Any + Send + 'static>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(message) => (*message).to_string(),
            Err(_) => "unknown panic payload".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Error, ErrorKind};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use listenfd::ListenFd;
    use nix::unistd::{getegid, geteuid};
    use tokio::net::UnixStream;
    use tokio::time::timeout;

    use super::{accept_next_stream, listener_from_listenfd, peer_is_allowed};

    /// listenfd's `from_env` mutates `LISTEN_*` env vars, so every test that
    /// touches them must serialise. Mirrors the env_lock pattern in
    /// `config.rs` and `wire_contract.rs`.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn clear_listen_env() {
        std::env::remove_var("LISTEN_PID");
        std::env::remove_var("LISTEN_FDS");
        std::env::remove_var("LISTEN_FDNAMES");
        std::env::remove_var("LISTEN_FDS_FIRST_FD");
    }

    #[test]
    fn activation_listener_falls_back_when_no_descriptors() {
        // `ListenFd::empty()` simulates the "no socket activation" case
        // without touching env vars — that's the dev / `cargo run`
        // path that has to keep working.
        let listener = listener_from_listenfd(ListenFd::empty(), "/run/botwork/launcher.sock")
            .expect("empty descriptor list should fall back to self-bind");
        assert!(listener.is_none(), "empty fd set must signal self-bind");
    }

    #[test]
    fn activation_listener_rejects_wrong_fd_count() {
        // listenfd's `from_env` reads `LISTEN_FDS` verbatim and reports a
        // matching `len()`, regardless of whether the fds actually exist.
        // We can therefore exercise our "must be exactly one" check
        // without setting up real listener fds.
        let _guard = env_lock().lock().expect("env lock");
        clear_listen_env();
        std::env::set_var("LISTEN_PID", std::process::id().to_string());
        std::env::set_var("LISTEN_FDS", "2");

        let listenfd = ListenFd::from_env();
        let err = listener_from_listenfd(listenfd, "/run/botwork/launcher.sock")
            .expect_err("descriptor count should fail");
        assert_eq!(err, "socket activation expected exactly one socket, got 2");

        clear_listen_env();
    }

    #[tokio::test]
    async fn accept_error_continues_until_next_connection() {
        let (server_stream, _client_stream) = UnixStream::pair().expect("unix stream pair");
        let mut attempts = 0;
        let mut next_stream = Some(server_stream);

        let (accepted, peer_pid) = timeout(Duration::from_secs(1), async {
            accept_next_stream(
                &mut || {
                    attempts += 1;
                    let maybe_stream = if attempts == 2 {
                        next_stream.take()
                    } else {
                        None
                    };
                    async move {
                        match maybe_stream {
                            Some(stream) => Ok(stream),
                            None => Err(Error::new(ErrorKind::ConnectionAborted, "boom")),
                        }
                    }
                },
                Some(geteuid().as_raw()),
                Some(getegid().as_raw()),
            )
            .await
        })
        .await
        .expect("accept loop should continue after error");

        assert_eq!(attempts, 2);
        assert_eq!(peer_pid, Some(std::process::id()));
        drop(accepted);
    }

    #[test]
    fn peer_auth_allows_matching_uid_or_gid() {
        assert!(peer_is_allowed(1000, 2000, Some(1000), None));
        assert!(peer_is_allowed(1000, 2000, None, Some(2000)));
        assert!(peer_is_allowed(1000, 2000, Some(1234), Some(2000)));
        assert!(!peer_is_allowed(1000, 2000, Some(1234), Some(5678)));
        assert!(!peer_is_allowed(1000, 2000, None, None));
    }

    // ── peer_pid_label ──────────────────────────────────────────────────────

    #[test]
    fn peer_pid_label_some_prints_number() {
        use super::peer_pid_label;
        assert_eq!(peer_pid_label(Some(42)), "42");
        assert_eq!(peer_pid_label(Some(0)), "0");
        assert_eq!(peer_pid_label(Some(u32::MAX)), u32::MAX.to_string());
    }

    #[test]
    fn peer_pid_label_none_prints_unknown() {
        use super::peer_pid_label;
        assert_eq!(peer_pid_label(None), "unknown");
    }

    // ── panic_payload ───────────────────────────────────────────────────────

    #[test]
    fn panic_payload_from_string() {
        use super::panic_payload;
        let payload: Box<dyn std::any::Any + Send + 'static> = Box::new("oh no".to_string());
        assert_eq!(panic_payload(payload), "oh no");
    }

    #[test]
    fn panic_payload_from_static_str() {
        use super::panic_payload;
        let payload: Box<dyn std::any::Any + Send + 'static> = Box::new("static message");
        assert_eq!(panic_payload(payload), "static message");
    }

    #[test]
    fn panic_payload_unknown_type_returns_fallback() {
        use super::panic_payload;
        let payload: Box<dyn std::any::Any + Send + 'static> = Box::new(42u64);
        assert_eq!(panic_payload(payload), "unknown panic payload");
    }

    // ── PeerCredentials::describe ───────────────────────────────────────────

    #[test]
    fn peer_credentials_describe_with_pid() {
        use super::PeerCredentials;
        let creds = PeerCredentials {
            uid: 1000,
            gid: 2000,
            pid: Some(12345),
        };
        assert_eq!(creds.describe(), "uid=1000 gid=2000 pid=12345");
    }

    #[test]
    fn peer_credentials_describe_without_pid() {
        use super::PeerCredentials;
        let creds = PeerCredentials {
            uid: 0,
            gid: 0,
            pid: None,
        };
        assert_eq!(creds.describe(), "uid=0 gid=0 pid=unknown");
    }
}
