use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use axum::body::Body;
use botwork_session_broker::admin::build_router;
use botwork_session_broker::session_registry::{utc_now, RegistryLoadError, SessionRegistry};
use botwork_session_broker::AppState;
use http::Request;
use tempfile::tempdir;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Session registry tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_registry_missing_file_gives_empty_start() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    // Path doesn't exist — load_and_reconcile should be a no-op
    let registry = SessionRegistry::new(path.to_str().unwrap());
    registry.load_and_reconcile().await.expect("no file is ok");

    let data = registry.read().await;
    assert_eq!(data.version, 1);
    assert!(data.sessions.is_empty());
}

#[tokio::test]
async fn session_registry_load_skips_pre_workspace_entries_and_continues() {
    // RFE #105 / 2026-06-21 regression:
    //
    // A session-registry file written by a pre-workspace broker is
    // missing the tenant/workspace fields. The pre-PR2 broker
    // returned `Err(SchemaMismatch)` from `load_and_reconcile`, which
    // cascaded into a full broker exit-on-start on every box that
    // had been redeployed across the v0.3.0 namespace→workspace
    // rename. That cascade brought down control-plane and envoy.
    //
    // The new behaviour: skip malformed rows, WARN with the count +
    // container names, and let the rest of the file load. The good
    // row in this fixture must survive the load; the two bad rows
    // are dropped (operator can `docker rm -f` them after the fact).
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");

    let mixed_json = r#"{
        "version": 1,
        "updated_at": "2026-01-01T00:00:00Z",
        "sessions": {
            "mcp_session_old1": {
                "container": "mcp_session_old1",
                "staging_path": "/staging/old1",
                "image": "botwork/mcp-echo:local",
                "created_at": "2026-01-01T00:00:00Z",
                "mcp_session_id": null,
                "agent_id": null,
                "bound_at": null
            },
            "mcp_session_good": {
                "container": "mcp_session_good",
                "staging_path": "/staging/good",
                "tenant": "acme",
                "workspace": "mcp",
                "image": "botwork/mcp-echo:local",
                "created_at": "2026-01-01T00:00:00Z",
                "mcp_session_id": null,
                "agent_id": null,
                "bound_at": null
            },
            "mcp_session_old2": {
                "container": "mcp_session_old2",
                "staging_path": "/staging/old2",
                "image": "botwork/mcp-echo:local",
                "created_at": "2026-01-01T00:00:00Z",
                "mcp_session_id": null,
                "agent_id": null,
                "bound_at": null
            }
        }
    }"#;
    std::fs::write(&path, mixed_json).unwrap();

    let registry = SessionRegistry::new(path.to_str().unwrap());
    registry
        .load_and_reconcile()
        .await
        .expect("malformed rows must not fail the load (RFE #105 regression fix)");

    let data = registry.read().await;
    // The post-load disk write rewrites with only the surviving row.
    // (load_and_reconcile also retains-by-running-containers when docker is
    // reachable; in this test docker isn't reachable from cargo, so the
    // reconcile is a no-op and we keep the deserialised set.)
    assert_eq!(
        data.sessions.len(),
        1,
        "the one well-formed row must load; the two malformed must be skipped"
    );
    assert!(data.sessions.contains_key("mcp_session_good"));
    assert!(!data.sessions.contains_key("mcp_session_old1"));
    assert!(!data.sessions.contains_key("mcp_session_old2"));
}

#[tokio::test]
async fn session_registry_load_with_all_malformed_returns_empty() {
    // Boundary: every row is malformed. The loader must still succeed,
    // and `sessions` ends up empty. Distinct from the pre-PR2
    // behaviour which would have returned SchemaMismatch.
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");

    let all_bad = r#"{
        "version": 1,
        "updated_at": "2026-01-01T00:00:00Z",
        "sessions": {
            "mcp_session_old1": {
                "container": "mcp_session_old1",
                "staging_path": "/staging/old1",
                "image": "botwork/mcp-echo:local",
                "created_at": "2026-01-01T00:00:00Z"
            }
        }
    }"#;
    std::fs::write(&path, all_bad).unwrap();

    let registry = SessionRegistry::new(path.to_str().unwrap());
    registry
        .load_and_reconcile()
        .await
        .expect("all-malformed file must still load successfully");
    let data = registry.read().await;
    assert!(data.sessions.is_empty());
}

#[test]
fn schema_mismatch_display_is_preserved_for_future_strict_callers() {
    // `SchemaMismatch` is no longer returned by the production loader
    // (which now skips + WARNs per RFE #105). The variant survives
    // for a hypothetical future strict caller — if you remove it,
    // also remove this test. The check here just guarantees the
    // operator-facing message is stable for grep tooling.
    let err = RegistryLoadError::SchemaMismatch {
        offending: vec!["mcp_session_old1".into(), "mcp_session_old2".into()],
    };
    let display = err.to_string();
    assert!(display.contains("2 entries"), "message: {display}");
    assert!(display.contains("mcp_session_old1"), "message: {display}");
    assert!(display.contains("mcp_session_old2"), "message: {display}");
    assert!(display.contains("docker rm -f"), "message: {display}");
    assert!(
        display.contains("remove the registry file"),
        "message: {display}"
    );
}

