use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use botwork_session_broker::config_broker::UpstreamAuth;
use botwork_session_broker::exit_listener::handle_container_exit;
use botwork_session_broker::store::mock::MockSessionWorkerStore;
use botwork_session_broker::{AppState, TransportState};
use tokio::sync::Mutex;

fn make_state() -> AppState {
    make_state_with_worker(None)
}

fn make_state_with_worker(
    writer: Option<Arc<dyn botwork_session_broker::store::SessionWorkerStore>>,
) -> AppState {
    AppState {
        transport_sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_init: Arc::new(Mutex::new(HashMap::new())),
        launcher_socket_path: "/tmp/missing-launcher.sock".to_string(),
        auth_broker_url: "http://127.0.0.1:1".to_string(),
        config_broker_endpoint: "http://127.0.0.1:1".to_string(),
        control_plane_endpoint: "http://127.0.0.1:1".to_string(),
        tombstones: Arc::new(Mutex::new(HashMap::new())),
        liveness_cache: Arc::new(Mutex::new(HashMap::new())),
        stream_liveness: Arc::new(Mutex::new(HashMap::new())),
        disconnect_grace: Duration::from_secs(300),
        // RFE #105 PR2 / round-3: production wires three DB-bound
        // handles via `run()`. The container-exit path only touches
        // the in-memory `transport_sessions` map, so passing `None`
        // keeps the setup hermetic — no testcontainers postgres
        // required.
        agent_session_writer: None,
        session_worker_writer: writer,
        db: None,
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
    let state = make_state();
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
    let state = make_state();

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
    let state = make_state();
    insert_transport(
        &state,
        "sess-idem",
        sample_transport("mcp_session_112233445566"),
    )
    .await;

    let r1 = handle_container_exit(&state, "mcp_session_112233445566", "destroy", None)
        .await
        .unwrap();
    assert_eq!(r1.status(), 200);

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
    let state = make_state();
    insert_transport(
        &state,
        "sess-tomb",
        sample_transport("mcp_session_ffeeddccbbaa"),
    )
    .await;

    handle_container_exit(&state, "mcp_session_ffeeddccbbaa", "oom", None)
        .await
        .unwrap();

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
// handle_container_exit: spawned teardown task runs to completion
// ---------------------------------------------------------------------------
//
// Pre-RFE-#105 round-3 this test asserted that the session_registry row was
// removed by the spawned teardown task. After the cutover the registry is
// gone — the equivalent property is "the spawned task ran without panicking
// and the transport_sessions entry is gone by the time
// handle_container_exit returns". The first half is exercised implicitly by
// the lack of a panic propagating into this test; the second is the
// assertion below. Kept as a separate test (rather than folded into
// `container_exit_drops_transport_session`) so the sleep-and-recheck shape
// stays grep-able for future operators debugging an async-teardown
// regression.

#[tokio::test]
async fn container_exit_removes_routing_state() {
    let state = make_state();
    insert_transport(
        &state,
        "sess-reg-gone",
        sample_transport("mcp_session_cafebabe0000"),
    )
    .await;

    handle_container_exit(&state, "mcp_session_cafebabe0000", "die", Some(0))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        !state
            .transport_sessions
            .lock()
            .await
            .contains_key("sess-reg-gone"),
        "transport session row should be removed"
    );
}

#[tokio::test]
async fn container_exit_with_session_worker_writer_swallows_db_error() {
    let writer: Arc<dyn botwork_session_broker::store::SessionWorkerStore> =
        Arc::new(MockSessionWorkerStore::always_error("boom"));
    let state = make_state_with_worker(Some(writer));
    insert_transport(
        &state,
        "sess-db-warn",
        sample_transport("mcp_session_db_warn"),
    )
    .await;

    let response = handle_container_exit(&state, "mcp_session_db_warn", "die", Some(0))
        .await
        .expect("handle_container_exit should not fail");

    assert_eq!(response.status(), 200);
    assert!(!state
        .transport_sessions
        .lock()
        .await
        .contains_key("sess-db-warn"));
}

// ── serve_exit_listener end-to-end ──────────────────────────────────────────

/// Spin up `serve_exit_listener` on a temporary Unix socket, send a real
/// HTTP POST /container-exit, and verify the response status code.
/// This covers the `serve_exit_listener` accept loop and `handle_exit_request`
/// which can only be reached through a live Hyper connection.
#[tokio::test]
async fn serve_exit_listener_forwards_exit_event_returns_200() {
    use botwork_session_broker::exit_listener::serve_exit_listener;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    let tmp = tempfile::tempdir().expect("tempdir");
    let socket_path = tmp.path().join("broker.sock");
    let socket_path_str = socket_path.to_string_lossy().to_string();

    let state = make_state();
    // Pre-populate a transport so handle_container_exit returns 200 rather than 404.
    insert_transport(&state, "sess-e2e", sample_transport("mcp_session_e2e")).await;

    // Spawn the server.
    let server_task =
        tokio::spawn(async move { serve_exit_listener(state, &socket_path_str).await });

    // Wait briefly for the server to bind and enter its accept loop.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send a POST /container-exit request.
    let body = r#"{"name":"mcp_session_e2e","event":"die","exit_code":0}"#;
    let request = format!(
        "POST /container-exit HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let mut stream = UnixStream::connect(&socket_path).await.expect("connect");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");

    // Read the response status line.
    let mut response = vec![0u8; 1024];
    let n = stream.read(&mut response).await.expect("read response");
    let response_str = String::from_utf8_lossy(&response[..n]);
    assert!(
        response_str.starts_with("HTTP/1.1 200"),
        "expected HTTP 200, got: {response_str:?}"
    );

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn serve_exit_listener_returns_404_for_unknown_container() {
    use botwork_session_broker::exit_listener::serve_exit_listener;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    let tmp = tempfile::tempdir().expect("tempdir");
    let socket_path = tmp.path().join("broker2.sock");
    let socket_path_str = socket_path.to_string_lossy().to_string();

    let state = make_state(); // no transports seeded → any container name is unknown

    let server_task =
        tokio::spawn(async move { serve_exit_listener(state, &socket_path_str).await });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let body = r#"{"name":"mcp_session_unknown","event":"die"}"#;
    let request = format!(
        "POST /container-exit HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let mut stream = UnixStream::connect(&socket_path).await.expect("connect");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");

    let mut response = vec![0u8; 1024];
    let n = stream.read(&mut response).await.expect("read response");
    let response_str = String::from_utf8_lossy(&response[..n]);
    assert!(
        response_str.starts_with("HTTP/1.1 404"),
        "expected HTTP 404 for unknown container, got: {response_str:?}"
    );

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn serve_exit_listener_returns_500_for_malformed_json() {
    use botwork_session_broker::exit_listener::serve_exit_listener;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    let tmp = tempfile::tempdir().expect("tempdir");
    let socket_path = tmp.path().join("broker3.sock");
    let socket_path_str = socket_path.to_string_lossy().to_string();

    let state = make_state();

    let server_task =
        tokio::spawn(async move { serve_exit_listener(state, &socket_path_str).await });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let body = "not-json";
    let request = format!(
        "POST /container-exit HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let mut stream = UnixStream::connect(&socket_path).await.expect("connect");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");

    let mut response = vec![0u8; 1024];
    let n = stream.read(&mut response).await.expect("read response");
    let response_str = String::from_utf8_lossy(&response[..n]);
    // Malformed JSON causes dispatch_exit_request to return Err →
    // handle_exit_request returns 500.
    assert!(
        response_str.starts_with("HTTP/1.1 500"),
        "expected HTTP 500 for malformed JSON, got: {response_str:?}"
    );

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn serve_exit_listener_returns_404_for_non_post_method() {
    use botwork_session_broker::exit_listener::serve_exit_listener;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    let tmp = tempfile::tempdir().expect("tempdir");
    let socket_path = tmp.path().join("broker4.sock");
    let socket_path_str = socket_path.to_string_lossy().to_string();

    let state = make_state();

    let server_task =
        tokio::spawn(async move { serve_exit_listener(state, &socket_path_str).await });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send GET instead of POST.
    let request =
        "GET /container-exit HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let mut stream = UnixStream::connect(&socket_path).await.expect("connect");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");

    let mut response = vec![0u8; 1024];
    let n = stream.read(&mut response).await.expect("read response");
    let response_str = String::from_utf8_lossy(&response[..n]);
    assert!(
        response_str.starts_with("HTTP/1.1 404"),
        "expected HTTP 404 for GET method, got: {response_str:?}"
    );

    server_task.abort();
    let _ = server_task.await;
}
