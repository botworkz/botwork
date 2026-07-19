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
/// `GET /sessions` view (`botctl ps` reads it).
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
    /// Same shape `botctl ps` consumes (it walks the values
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppState, TransportState, UpstreamAuth};
    use axum::body::{to_bytes, Body};
    use http::Request;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;
    use tower::ServiceExt;

    fn bare_state() -> AppState {
        AppState {
            transport_sessions: Arc::new(Mutex::new(HashMap::new())),
            pending_init: Arc::new(Mutex::new(HashMap::new())),
            launcher_socket_path: "/tmp/admin-unit-launcher.sock".to_string(),
            auth_broker_url: "http://127.0.0.1:1".to_string(),
            config_broker_endpoint: "http://127.0.0.1:1".to_string(),
            control_plane_endpoint: "http://127.0.0.1:1".to_string(),
            tombstones: Arc::new(Mutex::new(HashMap::new())),
            liveness_cache: Arc::new(Mutex::new(HashMap::new())),
            stream_liveness: Arc::new(Mutex::new(HashMap::new())),
            disconnect_grace: Duration::from_secs(300),
            cold_start_timeout: crate::COLD_START_TIMEOUT,
            agent_session_writer: None,
            session_worker_writer: None,
            db: None,
        }
    }

    fn sample_transport(container_name: &str, tenant: &str) -> TransportState {
        TransportState {
            container_name: container_name.to_string(),
            container_ip: "10.0.0.1".to_string(),
            tenant_name: tenant.to_string(),
            workspace: "mcp".to_string(),
            plugin_name: "mcp-bash".to_string(),
            staging_token: "tok-abc".to_string(),
            port: 8000,
            path: "/mcp".to_string(),
            upstream_auth: UpstreamAuth::None,
            upstream_authorization: None,
            agent_id: Some("agent-1".to_string()),
            egress_policy: None,
        }
    }

    #[tokio::test]
    async fn get_sessions_returns_container_keyed_view() {
        let state = bare_state();
        state.transport_sessions.lock().await.insert(
            "sess-2".to_string(),
            sample_transport("mcp-session-b", "tenant-b"),
        );
        state.transport_sessions.lock().await.insert(
            "sess-1".to_string(),
            sample_transport("mcp-session-a", "tenant-a"),
        );

        let Json(view) = get_sessions(State(state)).await;
        let keys: Vec<String> = view.sessions.keys().cloned().collect();
        assert_eq!(
            keys,
            vec!["mcp-session-a".to_string(), "mcp-session-b".to_string()]
        );
        assert_eq!(view.sessions["mcp-session-a"].tenant, "tenant-a");
        assert_eq!(view.sessions["mcp-session-b"].plugin, "mcp-bash");
    }

    #[tokio::test]
    async fn evict_tenant_returns_ok_when_no_sessions() {
        let state = bare_state();
        let response = evict_tenant(State(state), Path("missing".to_string()))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn build_router_serves_sessions_and_evict_routes() {
        let state = bare_state();
        state.transport_sessions.lock().await.insert(
            "sess-1".to_string(),
            sample_transport("mcp-session-a", "tenant-a"),
        );
        let app = build_router(state);

        let sessions_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/sessions")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("sessions response");
        assert_eq!(sessions_response.status(), StatusCode::OK);
        let bytes = to_bytes(sessions_response.into_body(), usize::MAX)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(json["sessions"]["mcp-session-a"]["tenant"], "tenant-a");

        let evict_response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/evict-tenant/tenant-a")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("evict response");
        assert_eq!(evict_response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn serve_admin_rejects_invalid_bind_address() {
        let err = serve_admin(bare_state(), "definitely not a socket address")
            .await
            .expect_err("invalid address should fail");
        assert!(err.contains("failed to bind admin HTTP server"), "{err}");
    }
}