#[tokio::test]
async fn session_registry_load_io_error_propagates() {
    // A non-JSON file at the registry path must produce Io, not SchemaMismatch.
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    std::fs::write(&path, "this is not json").unwrap();

    let registry = SessionRegistry::new(path.to_str().unwrap());
    let err = registry
        .load_and_reconcile()
        .await
        .expect_err("malformed JSON must fail");

    assert!(
        matches!(err, RegistryLoadError::Io(_)),
        "expected Io variant, got {err:?}"
    );
}

#[tokio::test]
async fn session_registry_record_spawn_and_read() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    let registry = SessionRegistry::new(path.to_str().unwrap());

    let now = utc_now();
    registry
        .record_spawn(
            "mcp_session_aabbccddeeff",
            "/var/lib/botwork/tenants/acme/staging/aabbccddeeff",
            "acme",
            "mcp",
            "botwork/mcp-echo:local",
            &now,
        )
        .await;

    let data = registry.read().await;
    assert_eq!(data.sessions.len(), 1);

    let entry = data.sessions.get("mcp_session_aabbccddeeff").unwrap();
    assert_eq!(entry.container, "mcp_session_aabbccddeeff");
    assert_eq!(
        entry.staging_path,
        "/var/lib/botwork/tenants/acme/staging/aabbccddeeff"
    );
    assert_eq!(entry.tenant, "acme");
    assert_eq!(entry.workspace, "mcp");
    assert_eq!(entry.image, "botwork/mcp-echo:local");
    assert_eq!(entry.created_at, now);
    assert!(entry.mcp_session_id.is_none());
    assert!(entry.agent_id.is_none());
    assert!(entry.bound_at.is_none());
}

#[tokio::test]
async fn session_registry_record_mcp_session_id_roundtrip() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    let registry = SessionRegistry::new(path.to_str().unwrap());

    registry
        .record_spawn(
            "mcp_session_aabbccddeeff",
            "/staging/aabbccddeeff",
            "acme",
            "mcp",
            "botwork/mcp-echo:local",
            &utc_now(),
        )
        .await;

    registry
        .record_mcp_session_id("mcp_session_aabbccddeeff", "sess-abc123")
        .await;

    let data = registry.read().await;
    let entry = data.sessions.get("mcp_session_aabbccddeeff").unwrap();
    assert_eq!(entry.mcp_session_id.as_deref(), Some("sess-abc123"));

    // Overwrite with same id — no change
    registry
        .record_mcp_session_id("mcp_session_aabbccddeeff", "sess-abc123")
        .await;
    let data2 = registry.read().await;
    assert_eq!(
        data2
            .sessions
            .get("mcp_session_aabbccddeeff")
            .unwrap()
            .mcp_session_id
            .as_deref(),
        Some("sess-abc123")
    );
}

