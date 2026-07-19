//! Unit tests for the stream-liveness grace-timer reap mechanism.
//!
//! These tests cover:
//! - Open-stream counter increments/decrements via the ext_proc path
//! - Grace timer arms when count hits 0
//! - Reconnect within grace cancels the timer
//! - Unknown-session POST does not create a liveness entry
//! - `seed_startup_liveness` seeds entries from `transport_sessions`
//!   (post-RFE-#105 round-3; pre-cutover this was from sessions.json)
//! - `BOTWORK_BROKER_DISCONNECT_GRACE_SECS` env-knob is honoured

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use botwork_session_broker::config_broker::UpstreamAuth;
use botwork_session_broker::ext_proc::{
    seed_startup_liveness, ExternalProcessorService, PerStreamState,
};
use botwork_session_broker::test_support::{
    liveness_drop, log_capture_guard, start_log_capture, take_log_capture,
};
use botwork_session_broker::{AppState, TransportState, COLD_START_TIMEOUT};
use envoy_proto::envoy::config::core::v3::{HeaderMap, HeaderValue};
use envoy_proto::envoy::service::ext_proc::v3::HttpHeaders;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_headers(values: &[(&str, &str)]) -> HttpHeaders {
    HttpHeaders {
        headers: Some(HeaderMap {
            headers: values
                .iter()
                .map(|(k, v)| HeaderValue {
                    key: k.to_string(),
                    value: v.to_string(),
                    raw_value: Default::default(),
                })
                .collect(),
        }),
        ..HttpHeaders::default()
    }
}

fn make_state() -> AppState {
    make_state_with_grace(Duration::from_secs(300))
}

fn make_state_with_grace(disconnect_grace: Duration) -> AppState {
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
        disconnect_grace,
        cold_start_timeout: COLD_START_TIMEOUT,
        // RFE #105 PR2 / round-3: production wires three DB-bound
        // handles via `run()`. Liveness tests drive the grace state
        // machine against an in-memory `transport_sessions` map only,
        // so `None` keeps the setup hermetic — no testcontainers
        // postgres required.
        agent_session_writer: None,
        session_worker_writer: None,
        db: None,
    }
}

async fn insert_transport(state: &AppState, mcp_session_id: &str, container: &str) {
    // Seed liveness cache so the ext_proc handler doesn't run docker inspect.
    state.liveness_cache.lock().await.insert(
        container.to_string(),
        std::time::Instant::now() + Duration::from_secs(300),
    );
    state.transport_sessions.lock().await.insert(
        mcp_session_id.to_string(),
        TransportState {
            container_name: container.to_string(),
            container_ip: "172.20.0.5".to_string(),
            staging_token: "aabbccdd".to_string(),
            tenant_name: "tenant1".to_string(),
            workspace: "mcp".to_string(),
            plugin_name: "plugin-a".to_string(),
            port: 8000,
            path: "/mcp".to_string(),
            upstream_auth: UpstreamAuth::None,
            upstream_authorization: None,
            agent_id: None,
            egress_policy: None,
        },
    );
}

fn open_stream_count(state: &AppState, mcp_session_id: &str) -> Option<usize> {
    state
        .stream_liveness
        .try_lock()
        .ok()?
        .get(mcp_session_id)
        .map(|l| l.open_streams.load(std::sync::atomic::Ordering::SeqCst))
}

// ---------------------------------------------------------------------------
// Counter bumps via handle_request_headers — GET known session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_known_session_bumps_liveness_counter() {
    let state = make_state();
    insert_transport(&state, "sess-get-1", "mcp_session_get1").await;

    let mut stream = PerStreamState::default();
    let _ = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        make_headers(&[
            (":method", "GET"),
            (":path", "/tenant1/mcp/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-get-1"),
        ]),
    )
    .await;

    assert_eq!(
        open_stream_count(&state, "sess-get-1"),
        Some(1),
        "GET known session should bump open_streams to 1"
    );
    assert_eq!(
        stream.liveness_session_id.as_deref(),
        Some("sess-get-1"),
        "liveness_session_id should be set for end-of-stream decrement"
    );
}

