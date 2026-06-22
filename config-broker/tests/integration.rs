//! End-to-end wire-contract tests for `POST /resolve` (post-PR2).
//!
//! Spins up a real postgres via testcontainers, runs the schema
//! migrations, applies a bootstrap config, then spins the axum
//! server against the DB and exercises `/resolve` end-to-end.
//!
//! Gated on docker the same way the bootstrap/migration smokes are:
//! a clearly-labelled `IGNORED` line when docker isn't reachable
//! keeps `cargo test` green on dev machines without docker.

use std::sync::Arc;
use std::time::Duration;

use botwork_bootstrap::{apply, BootstrapConfig, BootstrapConfigRaw};
use botwork_config_broker::{build_router, AppState};
use botwork_entity::connection::connect;
use botwork_migration::Migrator;
use reqwest::StatusCode;
use sea_orm::DatabaseConnection;
use sea_orm_migration::MigratorTrait;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const POSTGRES_TAG: &str = "16-alpine";

struct Server {
    base: String,
    _handle: JoinHandle<()>,
    _pg: testcontainers::ContainerAsync<Postgres>,
}

async fn docker_available() -> bool {
    use testcontainers::core::WaitFor;
    use testcontainers::GenericImage;
    let probe =
        GenericImage::new("testcontainers/helloworld", "1.3.0").with_wait_for(WaitFor::seconds(1));
    match tokio::time::timeout(Duration::from_secs(5), probe.start()).await {
        Ok(Ok(container)) => {
            let _ = container.rm().await;
            true
        }
        _ => false,
    }
}

async fn start_postgres() -> Result<(testcontainers::ContainerAsync<Postgres>, String), String> {
    use testcontainers::ImageExt;

    let image = Postgres::default()
        .with_db_name("botwork")
        .with_user("botwork")
        .with_password("test")
        .with_tag(POSTGRES_TAG);
    let container = image
        .start()
        .await
        .map_err(|err| format!("start container: {err}"))?;
    let host = container
        .get_host()
        .await
        .map_err(|err| format!("get_host: {err}"))?;
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .map_err(|err| format!("get_host_port_ipv4: {err}"))?;
    let url = format!("postgres://botwork:test@{host}:{port}/botwork");
    Ok((container, url))
}

async fn connect_with_retry(url: &str) -> Result<DatabaseConnection, sea_orm::DbErr> {
    let mut last = None;
    for attempt in 0..10u32 {
        match connect(url).await {
            Ok(db) => return Ok(db),
            Err(err) => {
                last = Some(err);
                tokio::time::sleep(Duration::from_millis(200 * (1 + u64::from(attempt)))).await;
            }
        }
    }
    Err(last.expect("at least one error after retry loop"))
}

/// Spin up postgres, migrate, bootstrap the supplied yaml, and bind
/// the broker on a random local port. Returns `None` when docker
/// isn't available (test prints IGNORED and exits early).
async fn spawn_server(bootstrap_yaml: &str) -> Option<Server> {
    if !docker_available().await {
        return None;
    }
    let (pg, url) = start_postgres()
        .await
        .expect("postgres container must start");
    let db = connect_with_retry(&url)
        .await
        .expect("connect to ephemeral postgres");
    Migrator::up(&db, None)
        .await
        .expect("schema migrations must apply");

    let raw: BootstrapConfigRaw =
        serde_yaml::from_str(bootstrap_yaml).expect("bootstrap yaml parse");
    let cfg = BootstrapConfig::from_raw(raw).expect("bootstrap validate");
    apply(&db, &cfg).await.expect("bootstrap apply");

    let state = AppState { db: Arc::new(db) };
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Some(Server {
        base: format!("http://{addr}"),
        _handle: handle,
        _pg: pg,
    })
}

/// Sample bootstrap config used across the resolve tests. Mirrors what
/// the production deployment will carry: one tenant, one workspace,
/// two plugins with the full set of fields.
const SAMPLE_YAML: &str = r#"
tenants:
- name: phlax
  workspaces:
  - name: mcp
    plugins:
    - name: mcp-bash
    - name: mcp-fetch
      config:
        url: https://example.com

plugins:
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:1.0
  egress: none
- name: mcp-fetch
  image: ghcr.io/example/mcp-fetch:1.0
  port: 8001
  path: /mcp
  upstream_auth: bearer/example.com
  env:
    LOG_LEVEL: info
  resources:
    memory: 4g
    pids: 1024
  egress:
    allow:
    - host: example.com
      ports: [443]
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolves_known_plugin_returns_full_descriptor() {
    let Some(server) = spawn_server(SAMPLE_YAML).await else {
        eprintln!(
            "IGNORED resolves_known_plugin_returns_full_descriptor: \
             docker not reachable; full proof runs in ci.yml smoke"
        );
        return;
    };
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/resolve", server.base))
        .json(&serde_json::json!({
            "tenant": "phlax",
            "workspace": "mcp",
            "plugin": "mcp-fetch",
        }))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["image"], "ghcr.io/example/mcp-fetch:1.0");
    assert_eq!(body["port"], 8001);
    assert_eq!(body["path"], "/mcp");
    assert_eq!(body["upstream_auth"], "bearer/example.com");
    assert_eq!(body["resources"]["memory"], "4g");
    assert_eq!(body["resources"]["pids"], 1024);
    assert_eq!(body["env"][0]["name"], "LOG_LEVEL");
    assert_eq!(body["env"][0]["value"], "info");
    assert!(body["config_blob"]
        .as_str()
        .unwrap()
        .contains("https://example.com"));
    assert_eq!(body["egress"]["allow"][0]["host"], "example.com");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_unknown_plugin_returns_404() {
    let Some(server) = spawn_server(SAMPLE_YAML).await else {
        eprintln!("IGNORED resolve_unknown_plugin_returns_404");
        return;
    };
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/resolve", server.base))
        .json(&serde_json::json!({
            "tenant": "phlax",
            "workspace": "mcp",
            "plugin": "nope",
        }))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "unknown_plugin");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_unknown_tenant_or_workspace_returns_404() {
    let Some(server) = spawn_server(SAMPLE_YAML).await else {
        eprintln!("IGNORED resolve_unknown_tenant_or_workspace_returns_404");
        return;
    };
    let client = reqwest::Client::new();
    for body in [
        serde_json::json!({"tenant": "ada", "workspace": "mcp", "plugin": "mcp-bash"}),
        serde_json::json!({"tenant": "phlax", "workspace": "scratch", "plugin": "mcp-bash"}),
    ] {
        let resp = client
            .post(format!("{}/resolve", server.base))
            .json(&body)
            .send()
            .await
            .expect("POST");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{body}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_returns_400_for_bad_workspace_shape() {
    let Some(server) = spawn_server(SAMPLE_YAML).await else {
        eprintln!("IGNORED resolve_returns_400_for_bad_workspace_shape");
        return;
    };
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/resolve", server.base))
        .json(&serde_json::json!({
            "tenant": "phlax",
            "workspace": "BAD",
            "plugin": "mcp-bash",
        }))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "invalid_workspace");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_returns_400_when_required_field_missing() {
    let Some(server) = spawn_server(SAMPLE_YAML).await else {
        eprintln!("IGNORED resolve_returns_400_when_required_field_missing");
        return;
    };
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/resolve", server.base))
        .json(&serde_json::json!({
            "tenant": "phlax",
            "workspace": "mcp",
        }))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "invalid_request");
}
