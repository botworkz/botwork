use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use axum::extract::State;
use axum::response::Json;
use axum::routing::get;
use axum::Router;
use serde::Serialize;
use tokio::net::TcpListener;

use crate::session_registry::RegistryData;
use crate::AppState;

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/sessions", get(get_sessions))
        .route("/control-plane/sessions", get(get_control_plane_sessions))
        .with_state(state)
}

pub async fn serve_admin(state: AppState, addr: &str) -> Result<(), String> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| format!("failed to bind admin HTTP server on {addr}: {e}"))?;

    let app = build_router(state);

    axum::serve(listener, app)
        .await
        .map_err(|e| format!("admin HTTP server error: {e}"))
}

async fn get_sessions(State(state): State<AppState>) -> Json<RegistryData> {
    Json(state.session_registry.read().await)
}

/// `GET /control-plane/sessions` -- recovery-sync surface.
///
/// Returns one entry per live `transport_sessions` record, in
/// `SessionRecord`-wire-shape (botwork #81). Used by control-plane on
/// cold start to seed its in-memory store; only sessions that have
/// reached the `response_headers` phase (and therefore have an
/// `Mcp-Session-Id` populated) appear here. Pre-`response_headers`
/// `pending_init` records are deliberately excluded: control-plane only
/// cares about sessions that can actually be hit by an inbound request.
///
/// Sorted by `session_id` for stable output, matching control-plane's
/// own `GET /sessions` semantics so a recovery-sync consumer can
/// compare snapshots directly.
async fn get_control_plane_sessions(
    State(state): State<AppState>,
) -> Json<ControlPlaneSessionsBody> {
    let snapshot = state.transport_sessions.lock().await;
    let mut sessions: Vec<ControlPlaneSessionView> = snapshot
        .iter()
        .filter_map(|(session_id, transport)| {
            // Skip records with a bogus IP -- defensive: post-0.1.5
            // launcher refuses to return one, but the test path may
            // populate transport state directly.
            transport
                .container_ip
                .parse::<Ipv4Addr>()
                .ok()
                .map(|_| ControlPlaneSessionView {
                    session_id: session_id.clone(),
                    container_ip: transport.container_ip.clone(),
                    tenant: transport.tenant_name.clone(),
                    namespace: transport.namespace.clone(),
                    plugin: transport.plugin_name.clone(),
                    // Surface a JSON `null` (not omit) for parity with
                    // the wire body control-plane already accepts.
                    egress_policy: transport
                        .egress_policy
                        .clone()
                        .unwrap_or(serde_json::Value::Null),
                })
        })
        .collect();
    sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));

    // Re-collect into a BTreeMap-keyed Vec to keep the response order
    // stable across calls (HashMap snapshot order is not).
    let _ordering: BTreeMap<&str, ()> = sessions
        .iter()
        .map(|s| (s.session_id.as_str(), ()))
        .collect();

    Json(ControlPlaneSessionsBody { sessions })
}

#[derive(Debug, Serialize)]
struct ControlPlaneSessionsBody {
    sessions: Vec<ControlPlaneSessionView>,
}

#[derive(Debug, Serialize)]
struct ControlPlaneSessionView {
    session_id: String,
    container_ip: String,
    tenant: String,
    namespace: String,
    plugin: String,
    egress_policy: serde_json::Value,
}