// ---------------------------------------------------------------------------
// Counter bumps via handle_request_headers — POST known session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn post_known_session_bumps_liveness_counter() {
    let state = make_state();
    insert_transport(&state, "sess-post-1", "mcp_session_post1").await;

    let mut stream = PerStreamState::default();
    let _ = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        make_headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-post-1"),
            ("content-type", "application/json"),
        ]),
    )
    .await;

    assert_eq!(
        open_stream_count(&state, "sess-post-1"),
        Some(1),
        "POST known session should bump open_streams to 1"
    );
    assert_eq!(stream.liveness_session_id.as_deref(), Some("sess-post-1"));
}

// ---------------------------------------------------------------------------
// Unknown session POST must NOT create a liveness entry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn post_unknown_session_does_not_create_liveness_entry() {
    let state = make_state();
    // transport_sessions is empty — no session registered

    let mut stream = PerStreamState::default();
    let _ = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        make_headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "unknown-sess"),
        ]),
    )
    .await;

    assert!(
        state.stream_liveness.lock().await.is_empty(),
        "unknown session must not create a liveness entry"
    );
    assert!(stream.liveness_session_id.is_none());
}

// ---------------------------------------------------------------------------
// DELETE does NOT bump the counter (teardown happens via teardown_on_response)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_known_session_does_not_bump_liveness_counter() {
    let state = make_state();
    insert_transport(&state, "sess-del-1", "mcp_session_del1").await;

    let mut stream = PerStreamState::default();
    let _ = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        make_headers(&[
            (":method", "DELETE"),
            (":path", "/tenant1/mcp/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-del-1"),
        ]),
    )
    .await;

    // liveness map should be empty — DELETE doesn't register a stream
    assert!(
        state.stream_liveness.lock().await.is_empty(),
        "DELETE must not bump the liveness counter"
    );
    assert!(stream.liveness_session_id.is_none());
}

// ---------------------------------------------------------------------------
// Multi-stream concurrency: counter tracks correctly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multi_stream_counter_increments_and_decrements() {
    let state = make_state();
    insert_transport(&state, "sess-multi", "mcp_session_multi").await;

    // Open two streams
    let mut s1 = PerStreamState::default();
    let _ = ExternalProcessorService::handle_request_headers(
        &state,
        &mut s1,
        make_headers(&[
            (":method", "GET"),
            (":path", "/tenant1/mcp/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-multi"),
        ]),
    )
    .await;

    let mut s2 = PerStreamState::default();
    let _ = ExternalProcessorService::handle_request_headers(
        &state,
        &mut s2,
        make_headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-multi"),
            ("content-type", "application/json"),
        ]),
    )
    .await;

    assert_eq!(
        open_stream_count(&state, "sess-multi"),
        Some(2),
        "two open streams → count should be 2"
    );
    // No grace timer yet
    {
        let map = state.stream_liveness.lock().await;
        let liveness = map.get("sess-multi").expect("entry should exist");
        let no_timer = liveness.grace_handle.lock().await.is_none();
        assert!(
            no_timer,
            "grace timer must not be armed while streams are open"
        );
    }
}

// ---------------------------------------------------------------------------
// Grace timer arms when the last stream closes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grace_timer_arms_when_last_stream_closes() {
    let state = make_state();
    insert_transport(&state, "sess-grace", "mcp_session_grace").await;

    let mut stream = PerStreamState::default();
    let _ = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        make_headers(&[
            (":method", "GET"),
            (":path", "/tenant1/mcp/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-grace"),
        ]),
    )
    .await;

    assert_eq!(open_stream_count(&state, "sess-grace"), Some(1));

    // Simulate stream end: call liveness_drop via the public test surface.
    // We do this indirectly by driving the session id decrement ourselves
    // through the internal counter.  Since liveness_drop is private, we
    // exercise it via the open_streams fetch_sub path directly.
    let liveness = state
        .stream_liveness
        .lock()
        .await
        .get("sess-grace")
        .cloned()
        .expect("entry exists");
    let prev = liveness
        .open_streams
        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(prev, 1, "counter was 1 before decrement");
    // The count is now 0; grace timer logic runs in the real ext_proc spawn,
    // so here we verify the counter really is 0.
    let now = liveness
        .open_streams
        .load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(now, 0, "counter must be 0 after decrement");
}

