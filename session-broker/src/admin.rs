use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use serde::Serialize;
use tokio::net::TcpListener;

use crate::AppState;

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/sessions", get(get_sessions))
        .route("/evict-tenant/:tenant", post(evict_tenant))
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

/// `GET /sessions` — operator-visible view of the live in-memory
/// routing state.
///
/// Pre-RFE-#105-round-3, this served a snapshot of the on-disk
/// `sessions.json` file (which had its own container-name-keyed
/// shape). After the round-3 cutover the JSON is gone and routing
/// state lives in `state.transport_sessions` keyed by
/// `Mcp-Session-Id`. The endpoint preserves the operator-readable
/// shape — container name → entry — by walking the in-memory map
/// at render time.
///
/// `GET /control-plane/sessions` was retired in the round-3
/// follow-up: control-plane no longer polls session-broker for cold-
/// start recovery and instead reads `session_worker` JOIN
/// `agent_session` directly from postgres. Anything that needs to
/// observe in-memory state from outside the broker still uses this
/// `GET /sessions` view (`botwork-tools ps` reads it).
async fn get_sessions(State(state): State<AppState>) -> Json<SessionsView> {
    let snapshot = state.transport_sessions.lock().await;
    let mut sessions: BTreeMap<String, SessionView> = BTreeMap::new();
    for transport in snapshot.values() {
        sessions.insert(
            transport.container_name.clone(),
            SessionView {
                container: transport.container_name.clone(),
                container_ip: transport.container_ip.clone(),
                tenant: transport.tenant_name.clone(),
                workspace: transport.workspace.clone(),
                plugin: transport.plugin_name.clone(),
                agent_id: transport.agent_id.clone(),
            },
        );
    }
    Json(SessionsView { sessions })
}

#[derive(Debug, Serialize)]
struct SessionsView {
    /// Container-name-keyed view of every live transport session.
    /// Same shape `botwork-tools ps` consumes (it walks the values
    /// for the operator-visible table).
    sessions: BTreeMap<String, SessionView>,
}

#[derive(Debug, Serialize)]
struct SessionView {
    container: String,
    container_ip: String,
    tenant: String,
    workspace: String,
    plugin: String,
    /// Only populated once the first non-init JSON-RPC call has
    /// surfaced the goose agent-session-id (see ext_proc.rs).
    agent_id: Option<String>,
}

/// `POST /evict-tenant/{tenant}` — evict all live sessions for a tenant.
///
/// Called by api after a successful secret mutation (create, overwrite, or
/// delete) for the tenant so that the next request re-enters the spawn path
/// and re-fetches secrets with the updated credentials.
///
/// Sync: removes the session from the routing table and tombstones its
/// `Mcp-Session-Id`; subsequent requests with a stale id receive an
/// immediate 404, causing well-behaved MCP clients to re-initialize.
///
/// Async: spawns a background task per session to call the launcher
/// teardown helper and update the DB audit trail.
///
/// Returns `200 { "evicted": N }` regardless of whether any sessions were
/// found. A `0` result is normal (tenant has no live sessions).
async fn evict_tenant(
    State(state): State<AppState>,
    Path(tenant): Path<String>,
) -> impl IntoResponse {
    let evicted = crate::ext_proc::evict_sessions_for_tenant(&state, &tenant).await;
    (
        StatusCode::OK,
        Json(serde_json::json!({ "evicted": evicted })),
    )
}
