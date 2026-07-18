pub mod admin;
pub mod agent_session;
pub mod config_broker;
pub mod control_plane;
pub mod docker;
pub mod exit_listener;
pub mod ext_proc;
pub mod launcher;
pub mod recovery;
pub mod secrets;
pub mod session_worker;
pub mod sweeper;

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::info;

use crate::config_broker::{PluginDescriptor, UpstreamAuth};

pub const PREFIX: &str = "[session-broker]";
pub const VERSION: &str = include_str!("../../VERSION").trim_ascii();

pub fn version_string() -> String {
    botwork_version::format_full(VERSION, botwork_version::GIT_SHA)
}

pub const SESSION_PORT: u16 = 8000;
pub const COLD_START_TIMEOUT: Duration = Duration::from_secs(10);
pub const PROBE_SLEEP: Duration = Duration::from_millis(100);
pub const TENANT_RE: &str = r"^[a-z][a-z0-9-]{0,30}$";
pub const WORKSPACE_RE: &str = r"^[a-z][a-z0-9-]{0,30}$";
pub const TENANT_WORKSPACE_PLUGIN_PATH_RE: &str =
    r"^/([a-z][a-z0-9-]{0,30})/([a-z][a-z0-9-]{0,30})/([a-z][a-z0-9-]{0,30})(/.*)?$";

/// How long a tombstoned `Mcp-Session-Id` blocks new routing (5 minutes).
pub const TOMBSTONE_TTL: Duration = Duration::from_secs(300);

/// How long a container-liveness cache entry stays valid (5 minutes).
pub const LIVENESS_TTL: Duration = Duration::from_secs(300);

/// Default grace period before a fully-disconnected session is reaped.
/// Overridden at runtime by `BOTWORK_BROKER_DISCONNECT_GRACE_SECS`.
pub const DEFAULT_DISCONNECT_GRACE_SECS: u64 = 30;

/// Per-session stream-liveness tracking.
///
/// Counts how many ext_proc streams are currently open for this session id.
/// When the counter drops to zero a grace timer is armed; if no new stream
/// arrives within the grace period the session is reaped automatically.
pub struct SessionLiveness {
    pub open_streams: AtomicUsize,
    pub grace_handle: Mutex<Option<JoinHandle<()>>>,
}

impl Default for SessionLiveness {
    fn default() -> Self {
        Self {
            open_streams: AtomicUsize::new(0),
            grace_handle: Mutex::new(None),
        }
    }
}

pub fn redact_token(value: &str) -> String {
    let prefix: String = value.chars().take(6).collect();
    format!("{prefix}…")
}

/// Test-only helpers. Not part of the stable public API; required at module
/// scope (rather than `#[cfg(test)]`) because integration tests under `tests/`
/// compile against the crate's public surface and cannot see `cfg(test)` items.
#[doc(hidden)]
pub mod test_support {
    use std::sync::{Mutex as StdMutex, OnceLock};

    static LOG_CAPTURE: OnceLock<StdMutex<Option<Vec<String>>>> = OnceLock::new();

    pub(crate) fn log_capture() -> &'static StdMutex<Option<Vec<String>>> {
        LOG_CAPTURE.get_or_init(|| StdMutex::new(None))
    }

    pub fn start_log_capture() {
        *log_capture().lock().expect("lock log capture") = Some(Vec::new());
    }

    pub fn take_log_capture() -> Vec<String> {
        log_capture()
            .lock()
            .expect("lock log capture")
            .take()
            .unwrap_or_default()
    }

    /// Re-exported for integration tests so concurrent bump/drop tests can
    /// drive the full liveness state machine without going through the HTTP
    /// ext_proc path for the drop side.
    pub use crate::ext_proc::liveness_drop;
}

/// Per-session state held while a transport is alive.
///
/// `upstream_auth` is captured at spawn time (from the descriptor returned by
/// config-broker) so the routing-of-known-sessions path can decide whether to
/// project a `Bearer` header without going back to config-broker on every
/// request. `port` and `path` are similarly cached. The actual resolved
/// `Authorization` value (when any) lives in `upstream_authorization`.
#[derive(Clone)]
pub struct TransportState {
    pub container_name: String,
    /// IPv4 address the spawned plugin container holds on the plugin
    /// docker network. Captured at spawn time so the exit-listener path
    /// can build a SessionRecord-shaped DELETE without re-inspecting
    /// docker. Always populated (the launcher refuses to return 200
    /// without an IP since 0.1.5).
    pub container_ip: String,
    pub staging_token: String,
    pub tenant_name: String,
    pub workspace: String,
    pub plugin_name: String,
    pub port: u16,
    pub path: String,
    pub upstream_auth: UpstreamAuth,
    pub upstream_authorization: Option<String>,
    pub agent_id: Option<String>,
    /// Verbatim `egress:` block from the resolved PluginDescriptor.
    /// Cached on the transport so the admin `/sessions` endpoint can
    /// surface it for control-plane's cold-start recovery sync, and so
    /// future routing decisions can consult it without going back to
    /// config-broker.
    pub egress_policy: Option<serde_json::Value>,
}

