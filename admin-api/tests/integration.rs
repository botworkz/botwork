//! End-to-end wire-contract tests for the v0 surface.
//!
//! Spins up a real postgres via testcontainers, runs the schema
//! migrations, applies a known-good `bootstrap.yaml` so each entity
//! has at least one row, then binds admin-api on a random local port
//! and exercises the read endpoints.
//!
//! The tests are gated on docker the same way the bootstrap /
//! migration smokes are: a clearly-labelled `IGNORED` line when
//! docker isn't reachable keeps `cargo test` green on dev machines
//! without docker. Full proof runs in `.github/workflows/containers.yml`.
//!
//! Fixture shape (kept in `SAMPLE_YAML` below):
//!
//! * 1 tenant (`phlax`)
//! * 1 workspace (`mcp`) under that tenant
//! * 2 plugins (`mcp-bash`, `mcp-fetch`)
//! * 2 bindings (one for each plugin under `(phlax, mcp)`); the
//!   `mcp-fetch` binding carries a per-binding `config:` blob
//!
//! That gives the read endpoints something to discriminate against:
//! list endpoints return the right counts, by-id endpoints hit and
//! miss as expected, and the workspace filter narrows results.

use std::sync::Arc;
use std::time::Duration;

use botwork_admin_api::{build_router, AppState};
use botwork_bootstrap::{apply, BootstrapConfig, BootstrapConfigRaw};
use botwork_entity::connection::connect;
use botwork_migration::Migrator;
use reqwest::StatusCode;
use sea_orm::DatabaseConnection;
use sea_orm_migration::MigratorTrait;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use uuid::Uuid;

const POSTGRES_TAG: &str = "16-alpine";

/// Minimal-but-non-degenerate fixture: 1 tenant, 1 workspace, 2
/// plugins (one with a config-carrying binding) — enough to exercise
/// every read endpoint including the workspace_plugins filter shape.
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

/// Spin postgres, land the schema, apply [`SAMPLE_YAML`], and bind
/// admin-api on a random localhost port. Returns `None` when docker
/// is unreachable; every test short-circuits on that and prints
/// `IGNORED ...` so dev runs without docker still pass.
async fn spawn_server() -> Option<Server> {
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

    // Hand the bootstrap binary's apply path the same yaml we'd
    // ship in production so admin-api reads against realistic shape.
    let raw: BootstrapConfigRaw = serde_yaml::from_str(SAMPLE_YAML).expect("bootstrap yaml parse");
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

// ── health (unchanged from PR1) ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_endpoint_reports_db_reachable() {
    let Some(server) = spawn_server().await else {
        eprintln!(
            "IGNORED health_endpoint_reports_db_reachable: \
             docker not reachable; full proof runs in containers.yml smoke"
        );
        return;
    };
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/admin/api/v1/health", server.base))
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["db"], "reachable");
    assert!(body.get("message").is_none());
}

// ── tenant ──────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_tenants_returns_seeded_row() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED list_tenants_returns_seeded_row");
        return;
    };
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/admin/api/v1/tenants", server.base))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    assert_eq!(body["total"], 1);
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "phlax");
    // Every entity carries id + timestamps.
    assert!(items[0]["id"].is_string());
    assert!(items[0]["created_at"].is_string());
    assert!(items[0]["updated_at"].is_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_tenant_by_id_round_trips() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED get_tenant_by_id_round_trips");
        return;
    };
    let client = reqwest::Client::new();
    let list: serde_json::Value = client
        .get(format!("{}/admin/api/v1/tenants", server.base))
        .send()
        .await
        .expect("GET list")
        .json()
        .await
        .expect("json");
    let id = list["items"][0]["id"].as_str().expect("id").to_owned();
    let single: serde_json::Value = client
        .get(format!("{}/admin/api/v1/tenants/{id}", server.base))
        .send()
        .await
        .expect("GET single")
        .json()
        .await
        .expect("json");
    assert_eq!(single["name"], "phlax");
    assert_eq!(single["id"], id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_tenant_unknown_id_is_404() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED get_tenant_unknown_id_is_404");
        return;
    };
    let id = Uuid::new_v4();
    let resp = reqwest::Client::new()
        .get(format!("{}/admin/api/v1/tenants/{id}", server.base))
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "not_found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_tenant_invalid_uuid_is_400() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED get_tenant_invalid_uuid_is_400");
        return;
    };
    let resp = reqwest::Client::new()
        .get(format!("{}/admin/api/v1/tenants/not-a-uuid", server.base))
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "bad_request");
}

