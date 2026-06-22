//! Integration tests for the session-broker admin HTTP surface.
//!
//! Pre-RFE-#105 round-3 this file also held a large suite of
//! `SessionRegistry`-lifecycle tests (record_spawn, record_mcp_session_id,
//! load_and_reconcile, schema-mismatch handling, atomic-write contention).
//! Those tests covered the on-disk JSON shape that the round-3 cutover
//! deleted, so they go with it. The admin-endpoint tests survive in
//! updated shape — `GET /sessions` now renders from
//! `transport_sessions` rather than the registry, but the wire-readable
//! property the tests pin (container-name-keyed map) is unchanged.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use axum::body::Body;
use botwork_session_broker::admin::build_router;
use botwork_session_broker::config_broker::UpstreamAuth;
use botwork_session_broker::{AppState, TransportState};
use http::Request;
use tower::ServiceExt;

fn app_state_for_admin_tests() -> AppState {
    AppState {
        transport_sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_init: Arc::new(Mutex::new(HashMap::new())),
        launcher_socket_path: "/tmp/launcher.sock".to_string(),
        auth_broker_url: "http://127.0.0.1:1".to_string(),
        config_broker_endpoint: "http://127.0.0.1:1".to_string(),
        control_plane_endpoint: "http://127.0.0.1:1".to_string(),
        tombstones: Arc::new(Mutex::new(HashMap::new())),
        liveness_cache: Arc::new(Mutex::new(HashMap::new())),
        stream_liveness: Arc::new(Mutex::new(HashMap::new())),
        // RFE #105 PR2 / round-3: admin-endpoint tests render from
        // `transport_sessions` only. Production wires three DB-bound
        // handles via `run()`; tests pass `None` to stay hermetic
        // (no testcontainers postgres required for the JSON wire
        // tests).
        agent_session_writer: None,
        session_worker_writer: None,
        db: None,
    }
}

// ---------------------------------------------------------------------------
// GET /sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_get_sessions_returns_expected_json_shape() {
    let state = app_state_for_admin_tests();
    let app = build_router(state);

    let request = Request::builder()
        .uri("/sessions")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // RFE #105 round-3 cleanup: the JSON shape is `{"sessions": {…}}`
    // (no `version` / `updated_at` wrapper any more — those belonged
    // to the on-disk registry surface that's gone). Sessions are
    // keyed by container name, same as the operator-facing shape
    // `botwork-tools ps` consumes.
    assert!(json["sessions"].is_object(), "sessions must be an object");
    assert!(
        json["sessions"].as_object().unwrap().is_empty(),
        "sessions should be empty when no transports are registered"
    );
}

#[tokio::test]
async fn admin_get_sessions_includes_spawned_session() {
    let state = app_state_for_admin_tests();

    // Plant a transport directly — same shape `recover_live_workers`
    // produces during cold-start rehydration, and what the spawn
    // path leaves behind after the initialize response lands.
    {
        let mut sessions = state.transport_sessions.lock().await;
        sessions.insert(
            "sess-abc123".to_string(),
            TransportState {
                container_name: "mcp_session_aabbccddeeff".to_string(),
                container_ip: "172.20.0.5".to_string(),
                staging_token: "aabbccddeeff".to_string(),
                tenant_name: "acme".to_string(),
                workspace: "mcp".to_string(),
                plugin_name: "echo".to_string(),
                port: 8000,
                path: "/mcp".to_string(),
                upstream_auth: UpstreamAuth::None,
                upstream_authorization: None,
                agent_id: None,
                egress_policy: None,
            },
        );
    }

    let app = build_router(state);
    let request = Request::builder()
        .uri("/sessions")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let sessions = json["sessions"].as_object().unwrap();
    assert_eq!(sessions.len(), 1);

    let entry = &sessions["mcp_session_aabbccddeeff"];
    assert_eq!(entry["container"], "mcp_session_aabbccddeeff");
    assert_eq!(entry["container_ip"], "172.20.0.5");
    assert_eq!(entry["tenant"], "acme");
    assert_eq!(entry["workspace"], "mcp");
    assert_eq!(entry["plugin"], "echo");
    // agent_id is rendered as null pre-bind (the spawn path sets
    // it only after the first non-init JSON-RPC call surfaces the
    // goose agent-session-id).
    assert!(
        entry["agent_id"].is_null(),
        "agent_id must be null pre-bind"
    );
}

