pub mod admin;
pub mod ext_proc;
pub mod launcher;
pub mod plugin_registry;
pub mod session_registry;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::session_registry::SessionRegistry;
use plugin_registry::PluginConfig;

pub const PREFIX: &str = "[session-broker]";
pub const SESSION_NETWORK: &str = "botwork";
pub const SESSION_PORT: u16 = 8000;
pub const COLD_START_TIMEOUT: Duration = Duration::from_secs(10);
pub const PROBE_SLEEP: Duration = Duration::from_millis(100);
pub const TENANT_RE: &str = r"^[a-z][a-z0-9-]{0,30}$";
pub const TENANT_PLUGIN_PATH_RE: &str = r"^/([a-z][a-z0-9-]{0,30})/([a-z][a-z0-9-]{0,30})(/.*)?$";

#[derive(Debug, Clone)]
pub struct TransportState {
    pub container_name: String,
    pub staging_token: String,
    pub tenant_name: String,
    pub plugin_name: String,
    pub port: u16,
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PendingInit {
    pub container_name: String,
    pub staging_token: String,
    pub tenant_name: String,
    pub plugin_name: String,
    pub plugin_config: PluginConfig,
    pub created_at: String,
}

#[derive(Clone)]
pub struct AppState {
    pub plugin_registry: plugin_registry::PluginRegistry,
    pub session_registry: Arc<SessionRegistry>,
    pub transport_sessions: Arc<Mutex<HashMap<String, TransportState>>>,
    pub pending_init: Arc<Mutex<HashMap<String, PendingInit>>>,
    pub launcher_socket_path: String,
}

pub fn log_info(message: &str) {
    println!("{PREFIX} {message}");
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
    session_registry.load_and_reconcile().await;

    let admin_addr = std::env::var("BOTWORK_SESSION_BROKER_ADMIN_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:9002".to_string());
    let grpc_addr = std::env::var("BOTWORK_SESSION_BROKER_GRPC_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:9001".to_string());
    let launcher_socket_path = std::env::var("BOTWORK_LAUNCHER_SOCKET_PATH")
        .unwrap_or_else(|_| "/run/botwork/launcher.sock".to_string());

    let state = AppState {
        plugin_registry: plugins,
        session_registry,
        transport_sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_init: Arc::new(Mutex::new(HashMap::new())),
        launcher_socket_path,
    };

    log_info(&format!("starting admin HTTP server on {admin_addr}"));
    log_info(&format!("starting gRPC ext_proc service on {grpc_addr}"));

    tokio::try_join!(
        admin::serve_admin(state.clone(), &admin_addr),
        ext_proc::serve_grpc(state, &grpc_addr),
    )?;

    Ok(())
}
