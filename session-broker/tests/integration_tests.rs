use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use axum::body::Body;
use botwork_session_broker::admin::build_router;
use botwork_session_broker::plugin_registry::{
    self, PluginRegistryError, PluginResources, UpstreamAuth,
};
use botwork_session_broker::session_registry::{utc_now, SessionRegistry};
use botwork_session_broker::AppState;
use http::Request;
use tempfile::tempdir;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Plugin registry tests
// ---------------------------------------------------------------------------

fn write_plugins(dir: &std::path::Path, content: &str) -> std::path::PathBuf {
    let path = dir.join("plugins.yaml");
    std::fs::write(&path, content).unwrap();
    path
}

#[test]
fn plugin_registry_valid_load() {
    let dir = tempdir().unwrap();
    let path = write_plugins(
        dir.path(),
        "plugins:\n  fs:\n    image: botwork/mcp-fs:local\n  echo:\n    image: botwork/mcp-echo:local\n    port: 9000\n    network: custom\n    path: /mcp\n",
    );

    let loaded = plugin_registry::load(path.to_str().unwrap()).unwrap();

    assert_eq!(loaded.len(), 2);

    let fs = loaded.get("fs").unwrap();
    assert_eq!(fs.image, "botwork/mcp-fs:local");
    assert_eq!(fs.port, 8000);
    assert_eq!(fs.network, "botwork");
    assert_eq!(fs.path, "/");
    assert_eq!(fs.upstream_auth, UpstreamAuth::None);
    assert_eq!(fs.resources, PluginResources::default());

    let echo = loaded.get("echo").unwrap();
    assert_eq!(echo.port, 9000);
    assert_eq!(echo.network, "custom");
    assert_eq!(echo.path, "/mcp");
    assert_eq!(echo.upstream_auth, UpstreamAuth::None);
    assert_eq!(echo.resources, PluginResources::default());
}

#[test]
fn plugin_registry_missing_file_raises() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("missing.yaml");
    let err = plugin_registry::load(path.to_str().unwrap()).unwrap_err();
    assert!(
        matches!(err, PluginRegistryError::NotFound(_)),
        "unexpected error: {err}"
    );
    assert!(err.to_string().contains("plugin registry file not found"));
}

#[test]
fn plugin_registry_empty_plugins_map_raises() {
    let dir = tempdir().unwrap();
    let path = write_plugins(dir.path(), "plugins: {}\n");
    let err = plugin_registry::load(path.to_str().unwrap()).unwrap_err();
    assert!(
        err.to_string()
            .contains("'plugins' must be a non-empty map"),
        "unexpected error: {err}"
    );
}

#[test]
fn plugin_registry_missing_image_raises() {
    let dir = tempdir().unwrap();
    let path = write_plugins(dir.path(), "plugins:\n  fs:\n    port: 9000\n");
    let err = plugin_registry::load(path.to_str().unwrap()).unwrap_err();
    assert!(
        err.to_string()
            .contains("missing required non-empty 'image'"),
        "unexpected error: {err}"
    );
}

#[test]
fn plugin_registry_bad_name_raises() {
    for bad_name in ["Fs", "a/b", &"a".repeat(32)] {
        let dir = tempdir().unwrap();
        let path = write_plugins(
            dir.path(),
            &format!("plugins:\n  {bad_name}:\n    image: botwork/x:local\n"),
        );
        let err = plugin_registry::load(path.to_str().unwrap()).unwrap_err();
        assert!(
            err.to_string().contains("invalid plugin name"),
            "unexpected error for name '{bad_name}': {err}"
        );
    }
}

#[test]
fn plugin_registry_bad_port_raises() {
    let dir = tempdir().unwrap();
    let path = write_plugins(
        dir.path(),
        "plugins:\n  fs:\n    image: botwork/mcp-fs:local\n    port: 99999\n",
    );
    let err = plugin_registry::load(path.to_str().unwrap()).unwrap_err();
    assert!(
        err.to_string().contains("invalid 'port'"),
        "unexpected error: {err}"
    );
}

#[test]
fn plugin_registry_bad_network_raises() {
    let dir = tempdir().unwrap();
    // empty string network
    let path = write_plugins(
        dir.path(),
        "plugins:\n  fs:\n    image: botwork/mcp-fs:local\n    network: ''\n",
    );
    let err = plugin_registry::load(path.to_str().unwrap()).unwrap_err();
    assert!(
        err.to_string().contains("invalid 'network'"),
        "unexpected error: {err}"
    );
}

// ---------------------------------------------------------------------------
// Session registry tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_registry_missing_file_gives_empty_start() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    // Path doesn't exist — load_and_reconcile should be a no-op
    let registry = SessionRegistry::new(path.to_str().unwrap());
    registry.load_and_reconcile().await;

    let data = registry.read().await;
    assert_eq!(data.version, 1);
    assert!(data.sessions.is_empty());
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
            reg.record_spawn(&container, &staging, "botwork/mcp-echo:local", &utc_now())
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
        plugin_registry: HashMap::new(),
        session_registry: registry,
        transport_sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_init: Arc::new(Mutex::new(HashMap::new())),
        launcher_socket_path: "/tmp/launcher.sock".to_string(),
        auth_broker_url: "http://127.0.0.1:1".to_string(),
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