// ---------------------------------------------------------------------------
// GET /control-plane/sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_get_control_plane_sessions_returns_transport_sessions_in_record_shape() {
    // This is the recovery-sync surface: control-plane queries this on
    // cold start to seed its in-memory store. The shape must match
    // control-plane's `SessionRecord` exactly so the consumer doesn't
    // need a shim layer.
    let state = app_state_for_admin_tests();

    // Seed two transport entries with realistic shapes — one with an
    // explicit egress policy, one without (recovery-sync needs both
    // cases to round-trip correctly).
    {
        let mut sessions = state.transport_sessions.lock().await;
        sessions.insert(
            "mcp_session_b".to_string(),
            TransportState {
                container_name: "mcp_session_b".to_string(),
                container_ip: "172.20.0.6".to_string(),
                staging_token: "tokenb".to_string(),
                tenant_name: "phlax".to_string(),
                workspace: "mcp".to_string(),
                plugin_name: "fetch".to_string(),
                port: 8000,
                path: "/mcp".to_string(),
                upstream_auth: UpstreamAuth::None,
                upstream_authorization: None,
                agent_id: None,
                egress_policy: None,
            },
        );
        sessions.insert(
            "mcp_session_a".to_string(),
            TransportState {
                container_name: "mcp_session_a".to_string(),
                container_ip: "172.20.0.5".to_string(),
                staging_token: "tokena".to_string(),
                tenant_name: "phlax".to_string(),
                workspace: "mcp".to_string(),
                plugin_name: "github".to_string(),
                port: 8000,
                path: "/mcp".to_string(),
                upstream_auth: UpstreamAuth::None,
                upstream_authorization: None,
                agent_id: None,
                egress_policy: Some(serde_json::json!({
                    "allow": [{"host": "api.github.com", "ports": [443]}]
                })),
            },
        );
    }

    let app = build_router(state);
    let request = Request::builder()
        .uri("/control-plane/sessions")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let sessions = json["sessions"].as_array().expect("sessions array");
    // Sorted by session_id; "a" before "b".
    let ids: Vec<&str> = sessions
        .iter()
        .map(|s| s["session_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["mcp_session_a", "mcp_session_b"]);

    // Wire shape must match control-plane's SessionRecord: session_id,
    // container_ip, tenant, workspace, plugin, egress_policy. Nothing
    // else (no agent_id, no upstream_auth, no transport plumbing).
    let a = &sessions[0];
    assert_eq!(a["session_id"], "mcp_session_a");
    assert_eq!(a["container_ip"], "172.20.0.5");
    assert_eq!(a["tenant"], "phlax");
    assert_eq!(a["workspace"], "mcp");
    assert_eq!(a["plugin"], "github");
    assert_eq!(a["egress_policy"]["allow"][0]["host"], "api.github.com");

    let b = &sessions[1];
    assert_eq!(b["session_id"], "mcp_session_b");
    assert_eq!(b["plugin"], "fetch");
    // Absent egress is rendered as JSON null, not omitted — so the
    // recovery-sync consumer can branch on shape uniformly.
    assert!(b["egress_policy"].is_null());
}

#[tokio::test]
async fn admin_get_control_plane_sessions_empty_when_no_transport_sessions() {
    let state = app_state_for_admin_tests();
    let app = build_router(state);
    let request = Request::builder()
        .uri("/control-plane/sessions")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["sessions"].as_array().unwrap().is_empty(),
        "sessions must be empty when no transports are registered: {json}"
    );
}
