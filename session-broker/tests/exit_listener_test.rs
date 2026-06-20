use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use botwork_session_broker::config_broker::UpstreamAuth;
use botwork_session_broker::exit_listener::handle_container_exit;
use botwork_session_broker::session_registry::{utc_now, SessionRegistry};
use botwork_session_broker::{AppState, TransportState};
use tempfile::tempdir;
use tokio::sync::Mutex;

fn make_state(registry: Arc<SessionRegistry>) -> AppState {
    AppState {
        session_registry: registry,
        transport_sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_init: Arc::new(Mutex::new(HashMap::new())),
        launcher_socket_path: "/tmp/missing-launcher.sock".to_string(),
        auth_broker_url: "http://127.0.0.1:1".to_string(),
        config_broker_endpoint: "http://127.0.0.1:1".to_string(),
        control_plane_endpoint: "http://127.0.0.1:1".to_string(),
        tombstones: Arc::new(Mutex::new(HashMap::new())),
        liveness_cache: Arc::new(Mutex::new(HashMap::new())),
        stream_liveness: Arc::new(Mutex::new(HashMap::new())),
    }
}

fn sample_transport(container: &str) -> TransportState {
    TransportState {
        container_name: container.to_string(),
        container_ip: "172.20.0.5".to_string(),
        staging_token: "aabbccddeeff".to_string(),
        tenant_name: "acme".to_string(),
        workspace: "mcp".to_string(),
        plugin_name: "plugin-a".to_string(),
        port: 8000,
        path: "/mcp".to_string(),
        upstream_auth: UpstreamAuth::None,
        upstream_authorization: None,
        agent_id: None,
        egress_policy: None,
    }
}

async fn insert_transport(state: &AppState, mcp_session_id: &str, transport: TransportState) {
    state
        .transport_sessions
        .lock()
        .await
        .insert(mcp_session_id.to_string(), transport);
}

// ---------------------------------------------------------------------------
// handle_container_exit: drops transport_sessions entry for the container
// ---------------------------------------------------------------------------

#[tokio::test]
async fn container_exit_drops_transport_session() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    let registry = Arc::new(SessionRegistry::new(path.to_str().unwrap()));

    registry
        .record_spawn(
            "mcp_session_aabbccddeeff",
            "/staging/aabbccddeeff",
            "tenant1",
            "mcp",
            "botwork/mcp-echo:local",
            &utc_now(),
        )
        .await;
    registry
        .record_mcp_session_id("mcp_session_aabbccddeeff", "sess-abc123")
        .await;

    let state = make_state(Arc::clone(&registry));
    insert_transport(
        &state,
        "sess-abc123",
        sample_transport("mcp_session_aabbccddeeff"),
    )
    .await;

    let response = handle_container_exit(&state, "mcp_session_aabbccddeeff", "die", Some(137))
        .await
        .expect("handle_container_exit should not fail");

    assert_eq!(response.status(), 200);

    // Transport session must be gone
    assert!(
        !state
            .transport_sessions
            .lock()
            .await
            .contains_key("sess-abc123"),
        "transport session should be removed"
    );
}

// ---------------------------------------------------------------------------
// handle_container_exit: unknown container returns 404, not 500
// ---------------------------------------------------------------------------

#[tokio::test]
async fn container_exit_unknown_container_returns_404() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    let registry = Arc::new(SessionRegistry::new(path.to_str().unwrap()));
    let state = make_state(registry);

    let response = handle_container_exit(&state, "mcp_session_nonexistent", "die", Some(1))
        .await
        .expect("handle_container_exit should not fail");

    assert_eq!(response.status(), 404);
}

// ---------------------------------------------------------------------------
// handle_container_exit: idempotent — second call also returns 404
// ---------------------------------------------------------------------------

#[tokio::test]
async fn container_exit_idempotent() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    let registry = Arc::new(SessionRegistry::new(path.to_str().unwrap()));
    registry
        .record_spawn(
            "mcp_session_112233445566",
            "/staging/112233445566",
            "tenant1",
            "mcp",
            "botwork/mcp-echo:local",
            &utc_now(),
        )
        .await;
    registry
        .record_mcp_session_id("mcp_session_112233445566", "sess-idem")
        .await;

    let state = make_state(Arc::clone(&registry));
    insert_transport(
        &state,
        "sess-idem",
        sample_transport("mcp_session_112233445566"),
    )
    .await;

    // First call succeeds
    let r1 = handle_container_exit(&state, "mcp_session_112233445566", "destroy", None)
        .await
        .unwrap();
    assert_eq!(r1.status(), 200);

    // Second call is 404 (entry already removed)
    let r2 = handle_container_exit(&state, "mcp_session_112233445566", "destroy", None)
        .await
        .unwrap();
    assert_eq!(r2.status(), 404);
}

// ---------------------------------------------------------------------------
// handle_container_exit: tombstone is set after exit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn container_exit_tombstones_session() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    let registry = Arc::new(SessionRegistry::new(path.to_str().unwrap()));
    registry
        .record_spawn(
            "mcp_session_ffeeddccbbaa",
            "/staging/ffeeddccbbaa",
            "tenant1",
            "mcp",
            "botwork/mcp-echo:local",
            &utc_now(),
        )
        .await;
    registry
        .record_mcp_session_id("mcp_session_ffeeddccbbaa", "sess-tomb")
        .await;

    let state = make_state(Arc::clone(&registry));
    insert_transport(
        &state,
        "sess-tomb",
        sample_transport("mcp_session_ffeeddccbbaa"),
    )
    .await;

    handle_container_exit(&state, "mcp_session_ffeeddccbbaa", "oom", None)
        .await
        .unwrap();

    // Tombstone must be present and not yet expired
    let tombstones = state.tombstones.lock().await;
    let expires_at = tombstones.get("sess-tomb").copied();
    drop(tombstones);
    assert!(
        expires_at.is_some(),
        "tombstone should be set for sess-tomb"
    );
    assert!(
        expires_at.unwrap() > Instant::now(),
        "tombstone should not yet be expired"
    );
}

// ---------------------------------------------------------------------------
// handle_container_exit: session_registry row is removed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn container_exit_removes_registry_row() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    let registry = Arc::new(SessionRegistry::new(path.to_str().unwrap()));
    registry
        .record_spawn(
            "mcp_session_cafebabe0000",
            "/staging/cafebabe0000",
            "tenant1",
            "mcp",
            "botwork/mcp-echo:local",
            &utc_now(),
        )
        .await;
    registry
        .record_mcp_session_id("mcp_session_cafebabe0000", "sess-reg-gone")
        .await;

    let state = make_state(Arc::clone(&registry));
    insert_transport(
        &state,
        "sess-reg-gone",
        sample_transport("mcp_session_cafebabe0000"),
    )
    .await;

    handle_container_exit(&state, "mcp_session_cafebabe0000", "die", Some(0))
        .await
        .unwrap();

    // Give the spawned teardown task a chance to run
    tokio::time::sleep(Duration::from_millis(50)).await;

    let data = registry.read().await;
    assert!(
        !data.sessions.contains_key("mcp_session_cafebabe0000"),
        "session registry row should be removed"
    );
}
