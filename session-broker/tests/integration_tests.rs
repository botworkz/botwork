//! Integration tests for the session-broker admin HTTP surface.
//!
//! Pre-RFE-#105 round-3 this file also held a large suite of
//! `SessionRegistry`-lifecycle tests (record_spawn, record_mcp_session_id,
//! load_and_reconcile, schema-mismatch handling, atomic-write contention).
//! Those tests covered the on-disk JSON shape that the round-3 cutover
//! deleted, so they went with it. The admin-endpoint tests survive in
//! updated shape — `GET /sessions` now renders from
//! `transport_sessions` rather than the registry, but the wire-readable
//! property the tests pin (container-name-keyed map) is unchanged.
//!
//! `GET /control-plane/sessions` and its `SessionRecord`-wire-shape
//! tests were retired in the round-3 follow-up: control-plane no
//! longer polls session-broker for cold-start recovery. It reads
//! `session_worker` JOIN `agent_session` directly from postgres.
//! The wire-shape that the deleted tests pinned is now enforced on
//! the control-plane side (see `control-plane/src/recovery.rs`'s
//! `session_row_projects_into_session_record` test).

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
// GET /control-plane/sessions — RETIRED (RFE #105 round-3 follow-up).
//
// The endpoint and its admin handler are gone in this PR; control-plane
// reads `session_worker` JOIN `agent_session` from postgres directly.
// The two tests that lived here (round-trip and empty-set) covered the
// recovery-sync wire contract which is now enforced by:
//
//   * `control-plane/src/recovery.rs::tests::session_row_projects_into_session_record`
//     (projection contract — JOIN row → SessionRecord)
//   * `control-plane/tests/recovery_test.rs` (end-to-end JOIN run
//     against a testcontainers postgres seeded with realistic rows)
//
// Leaving this comment in place so a future bisect lands on the
// reason rather than `git log -p` archaeology.
// ---------------------------------------------------------------------------