impl fmt::Debug for TransportState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransportState")
            .field("container_name", &self.container_name)
            .field("container_ip", &self.container_ip)
            .field("staging_token", &self.staging_token)
            .field("tenant_name", &self.tenant_name)
            .field("workspace", &self.workspace)
            .field("plugin_name", &self.plugin_name)
            .field("port", &self.port)
            .field("path", &self.path)
            .field("upstream_auth", &self.upstream_auth)
            .field(
                "upstream_authorization",
                &self.upstream_authorization.as_deref().map(redact_token),
            )
            .field("agent_id", &self.agent_id)
            .field("egress_policy", &self.egress_policy)
            .finish()
    }
}

#[derive(Clone)]
pub struct PendingInit {
    pub container_name: String,
    /// IPv4 address on the plugin network, returned by the launcher's
    /// `/launch` response. Threaded through `PendingInit` so it's still
    /// in hand at `response_headers` time when we install the
    /// `TransportState`.
    pub container_ip: String,
    pub staging_token: String,
    pub tenant_name: String,
    pub workspace: String,
    pub plugin_name: String,
    pub descriptor: PluginDescriptor,
    pub upstream_authorization: Option<String>,
    pub created_at: String,
}

impl fmt::Debug for PendingInit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingInit")
            .field("container_name", &self.container_name)
            .field("container_ip", &self.container_ip)
            .field("staging_token", &self.staging_token)
            .field("tenant_name", &self.tenant_name)
            .field("workspace", &self.workspace)
            .field("plugin_name", &self.plugin_name)
            .field("descriptor", &self.descriptor)
            .field(
                "upstream_authorization",
                &self.upstream_authorization.as_deref().map(redact_token),
            )
            .field("created_at", &self.created_at)
            .finish()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub transport_sessions: Arc<Mutex<HashMap<String, TransportState>>>,
    pub pending_init: Arc<Mutex<HashMap<String, PendingInit>>>,
    pub launcher_socket_path: String,
    pub auth_broker_url: String,
    /// Base URL for `botwork-config-broker` (`POST /resolve` is appended).
    pub config_broker_endpoint: String,
    /// Base URL for `botwork-control-plane`. `POST /sessions` and
    /// `DELETE /sessions/<id>` are appended. Set to the plugin-network
    /// alias `http://control_plane:9300` in production; the test/unit
    /// path injects a `127.0.0.1:0` URL pointing at a fake server.
    /// See botwork #81 for the wire contract.
    pub control_plane_endpoint: String,
    /// Tombstoned `Mcp-Session-Id` values: maps session-id → expiry `Instant`.
    /// Requests referencing a tombstoned id receive an immediate 404 without a
    /// transport-session lookup, preventing re-spawn races on stale clients.
    ///
    /// Bounded by a background sweep task (see [`sweeper`]): the routing-time
    /// `is_tombstoned` purge is lazy and only fires on lookup of an expired
    /// id, which almost never happens for real traffic.
    pub tombstones: Arc<Mutex<HashMap<String, std::time::Instant>>>,
    /// Per-container liveness cache: maps container name → expiry `Instant`.
    /// An entry present and not yet expired means the container was confirmed
    /// running within the last `LIVENESS_TTL` and no docker inspect is needed.
    ///
    /// Bounded by a background sweep task (see [`sweeper`]): there is no
    /// lazy-on-access purge here, because container names are random per spawn
    /// and a torn-down container's entry is never looked up again.
    pub liveness_cache: Arc<Mutex<HashMap<String, std::time::Instant>>>,
    /// Per-session open-stream counter and grace-timer handle.
    /// Keyed by `Mcp-Session-Id`.  When the counter drops to zero after the
    /// last ext_proc stream closes, a grace timer is started; expiry triggers
    /// automatic session teardown.
    pub stream_liveness: Arc<Mutex<HashMap<String, Arc<SessionLiveness>>>>,
    /// Grace duration used when all streams for a session close. Read once at
    /// startup from `BOTWORK_BROKER_DISCONNECT_GRACE_SECS` and then reused for
    /// every timer arm.
    pub disconnect_grace: Duration,
    /// RFE #105 PR2: optional handle for the `agent_session`
    /// write-through path. `None` in tests that don't care about the
    /// DB; `Some(_)` in production where `run()` connects to
    /// postgres at startup. The writer is internally cheap to clone
    /// (everything behind `Arc`) so every clone of `AppState` carries
    /// the same handle.
    pub agent_session_writer: Option<Arc<crate::agent_session::AgentSessionWriter>>,
    /// RFE #105 round-3 PR2: write-through handle for the
    /// `session_worker` table — one row per spawned plugin container.
    /// Same `None`-in-tests convention as `agent_session_writer`.
    ///
    /// session-broker is the single writer of this table; reads happen
    /// from `recover_live_workers` at startup (paired with `docker ps`)
    /// and from the future janitor for sweep policy.
    pub session_worker_writer: Option<Arc<crate::session_worker::SessionWorkerWriter>>,
    /// RFE #105 round-3 PR2: shared `DatabaseConnection` so the broker
    /// can resolve cross-table reads outside of either writer's
    /// surface. Currently used only to resolve the
    /// `agent_session.id` PK after the per-name upsert lands in
    /// `AgentSessionWriter::record_bind_agent` — without that the
    /// `session_worker.agent_session_id` backfill would have to grow
    /// its own SELECT path.
    pub db: Option<Arc<sea_orm::DatabaseConnection>>,
}