// ── workspace ───────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_workspaces_returns_seeded_row() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED list_workspaces_returns_seeded_row");
        return;
    };
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/admin/api/v1/workspaces", server.base))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["name"], "mcp");
    assert!(body["items"][0]["tenant_id"].is_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_workspaces_filters_by_tenant_id() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED list_workspaces_filters_by_tenant_id");
        return;
    };
    let client = reqwest::Client::new();
    let tenants: serde_json::Value = client
        .get(format!("{}/admin/api/v1/tenants", server.base))
        .send()
        .await
        .expect("GET tenants")
        .json()
        .await
        .expect("json");
    let tenant_id = tenants["items"][0]["id"].as_str().expect("id");

    // Matching tenant: 1 row.
    let body: serde_json::Value = client
        .get(format!(
            "{}/admin/api/v1/workspaces?tenant_id={tenant_id}",
            server.base
        ))
        .send()
        .await
        .expect("GET filtered")
        .json()
        .await
        .expect("json");
    assert_eq!(body["total"], 1);

    // Mismatching tenant: 0 rows.
    let other = Uuid::new_v4();
    let body: serde_json::Value = client
        .get(format!(
            "{}/admin/api/v1/workspaces?tenant_id={other}",
            server.base
        ))
        .send()
        .await
        .expect("GET filtered-empty")
        .json()
        .await
        .expect("json");
    assert_eq!(body["total"], 0);
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_workspaces_rejects_garbage_tenant_filter() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED list_workspaces_rejects_garbage_tenant_filter");
        return;
    };
    let resp = reqwest::Client::new()
        .get(format!(
            "{}/admin/api/v1/workspaces?tenant_id=not-a-uuid",
            server.base
        ))
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ── plugin ──────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_plugins_returns_seeded_rows() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED list_plugins_returns_seeded_rows");
        return;
    };
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/admin/api/v1/plugins", server.base))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    assert_eq!(body["total"], 2);
    let names: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    // Asc-by-name ordering: bash before fetch.
    assert_eq!(names, vec!["mcp-bash", "mcp-fetch"]);
    // Fetch entry carries the validated egress allow-list verbatim.
    let fetch = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "mcp-fetch")
        .expect("mcp-fetch row");
    assert_eq!(fetch["port"], 8001);
    assert_eq!(fetch["path"], "/mcp");
    assert_eq!(fetch["egress"]["allow"][0]["host"], "example.com");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_plugin_by_id_round_trips() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED get_plugin_by_id_round_trips");
        return;
    };
    let client = reqwest::Client::new();
    let list: serde_json::Value = client
        .get(format!("{}/admin/api/v1/plugins", server.base))
        .send()
        .await
        .expect("GET list")
        .json()
        .await
        .expect("json");
    let id = list["items"][0]["id"].as_str().expect("id").to_owned();
    let resp = client
        .get(format!("{}/admin/api/v1/plugins/{id}", server.base))
        .send()
        .await
        .expect("GET single");
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["id"], id);
}

// ── workspace_plugin (binding) ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_workspace_plugins_returns_seeded_bindings() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED list_workspace_plugins_returns_seeded_bindings");
        return;
    };
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/admin/api/v1/workspace_plugins", server.base))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    assert_eq!(body["total"], 2);
    // Exactly one binding carries a non-null `config` (mcp-fetch).
    let with_config = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| !b["config"].is_null())
        .count();
    assert_eq!(with_config, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_workspace_plugins_filters_by_plugin_id() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED list_workspace_plugins_filters_by_plugin_id");
        return;
    };
    let client = reqwest::Client::new();
    let plugins: serde_json::Value = client
        .get(format!("{}/admin/api/v1/plugins", server.base))
        .send()
        .await
        .expect("GET plugins")
        .json()
        .await
        .expect("json");
    let bash_id = plugins["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "mcp-bash")
        .expect("mcp-bash row")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let body: serde_json::Value = client
        .get(format!(
            "{}/admin/api/v1/workspace_plugins?plugin_id={bash_id}",
            server.base
        ))
        .send()
        .await
        .expect("GET filtered")
        .json()
        .await
        .expect("json");
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["plugin_id"], bash_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_workspace_plugin_round_trips_composite_pk() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED get_workspace_plugin_round_trips_composite_pk");
        return;
    };
    let client = reqwest::Client::new();
    let list: serde_json::Value = client
        .get(format!("{}/admin/api/v1/workspace_plugins", server.base))
        .send()
        .await
        .expect("GET list")
        .json()
        .await
        .expect("json");
    let entry = &list["items"][0];
    let wid = entry["workspace_id"].as_str().expect("workspace_id");
    let pid = entry["plugin_id"].as_str().expect("plugin_id");
    let resp = client
        .get(format!(
            "{}/admin/api/v1/workspace_plugins/{wid}/{pid}",
            server.base
        ))
        .send()
        .await
        .expect("GET single");
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["workspace_id"], wid);
    assert_eq!(body["plugin_id"], pid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_workspace_plugin_unknown_pair_is_404() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED get_workspace_plugin_unknown_pair_is_404");
        return;
    };
    let wid = Uuid::new_v4();
    let pid = Uuid::new_v4();
    let resp = reqwest::Client::new()
        .get(format!(
            "{}/admin/api/v1/workspace_plugins/{wid}/{pid}",
            server.base
        ))
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
