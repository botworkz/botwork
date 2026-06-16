mod cmd;
pub mod config;
mod docker;
mod error;
pub mod events;
mod mount;
pub mod server;
pub mod validate;

use std::any::Any;
use std::ffi::CString;
use std::future::Future;
use std::mem::{size_of, MaybeUninit};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use libsystemd::activation::{self, FileDescriptor};
use tokio::net::{UnixListener, UnixStream};

use crate::cmd::{log_info, log_warn};
use crate::server::handle_request;

pub use config::Config;
pub use config::PREFIX;
pub use server::AppState;
pub use validate::Validators;

const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn run() -> Result<(), String> {
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
    let descriptors = activation::receive_descriptors(true)
        .map_err(|err| format!("failed to receive systemd socket activation descriptors: {err}"))?;
    listener_from_descriptors(descriptors, socket_path)
}

fn listener_from_descriptors(
    descriptors: Vec<FileDescriptor>,
    socket_path: &str,
) -> Result<Option<UnixListener>, String> {
    match descriptors.len() {
        0 => {
            log_info(&format!("self-bind: no LISTEN_FDS, binding {socket_path}"));
            Ok(None)
        }
        1 => {
            let descriptor = descriptors
                .into_iter()
                .next()
                .ok_or_else(|| "missing activated socket descriptor".to_string())?;
            let fd = descriptor.into_raw_fd();
            validate_activated_listener(fd)?;
            let listener = unix_listener_from_raw_fd(fd)?;
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

fn validate_activated_listener(fd: RawFd) -> Result<(), String> {
    let family = socket_family(fd)?;
    if family != libc::AF_UNIX {
        return Err(format!(
            "systemd socket activation fd {fd} must be AF_UNIX, got family={family}"
        ));
    }

    let socket_type = socket_option_int(fd, libc::SO_TYPE, "SO_TYPE")?;
    if socket_type != libc::SOCK_STREAM {
        return Err(format!(
            "systemd socket activation fd {fd} must be SOCK_STREAM, got type={socket_type}"
        ));
    }

    let accept_conn = socket_option_int(fd, libc::SO_ACCEPTCONN, "SO_ACCEPTCONN")?;
    if accept_conn == 0 {
        return Err(format!(
            "systemd socket activation fd {fd} is not a listening socket"
        ));
    }

    Ok(())
}

fn socket_family(fd: RawFd) -> Result<i32, String> {
    let mut address = MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut length = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockname(
            fd,
            address.as_mut_ptr().cast::<libc::sockaddr>(),
            &mut length,
        )
    };
    if rc != 0 {
        return Err(format!(
            "failed to inspect activated socket fd {fd} family: {}",
            std::io::Error::last_os_error()
        ));
    }

    let address = unsafe { address.assume_init() };
    Ok(address.ss_family as i32)
}

fn socket_option_int(fd: RawFd, option: libc::c_int, option_name: &str) -> Result<i32, String> {
    let mut value = MaybeUninit::<libc::c_int>::uninit();
    let mut length = size_of::<libc::c_int>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            value.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if rc != 0 {
        return Err(format!(
            "failed to inspect activated socket fd {fd} {option_name}: {}",
            std::io::Error::last_os_error()
        ));
    }

    Ok(unsafe { value.assume_init() })
}

fn unix_listener_from_raw_fd(fd: RawFd) -> Result<UnixListener, String> {
    let std_listener = unsafe { StdUnixListener::from_raw_fd(fd) };
    std_listener
        .set_nonblocking(true)
        .map_err(|err| format!("failed to set activated socket fd {fd} nonblocking: {err}"))?;
    UnixListener::from_std(std_listener)
        .map_err(|err| format!("failed to adopt activated socket fd {fd}: {err}"))
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
    let mut credentials = MaybeUninit::<libc::ucred>::zeroed();
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if rc != 0 || (length as usize) != size_of::<libc::ucred>() {
        return None;
    }

    let credentials = unsafe { credentials.assume_init() };
    Some(PeerCredentials {
        uid: credentials.uid,
        gid: credentials.gid,
        pid: (credentials.pid > 0).then_some(credentials.pid as u32),
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
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|err| format!("failed to prepare {} for chown: {err}", path.display()))?;
    let rc = unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) };
    if rc != 0 {
        return Err(format!(
            "failed to chown {} to group {gid}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
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
    use std::convert::TryFrom;
    use std::io::{Error, ErrorKind};
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::{UnixDatagram, UnixListener as StdUnixListener};
    use std::time::Duration;

    use libsystemd::activation::FileDescriptor;
    use tempfile::tempdir;
    use tokio::net::UnixStream;
    use tokio::time::timeout;

    use super::{accept_next_stream, listener_from_descriptors, peer_is_allowed};

    #[test]
    fn activation_listener_rejects_wrong_fd_count() {
        let temp = tempdir().expect("temp dir");
        let first = StdUnixListener::bind(temp.path().join("first.sock")).expect("first listener");
        let second =
            StdUnixListener::bind(temp.path().join("second.sock")).expect("second listener");
        let descriptors = vec![
            FileDescriptor::try_from(first.into_raw_fd()).expect("first descriptor"),
            FileDescriptor::try_from(second.into_raw_fd()).expect("second descriptor"),
        ];

        let err = listener_from_descriptors(descriptors, "/run/botwork/launcher.sock")
            .expect_err("descriptor count should fail");
        assert_eq!(err, "socket activation expected exactly one socket, got 2");
    }

    #[test]
    fn activation_listener_rejects_wrong_socket_type() {
        let (first, _second) = UnixDatagram::pair().expect("unix datagram pair");
        let descriptor =
            FileDescriptor::try_from(first.into_raw_fd()).expect("unix datagram descriptor");

        let err = listener_from_descriptors(vec![descriptor], "/run/botwork/launcher.sock")
            .expect_err("datagram socket should fail");
        assert!(
            err.contains("must be SOCK_STREAM"),
            "unexpected error: {err}"
        );
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
                Some(unsafe { libc::geteuid() }),
                Some(unsafe { libc::getegid() }),
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
}