pub fn log_info(message: &str) {
    let formatted = format!("{PREFIX} {message}");
    tracing::info!("{formatted}");
    if let Some(entries) = test_support::log_capture()
        .lock()
        .expect("lock log capture")
        .as_mut()
    {
        entries.push(formatted);
    }
}

pub async fn run() -> Result<(), String> {
    // tracing-subscriber: the existing log_info path keeps doing what it
    // does (println prefixed with [session-broker]). The new agent_session
    // write-through code uses tracing's `warn!`/`info!` macros so the
    // diagnostics show up alongside the existing log lines as long as a
    // subscriber is installed. Match the shape config-broker / control-
    // plane use so RUST_LOG works the same way across the workspace.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
    info!("{PREFIX} botwork-session-broker {}", version_string());

    // Round-3 cutover (RFE #105 PR2 followup): sessions.json is no
    // longer the source of truth, and after this PR there is no
    // `SessionRegistry` shape at all. If the file exists from a prior
    // installation, dump its contents to the journal once (so an
    // operator can audit it against `docker ps`) and unlink it. The
    // `recover_live_workers` path below rebuilds in-memory state
    // from the DB + docker labels.
    //
    // `BOTWORK_SESSION_REGISTRY_PATH` is still honoured as an
    // operator escape hatch — pointing at a non-default path lets
    // ops drive the one-time migration on an unusual layout — but
    // there is no longer a runtime read or write against the path
    // beyond this initial sweep.
    let legacy_registry_path = std::env::var("BOTWORK_SESSION_REGISTRY_PATH")
        .unwrap_or_else(|_| "/var/lib/botwork/sessions.json".to_string());
    crate::recovery::migrate_legacy_sessions_json(&legacy_registry_path);

    // RFE #105 PR2: connect to postgres and build the agent_session
    // write-through handle. Same env contract as config-broker /
    // bootstrap / api — `BOTWORK_DATABASE_URL` is required in
    // production and is rendered into the systemd unit by the
    // `EnvironmentFile=/var/lib/botwork-db/secret.env` line.
    //
    // We fail-fast on missing/invalid URL: session-broker is now a DB
    // consumer, so a misconfigured deploy must trip a unit-failure
    // restart loop rather than silently degrading to "JSON-only" mode.
    // The `Restart=always` posture on botwork-session-broker.service
    // picks this up.
    let db = match botwork_entity::connection::connect_from_env().await {
        Ok(db) => db,
        Err(botwork_entity::connection::ConnectError::MissingUrl) => {
            return Err(format!(
                "{} is not set; PR2 makes session-broker a DB consumer — \
                 add EnvironmentFile=/var/lib/botwork-db/secret.env to the systemd unit",
                botwork_entity::connection::DATABASE_URL_ENV
            ));
        }
        Err(botwork_entity::connection::ConnectError::Db(err)) => {
            return Err(format!("failed to connect to postgres: {err}"));
        }
    };
    let db_arc = Arc::new(db);
    let agent_session_writer = Some(Arc::new(crate::agent_session::AgentSessionWriter::new(
        Arc::clone(&db_arc),
    )));
    let session_worker_writer = Some(Arc::new(crate::session_worker::SessionWorkerWriter::new(
        Arc::clone(&db_arc),
    )));

    let admin_addr = std::env::var("BOTWORK_SESSION_BROKER_ADMIN_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:9002".to_string());
    let grpc_addr = std::env::var("BOTWORK_SESSION_BROKER_GRPC_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:9001".to_string());
    let launcher_socket_path = std::env::var("BOTWORK_LAUNCHER_SOCKET_PATH")
        .unwrap_or_else(|_| "/run/botwork/launcher.sock".to_string());
    let auth_broker_url = std::env::var("BOTWORK_AUTH_BROKER_URL")
        .unwrap_or_else(|_| "http://auth_broker:9100".to_string());
    let config_broker_endpoint = std::env::var("BOTWORK_CONFIG_BROKER_ENDPOINT")
        .unwrap_or_else(|_| "http://config_broker:9200".to_string());
    let control_plane_endpoint = std::env::var("BOTWORK_CONTROL_PLANE_ENDPOINT")
        .unwrap_or_else(|_| "http://control_plane:9300".to_string());
    let broker_socket_path = std::env::var("BOTWORK_BROKER_SOCKET_PATH")
        .unwrap_or_else(|_| "/run/botwork/broker.sock".to_string());
    let disconnect_grace_secs = std::env::var("BOTWORK_BROKER_DISCONNECT_GRACE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DISCONNECT_GRACE_SECS);
    let disconnect_grace = Duration::from_secs(disconnect_grace_secs);

    let state = AppState {
        transport_sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_init: Arc::new(Mutex::new(HashMap::new())),
        launcher_socket_path,
        auth_broker_url,
        config_broker_endpoint,
        control_plane_endpoint,
        tombstones: Arc::new(Mutex::new(HashMap::new())),
        liveness_cache: Arc::new(Mutex::new(HashMap::new())),
        stream_liveness: Arc::new(Mutex::new(HashMap::new())),
        disconnect_grace,
        agent_session_writer,
        session_worker_writer,
        db: Some(db_arc),
    };

    log_info(&format!(
        "config-broker endpoint: {}",
        state.config_broker_endpoint
    ));
    log_info(&format!(
        "control-plane endpoint: {}",
        state.control_plane_endpoint
    ));

    // Round-3 cutover: rebuild in-memory routing state from
    // session_worker rows + docker ps + docker inspect labels.
    // Replaces the JSON-based load_and_reconcile path. Orphan
    // containers (live in docker, no DB row) are reaped immediately
    // per the design call — "if it's not in DB it shouldn't be
    // running" — and audit rows with no live container get their
    // reaped_at column set.
    crate::recovery::recover_live_workers(&state).await;

    ext_proc::seed_startup_liveness(&state).await;

    // Background sweepers for the two expiry-keyed maps in AppState. Both
    // maps had only opportunistic per-request purging; without these tasks
    // they grow with historical session/container count for the lifetime of
    // the broker process. JoinHandles are intentionally dropped — the tokio
    // runtime aborts them on shutdown and they hold no external resources.
    let sweeper_interval = sweeper::sweeper_interval_from_env();
    log_info(&format!(
        "starting TTL sweepers (interval={}s)",
        sweeper_interval.as_secs()
    ));
    let _tombstone_sweeper = sweeper::spawn_ttl_sweeper(
        "tombstones",
        Arc::clone(&state.tombstones),
        sweeper_interval,
    );
    let _liveness_sweeper = sweeper::spawn_ttl_sweeper(
        "liveness_cache",
        Arc::clone(&state.liveness_cache),
        sweeper_interval,
    );

    log_info(&format!("starting admin HTTP server on {admin_addr}"));
    log_info(&format!("starting gRPC ext_proc service on {grpc_addr}"));
    log_info(&format!(
        "starting exit listener on unix://{broker_socket_path}"
    ));

    let exit_listener_state = state.clone();
    tokio::spawn(async move {
        if let Err(e) =
            exit_listener::serve_exit_listener(exit_listener_state, &broker_socket_path).await
        {
            log_info(&format!("exit listener error: {e}"));
        }
    });

    tokio::try_join!(
        admin::serve_admin(state.clone(), &admin_addr),
        ext_proc::serve_grpc(state, &grpc_addr),
    )?;

    Ok(())
}