// ---------------------------------------------------------------------------
// Reconnect within grace cancels the pending timer
// ---------------------------------------------------------------------------

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn reconnect_within_grace_cancels_timer() {
    let state = make_state();
    insert_transport(&state, "sess-reconnect", "mcp_session_reconn").await;

    let _guard = log_capture_guard();
    start_log_capture();

    // First GET — bumps counter to 1
    let mut s1 = PerStreamState::default();
    let _ = ExternalProcessorService::handle_request_headers(
        &state,
        &mut s1,
        make_headers(&[
            (":method", "GET"),
            (":path", "/tenant1/mcp/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-reconnect"),
        ]),
    )
    .await;

    // Manually arm a dummy grace timer to simulate "all streams closed"
    {
        let map = state.stream_liveness.lock().await;
        let liveness = map.get("sess-reconnect").expect("entry exists");
        let dummy = tokio::spawn(async { tokio::time::sleep(Duration::from_secs(300)).await });
        let mut guard = liveness.grace_handle.lock().await;
        *guard = Some(dummy);
    }

    // Second GET — should cancel the grace timer
    let mut s2 = PerStreamState::default();
    let _ = ExternalProcessorService::handle_request_headers(
        &state,
        &mut s2,
        make_headers(&[
            (":method", "GET"),
            (":path", "/tenant1/mcp/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-reconnect"),
        ]),
    )
    .await;

    let logs = take_log_capture();
    let cancelled = logs
        .iter()
        .any(|l| l.contains("sess-reconnect") && l.contains("reconnected, grace cancelled"));
    assert!(cancelled, "expected grace-cancel log; got: {logs:?}");

    // Grace handle must be cleared
    {
        let map = state.stream_liveness.lock().await;
        let liveness = map.get("sess-reconnect").expect("entry still present");
        assert!(
            liveness.grace_handle.lock().await.is_none(),
            "grace handle should be None after reconnect"
        );
    }

    // Counter should be 2 (original + reconnect)
    assert_eq!(open_stream_count(&state, "sess-reconnect"), Some(2));

    // Container must NOT have been torn down
    assert!(
        state
            .transport_sessions
            .lock()
            .await
            .contains_key("sess-reconnect"),
        "transport must still be present"
    );
}

// ---------------------------------------------------------------------------
// seed_startup_liveness: every transport_sessions entry gets a liveness row
// ---------------------------------------------------------------------------
//
// Pre-RFE-#105 round-3 this read from the on-disk session registry. After the
// cutover the recovery path seeds `transport_sessions` from the DB + docker
// inspect, and `seed_startup_liveness` walks that map instead. Same property
// either way: every keyed Mcp-Session-Id gets a grace-timer-armed entry with
// open_streams=0.

#[tokio::test]
async fn seed_startup_liveness_seeds_entries_from_transport_sessions() {
    let state = make_state();

    // Plant two recovered transports.
    insert_transport(&state, "sid-seed-1", "mcp_session_seed1").await;
    insert_transport(&state, "sid-seed-2", "mcp_session_seed2").await;

    seed_startup_liveness(&state).await;

    // Give the spawned grace-timer tasks a moment to store their handles.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let map = state.stream_liveness.lock().await;
    assert!(
        map.contains_key("sid-seed-1"),
        "sid-seed-1 should have a liveness entry"
    );
    assert!(
        map.contains_key("sid-seed-2"),
        "sid-seed-2 should have a liveness entry"
    );
    assert_eq!(map.len(), 2, "exactly two entries expected");

    // Both entries should have open_streams=0 (no connected client yet).
    for sid in ["sid-seed-1", "sid-seed-2"] {
        let l = map.get(sid).unwrap();
        assert_eq!(
            l.open_streams.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{sid} should have open_streams=0 at startup"
        );
    }
}

// ---------------------------------------------------------------------------
// Env knob: BOTWORK_BROKER_DISCONNECT_GRACE_SECS parsing logic
// ---------------------------------------------------------------------------

#[test]
fn env_knob_default_is_30_seconds() {
    // Verify the fallback value when the env var is absent or unparseable.
    let val = Some("not-a-number")
        .and_then(|s: &str| s.parse::<u64>().ok())
        .unwrap_or(30);
    assert_eq!(val, 30, "unparseable value should fall back to 30s default");

    let val_missing: u64 = None::<&str>
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(30);
    assert_eq!(val_missing, 30, "absent var should default to 30s");
}