#[tokio::test]
async fn session_registry_atomic_write_under_concurrent_mutation() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    let registry = Arc::new(SessionRegistry::new(path.to_str().unwrap()));

    let mut handles = Vec::new();
    for i in 0..10u64 {
        let reg = Arc::clone(&registry);
        handles.push(tokio::spawn(async move {
            let container = format!("mcp_session_{i:012x}");
            let staging = format!("/staging/{i:012x}");
            reg.record_spawn(
                &container,
                &staging,
                "acme",
                "mcp",
                "botwork/mcp-echo:local",
                &utc_now(),
            )
            .await;
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }

    // File must be valid JSON with all 10 sessions
    let content = std::fs::read_to_string(&path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&content).expect("valid JSON after writes");
    assert_eq!(
        value["sessions"].as_object().unwrap().len(),
        10,
        "expected 10 sessions, got: {value}"
    );
}

// ---------------------------------------------------------------------------
// Admin endpoint tests
// ---------------------------------------------------------------------------

fn app_state_for_registry(registry: Arc<SessionRegistry>) -> AppState {
    AppState {
        session_registry: registry,
        transport_sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_init: Arc::new(Mutex::new(HashMap::new())),
        launcher_socket_path: "/tmp/launcher.sock".to_string(),
        auth_broker_url: "http://127.0.0.1:1".to_string(),
        config_broker_endpoint: "http://127.0.0.1:1".to_string(),
        control_plane_endpoint: "http://127.0.0.1:1".to_string(),
        tombstones: Arc::new(Mutex::new(HashMap::new())),
        liveness_cache: Arc::new(Mutex::new(HashMap::new())),
        stream_liveness: Arc::new(Mutex::new(HashMap::new())),
        // RFE #105 PR2: admin-endpoint tests exercise the JSON
        // surface only. agent_session write-through is plumbed via
        // `run()` in production; tests pass `None` so the suite
        // doesn't need a testcontainers postgres.
        agent_session_writer: None,
    }
}

#[tokio::test]
async fn admin_get_sessions_returns_expected_json_shape() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    let registry = Arc::new(SessionRegistry::new(path.to_str().unwrap()));

    let app = build_router(app_state_for_registry(Arc::clone(&registry)));

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

    assert_eq!(json["version"], 1);
    assert!(
        json["updated_at"].is_string(),
        "updated_at must be a string"
    );
    // Verify timestamp format: YYYY-MM-DDTHH:MM:SSZ
    let ts = json["updated_at"].as_str().unwrap();
    assert!(
        ts.ends_with('Z') && ts.len() == 20,
        "updated_at '{ts}' must match YYYY-MM-DDTHH:MM:SSZ"
    );
    assert!(json["sessions"].is_object(), "sessions must be an object");
    assert!(
        json["sessions"].as_object().unwrap().is_empty(),
        "sessions should be empty for fresh registry"
    );
}

#[tokio::test]
async fn admin_get_sessions_includes_spawned_session() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    let registry = Arc::new(SessionRegistry::new(path.to_str().unwrap()));

    registry
        .record_spawn(
            "mcp_session_aabbccddeeff",
            "/var/lib/botwork/tenants/acme/staging/aabbccddeeff",
            "acme",
            "mcp",
            "botwork/mcp-echo:local",
            "2026-05-25T23:14:09Z",
        )
        .await;

    let app = build_router(app_state_for_registry(Arc::clone(&registry)));
    let request = Request::builder()
        .uri("/sessions")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let sessions = json["sessions"].as_object().unwrap();
    assert_eq!(sessions.len(), 1);

    let entry = &sessions["mcp_session_aabbccddeeff"];
    assert_eq!(entry["container"], "mcp_session_aabbccddeeff");
    assert_eq!(
        entry["staging_path"],
        "/var/lib/botwork/tenants/acme/staging/aabbccddeeff"
    );
    assert_eq!(entry["tenant"], "acme");
    assert_eq!(entry["workspace"], "mcp");
    assert_eq!(entry["image"], "botwork/mcp-echo:local");
    assert_eq!(entry["created_at"], "2026-05-25T23:14:09Z");
    // null fields must be present as null, not omitted
    assert!(
        entry["mcp_session_id"].is_null(),
        "mcp_session_id must be null"
    );
    assert!(entry["agent_id"].is_null(), "agent_id must be null");
    assert!(entry["bound_at"].is_null(), "bound_at must be null");
}

#[tokio::test]
async fn session_registry_record_teardown_removes_entry_and_persists() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    let registry = SessionRegistry::new(path.to_str().unwrap());

    registry
        .record_spawn(
            "mcp_session_aabbccddeeff",
            "/staging/aabbccddeeff",
            "acme",
            "mcp",
            "botwork/mcp-echo:local",
            &utc_now(),
        )
        .await;

    registry.record_teardown("mcp_session_aabbccddeeff").await;

    let data = registry.read().await;
    assert!(
        data.sessions.is_empty(),
        "sessions map should be empty after teardown"
    );

    // Verify the removal was persisted to disk
    let content = std::fs::read_to_string(&path).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&content).expect("valid JSON after teardown");
    assert!(
        value["sessions"].as_object().unwrap().is_empty(),
        "persisted sessions should be empty after teardown"
    );
}

#[tokio::test]
async fn session_registry_record_teardown_absent_container_is_noop() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    let registry = SessionRegistry::new(path.to_str().unwrap());

    // Record a different container so the file exists on disk
    registry
        .record_spawn(
            "mcp_session_other",
            "/staging/other",
            "acme",
            "mcp",
            "botwork/mcp-echo:local",
            &utc_now(),
        )
        .await;

    // Teardown a container that doesn't exist — should be a no-op
    registry.record_teardown("mcp_session_nonexistent").await;

    // The other entry must still be present
    let data = registry.read().await;
    assert_eq!(data.sessions.len(), 1);
    assert!(data.sessions.contains_key("mcp_session_other"));
}

#[tokio::test]
async fn admin_get_control_plane_sessions_returns_transport_sessions_in_record_shape() {
    // This is the recovery-sync surface: control-plane queries this on
    // cold start to seed its in-memory store. The shape must match
    // control-plane's `SessionRecord` exactly so the consumer doesn't
    // need a shim layer.
    use botwork_session_broker::config_broker::UpstreamAuth;
    use botwork_session_broker::TransportState;
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    let registry = Arc::new(SessionRegistry::new(path.to_str().unwrap()));

    let state = app_state_for_registry(Arc::clone(&registry));

    // Seed two transport entries with realistic shapes -- one with an
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
    // Absent egress is rendered as JSON null, not omitted -- so the
    // recovery-sync consumer can branch on shape uniformly.
    assert!(b["egress_policy"].is_null());
}

#[tokio::test]
async fn admin_get_control_plane_sessions_empty_when_no_transport_sessions() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    let registry = Arc::new(SessionRegistry::new(path.to_str().unwrap()));

    let app = build_router(app_state_for_registry(Arc::clone(&registry)));
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
