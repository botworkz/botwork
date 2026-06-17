pub mod admin;
pub mod exit_listener;
pub mod ext_proc;
pub mod launcher;
pub mod plugin_registry;
pub mod secrets;
pub mod session_registry;

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::session_registry::SessionRegistry;
use plugin_registry::PluginConfig;

pub const PREFIX: &str = "[session-broker]";
pub const SESSION_NETWORK: &str = "botwork";
pub const SESSION_PORT: u16 = 8000;
pub const COLD_START_TIMEOUT: Duration = Duration::from_secs(10);
pub const PROBE_SLEEP: Duration = Duration::from_millis(100);
pub const TENANT_RE: &str = r"^[a-z][a-z0-9-]{0,30}$";
pub const NAMESPACE_RE: &str = r"^[a-z][a-z0-9-]{0,30}$";
pub const TENANT_NAMESPACE_PLUGIN_PATH_RE: &str =
    r"^/([a-z][a-z0-9-]{0,30})/([a-z][a-z0-9-]{0,30})/([a-z][a-z0-9-]{0,30})(/.*)?$";

/// How long a tombstoned `Mcp-Session-Id` blocks new routing (5 minutes).
pub const TOMBSTONE_TTL: Duration = Duration::from_secs(300);

/// How long a container-liveness cache entry stays valid (5 minutes).
pub const LIVENESS_TTL: Duration = Duration::from_secs(300);

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
}

#[derive(Clone)]
pub struct TransportState {
    pub container_name: String,
    pub staging_token: String,
    pub tenant_name: String,
    pub namespace: String,
    pub plugin_name: String,
    pub port: u16,
    pub path: String,
    pub upstream_authorization: Option<String>,
    pub agent_id: Option<String>,
}

impl fmt::Debug for TransportState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransportState")
            .field("container_name", &self.container_name)
            .field("staging_token", &self.staging_token)
            .field("tenant_name", &self.tenant_name)
            .field("namespace", &self.namespace)
            .field("plugin_name", &self.plugin_name)
            .field("port", &self.port)
            .field("path", &self.path)
            .field(
                "upstream_authorization",
                &self.upstream_authorization.as_deref().map(redact_token),
            )
            .field("agent_id", &self.agent_id)
            .finish()
    }
}

#[derive(Clone)]
pub struct PendingInit {
    pub container_name: String,
    pub staging_token: String,
    pub tenant_name: String,
    pub namespace: String,
    pub plugin_name: String,
    pub plugin_config: PluginConfig,
    pub upstream_authorization: Option<String>,
    pub created_at: String,
}

impl fmt::Debug for PendingInit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingInit")
            .field("container_name", &self.container_name)
            .field("staging_token", &self.staging_token)
            .field("tenant_name", &self.tenant_name)
            .field("namespace", &self.namespace)
            .field("plugin_name", &self.plugin_name)
            .field("plugin_config", &self.plugin_config)
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
    pub plugin_registry: plugin_registry::PluginRegistry,
    pub session_registry: Arc<SessionRegistry>,
    pub transport_sessions: Arc<Mutex<HashMap<String, TransportState>>>,
    pub pending_init: Arc<Mutex<HashMap<String, PendingInit>>>,
    pub launcher_socket_path: String,
    pub auth_broker_url: String,
    /// Tombstoned `Mcp-Session-Id` values: maps session-id → expiry `Instant`.
    /// Requests referencing a tombstoned id receive an immediate 404 without a
    /// transport-session lookup, preventing re-spawn races on stale clients.
    pub tombstones: Arc<Mutex<HashMap<String, std::time::Instant>>>,
    /// Per-container liveness cache: maps container name → expiry `Instant`.
    /// An entry present and not yet expired means the container was confirmed
    /// running within the last `LIVENESS_TTL` and no docker inspect is needed.
    pub liveness_cache: Arc<Mutex<HashMap<String, std::time::Instant>>>,
    /// Per-session open-stream counter and grace-timer handle.
    /// Keyed by `Mcp-Session-Id`.  When the counter drops to zero after the
    /// last ext_proc stream closes, a grace timer is started; expiry triggers
    /// automatic session teardown.
    pub stream_liveness: Arc<Mutex<HashMap<String, Arc<SessionLiveness>>>>,
}

pub fn log_info(message: &str) {
    let formatted = format!("{PREFIX} {message}");
    println!("{formatted}");
    if let Some(entries) = test_support::log_capture()
        .lock()
        .expect("lock log capture")
        .as_mut()
    {
        entries.push(formatted);
    }
}

pub async fn run() -> Result<(), String> {
    let plugin_registry_path = std::env::var("BOTWORK_PLUGIN_REGISTRY_PATH")
        .unwrap_or_else(|_| "/etc/botwork/plugins.yaml".to_string());

    let plugins = plugin_registry::load(&plugin_registry_path).map_err(|e| format!("{e}"))?;

    log_info(&format!(
        "loaded plugin registry ({} plugins) from {}",
        plugins.len(),
        plugin_registry_path
    ));

    let session_registry_path = std::env::var("BOTWORK_SESSION_REGISTRY_PATH")
        .unwrap_or_else(|_| "/var/lib/botwork/sessions.json".to_string());

    let session_registry = Arc::new(SessionRegistry::new(&session_registry_path));
    // Refuse to start if the on-disk registry exists but cannot be safely
    // loaded — most commonly because it predates the namespace cutover.
    // Silently dropping such entries would orphan their containers (no
    // DELETE-routed teardown, no admin visibility), so this is intentional.
    // The error message tells the operator exactly what to clean up.
    session_registry
        .load_and_reconcile()
        .await
        .map_err(|e| format!("{e}"))?;

    let admin_addr = std::env::var("BOTWORK_SESSION_BROKER_ADMIN_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:9002".to_string());
    let grpc_addr = std::env::var("BOTWORK_SESSION_BROKER_GRPC_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:9001".to_string());
    let launcher_socket_path = std::env::var("BOTWORK_LAUNCHER_SOCKET_PATH")
        .unwrap_or_else(|_| "/run/botwork/launcher.sock".to_string());
    let auth_broker_url = std::env::var("BOTWORK_AUTH_BROKER_URL")
        .unwrap_or_else(|_| "http://auth_broker:9100".to_string());
    let broker_socket_path = std::env::var("BOTWORK_BROKER_SOCKET_PATH")
        .unwrap_or_else(|_| "/run/botwork/broker.sock".to_string());

    let state = AppState {
        plugin_registry: plugins,
        session_registry,
        transport_sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_init: Arc::new(Mutex::new(HashMap::new())),
        launcher_socket_path,
        auth_broker_url,
        tombstones: Arc::new(Mutex::new(HashMap::new())),
        liveness_cache: Arc::new(Mutex::new(HashMap::new())),
        stream_liveness: Arc::new(Mutex::new(HashMap::new())),
    };

    ext_proc::seed_startup_liveness(&state).await;

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