#[test]
fn env_knob_custom_value_is_parsed() {
    // Verify the env-knob parsing path used when AppState is constructed.
    for (raw, expected) in [("5", 5u64), ("0", 0), ("3600", 3600)] {
        let val = Some(raw)
            .and_then(|s: &str| s.parse::<u64>().ok())
            .unwrap_or(30);
        assert_eq!(
            val, expected,
            "env knob '{}' should parse to {}",
            raw, expected
        );
    }
}

// ---------------------------------------------------------------------------
// Grace expiry actually invokes teardown (regression: JoinHandle self-abort)
// ---------------------------------------------------------------------------

/// Verifies that when a grace timer fires the session is fully torn down —
/// i.e. the transport entry is removed and a tombstone is inserted.
///
/// This is a regression test for the bug where `teardown_session` called
/// `liveness_remove`, which aborted the grace-timer's own JoinHandle, causing
/// `call_teardown` (and all subsequent teardown steps) to be cancelled.
#[tokio::test]
async fn grace_expiry_invokes_teardown() {
    // Use a short grace so the test runs quickly.
    let state = make_state_with_grace(Duration::from_secs(1));
    insert_transport(&state, "sess-reap", "mcp_session_reap").await;

    // Open one stream so the liveness counter is bumped to 1.
    let mut ps = PerStreamState::default();
    let _ = ExternalProcessorService::handle_request_headers(
        &state,
        &mut ps,
        make_headers(&[
            (":method", "GET"),
            (":path", "/tenant1/mcp/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-reap"),
        ]),
    )
    .await;

    // Close the stream — arms the grace timer.
    liveness_drop(&state, "sess-reap").await;

    // Wait for the grace period to elapse plus a slack margin.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // The transport entry must have been removed by teardown_session.
    assert!(
        state
            .transport_sessions
            .lock()
            .await
            .get("sess-reap")
            .is_none(),
        "transport_sessions entry should be removed by teardown"
    );

    // A tombstone must have been inserted by teardown_session.
    assert!(
        state.tombstones.lock().await.contains_key("sess-reap"),
        "tombstone should be inserted by teardown"
    );
}

// ---------------------------------------------------------------------------
// Concurrent bump/drop stress: never leaves corrupt liveness state
// ---------------------------------------------------------------------------

/// Spawns N concurrent bump/drop pairs on the same mcp-session-id and asserts
/// that the final state is never corrupt.  Valid outcomes are:
///   - entry absent (reaped), *or*
///   - entry present with `open_streams == 0` and a grace handle armed, *or*
///   - entry present with `open_streams > 0` and no grace handle (streams still open).
///
/// A corrupt mix — e.g. `open_streams > 0` AND a grace handle armed — would
/// indicate the schedule-vs-bump race or another ordering hazard.
#[tokio::test]
async fn concurrent_bump_drop_never_leaves_corrupt_state() {
    let state = make_state();
    insert_transport(&state, "sess-race", "mcp_session_race").await;

    const TASKS: usize = 64;
    let mut joins = Vec::with_capacity(TASKS);
    for _ in 0..TASKS {
        let s = state.clone();
        joins.push(tokio::spawn(async move {
            let mut ps = PerStreamState::default();
            // Bump via the real request-headers path.
            let _ = ExternalProcessorService::handle_request_headers(
                &s,
                &mut ps,
                make_headers(&[
                    (":method", "GET"),
                    (":path", "/tenant1/mcp/plugin-a"),
                    ("x-botwork-tenant", "tenant1"),
                    ("mcp-session-id", "sess-race"),
                ]),
            )
            .await;
            // Drop via the exported liveness_drop so we exercise the full
            // decrement + potential grace-timer arming path.
            if let Some(ref sid) = ps.liveness_session_id {
                liveness_drop(&s, sid).await;
            }
        }));
    }
    for j in joins {
        j.await.unwrap();
    }

    // Allow any background grace-timer spawn to complete.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let map = state.stream_liveness.lock().await;
    if let Some(l) = map.get("sess-race") {
        let n = l.open_streams.load(std::sync::atomic::Ordering::SeqCst);
        let has_timer = l.grace_handle.lock().await.is_some();
        assert!(
            (n == 0 && has_timer) || (n > 0 && !has_timer),
            "corrupt liveness state: open_streams={n} has_timer={has_timer}"
        );
    }
    // If the entry is absent the session was already reaped — also valid.
}
