//! End-to-end wire-contract tests for `POST /resolve`.
//!
//! Spins up the full axum server bound to `127.0.0.1:0` against a tempdir
//! `plugins.yaml` so the tests exercise the same code path the real binary
//! does (request → JSON parse → registry lookup → response render).

use botwork_config_broker::{build_app_state, build_router};
use reqwest::StatusCode;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Owns the resources that must outlive the request: the tempdir holding
/// plugins.yaml and the server task. Both are dropped together when the test
/// function returns, ensuring no leaks across tests.
struct Server {
    base: String,
    _handle: JoinHandle<()>,
    _dir: TempDir,
}

async fn spawn_server(plugins_yaml: &str) -> Server {
    let dir = TempDir::new().expect("tempdir");
    let yaml_path = dir.path().join("plugins.yaml");
    std::fs::write(&yaml_path, plugins_yaml).expect("write plugins.yaml");

    let state = build_app_state(&yaml_path).expect("load registry");
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Server {
        base: format!("http://{addr}"),
        _handle: handle,
        _dir: dir,
    }
}

fn body(tenant: &str, namespace: &str, plugin: &str) -> String {
    serde_json::json!({
        "tenant": tenant,
        "namespace": namespace,
        "plugin": plugin,
    })
    .to_string()
}

const BASIC_YAML: &str = r#"
plugins:
  github:
    image: botwork/mcp-github:local
    upstream_auth: bearer/github.com
    env:
      GITHUB_TOOLSETS: default,actions
    config:
      routes:
        - owner: botworkz
          token_env: BOTWORK_SECRET_GITHUB_BOTWORKZ
  fs:
    image: botwork/mcp-fs:local
    path: /mcp
    resources:
      memory: 1g
      pids: 256
"#;

#[tokio::test]
async fn resolve_known_plugin_returns_full_descriptor() {
    let server = spawn_server(BASIC_YAML).await;
    let base = &server.base;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/resolve"))
        .header("content-type", "application/json")
        .body(body("phlax", "mcp", "github"))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["image"], "botwork/mcp-github:local");
    assert_eq!(body["port"], 8000);
    assert_eq!(body["network"], "botwork");
    assert_eq!(body["path"], "/");
    assert_eq!(body["upstream_auth"], "bearer/github.com");
    assert_eq!(body["env"][0]["name"], "GITHUB_TOOLSETS");
    assert_eq!(body["env"][0]["value"], "default,actions");
    let blob = body["config_blob"].as_str().expect("blob");
    assert_eq!(
        blob,
        r#"{"routes":[{"owner":"botworkz","token_env":"BOTWORK_SECRET_GITHUB_BOTWORKZ"}]}"#
    );
}

#[tokio::test]
async fn resolve_plugin_with_no_config_omits_config_blob() {
    let server = spawn_server(BASIC_YAML).await;
    let base = &server.base;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/resolve"))
        .header("content-type", "application/json")
        .body(body("phlax", "mcp", "fs"))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("json");
    assert!(
        body.get("config_blob").is_none(),
        "config_blob must be omitted when not set: {body}"
    );
    assert_eq!(body["path"], "/mcp");
    assert_eq!(body["resources"]["memory"], "1g");
    assert_eq!(body["resources"]["pids"], 256);
    assert!(body["resources"].get("cpus").is_none());
}

#[tokio::test]
async fn resolve_unknown_plugin_returns_404() {
    let server = spawn_server(BASIC_YAML).await;
    let base = &server.base;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/resolve"))
        .header("content-type", "application/json")
        .body(body("phlax", "mcp", "missing"))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"], "unknown_plugin");
}

#[tokio::test]
async fn resolve_invalid_namespace_returns_400_invalid_namespace() {
    let server = spawn_server(BASIC_YAML).await;
    let base = &server.base;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/resolve"))
        .header("content-type", "application/json")
        .body(body("phlax", "INVALID", "github"))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"], "invalid_namespace");
}

#[tokio::test]
async fn resolve_invalid_tenant_returns_400_invalid_request() {
    let server = spawn_server(BASIC_YAML).await;
    let base = &server.base;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/resolve"))
        .header("content-type", "application/json")
        .body(body("BAD-TENANT", "mcp", "github"))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"], "invalid_request");
}

#[tokio::test]
async fn resolve_missing_field_returns_400_invalid_request() {
    let server = spawn_server(BASIC_YAML).await;
    let base = &server.base;
    let client = reqwest::Client::new();
    let body = serde_json::json!({"tenant": "phlax", "namespace": "mcp"}).to_string();
    let response = client
        .post(format!("{base}/resolve"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"], "invalid_request");
    assert!(
        body["message"].as_str().unwrap_or("").contains("'plugin'"),
        "message should name missing field: {body}"
    );
}

#[tokio::test]
async fn resolve_missing_body_returns_400_invalid_request() {
    let server = spawn_server(BASIC_YAML).await;
    let base = &server.base;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/resolve"))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"], "invalid_request");
}

#[tokio::test]
async fn resolve_garbage_body_returns_400_invalid_request() {
    let server = spawn_server(BASIC_YAML).await;
    let base = &server.base;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/resolve"))
        .header("content-type", "application/json")
        .body("not-json".to_string())
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"], "invalid_request");
}

#[tokio::test]
async fn resolve_unknown_method_returns_405_or_404() {
    // axum surfaces unmatched routes as 405 (Method Not Allowed) for declared
    // path with wrong method; we just want to confirm GET on /resolve does not
    // accidentally succeed.
    let server = spawn_server(BASIC_YAML).await;
    let base = &server.base;
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{base}/resolve"))
        .send()
        .await
        .expect("send");

    assert!(
        matches!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_FOUND
        ),
        "GET /resolve should not succeed, got {}",
        response.status()
    );
}

#[tokio::test]
async fn resolve_other_path_returns_404() {
    let server = spawn_server(BASIC_YAML).await;
    let base = &server.base;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/secrets/fetch"))
        .header("content-type", "application/json")
        .body(body("phlax", "mcp", "github"))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
