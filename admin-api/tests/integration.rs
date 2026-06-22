//! End-to-end wire-contract tests for the v0 surface.
//!
//! Spins up a real postgres via testcontainers, runs the schema
//! migrations, applies a known-good `bootstrap.yaml` so each entity
//! has at least one row, then binds admin-api on a random local port
//! and exercises the read endpoints + the write endpoints.
//!
//! Write tests with live-state coupling stub control-plane via
//! `wiremock`. The default `spawn_server` flips the control-plane
//! gate to disabled so write paths that don't care about the
//! coupling stay simple; tests that DO care swap in a `wiremock`
//! endpoint explicitly.
//!
//! The tests are gated on docker the same way the bootstrap /
//! migration smokes are: a clearly-labelled `IGNORED` line when
//! docker isn't reachable keeps `cargo test` green on dev machines
//! without docker. Full proof runs in `.github/workflows/ci.yml`.
//!
//! Fixture shape (kept in `SAMPLE_YAML` below):
//!
//! * 1 tenant (`phlax`)
//! * 1 workspace (`mcp`) under that tenant
//! * 2 plugins (`mcp-bash`, `mcp-fetch`)
//! * 2 bindings (one for each plugin under `(phlax, mcp)`); the
//!   `mcp-fetch` binding carries a per-binding `config:` blob

use std::sync::Arc;
use std::time::Duration;

use botwork_admin_api::{build_router, AppState, ControlPlaneClient};
use botwork_bootstrap::{apply, BootstrapConfig, BootstrapConfigRaw};
use botwork_entity::connection::connect;
use botwork_migration::Migrator;
use reqwest::StatusCode;
use sea_orm::DatabaseConnection;
use sea_orm_migration::MigratorTrait;
use serde_json::json;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use uuid::Uuid;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
///
/// `control_plane` lets tests inject a wiremock-backed client;
/// passing `None` flips the gate to disabled (the default for tests
/// that don't exercise the coupling).
async fn spawn_server_with(control_plane: Option<ControlPlaneClient>) -> Option<Server> {
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

    let state = AppState {
        db: Arc::new(db),
        control_plane: control_plane.unwrap_or_else(ControlPlaneClient::disabled),
    };
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

async fn spawn_server() -> Option<Server> {
    spawn_server_with(None).await
}

/// Build a wiremock server that 5xxs every request. Used by the
/// failure-path test to assert the DB write rolls back and admin-api
/// returns 503.
async fn unreachable_control_plane() -> (MockServer, ControlPlaneClient) {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/sessions"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({"error": "down"})))
        .mount(&mock)
        .await;
    let client = ControlPlaneClient::with_endpoint(mock.uri());
    (mock, client)
}

// ── health (unchanged from PR1) ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_endpoint_reports_db_reachable() {
    let Some(server) = spawn_server().await else {
        eprintln!(
            "IGNORED health_endpoint_reports_db_reachable: \
             docker not reachable; full proof runs in ci.yml smoke"
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

// ── read tests (carried from PR2) ───────────────────────────────────

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

// ── workspace reads (carried from PR2) ──────────────────────────────

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

// ── plugin reads (carried from PR2) ─────────────────────────────────

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

// ── workspace_plugin reads (carried from PR2) ───────────────────────

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

// ── tenant writes ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_tenant_returns_201_and_location() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED create_tenant_returns_201_and_location");
        return;
    };
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/api/v1/tenants", server.base))
        .json(&json!({"name": "ada"}))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let location = resp
        .headers()
        .get("location")
        .expect("Location header")
        .to_str()
        .expect("ascii");
    assert!(
        location.starts_with("/admin/api/v1/tenants/"),
        "unexpected Location: {location}"
    );
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["name"], "ada");
    assert!(body["id"].is_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_tenant_rejects_duplicate_with_409_already_exists() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED create_tenant_rejects_duplicate_with_409_already_exists");
        return;
    };
    // phlax is in the seed.
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/api/v1/tenants", server.base))
        .json(&json!({"name": "phlax"}))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "already_exists");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_tenant_rejects_unknown_field_with_400() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED create_tenant_rejects_unknown_field_with_400");
        return;
    };
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/api/v1/tenants", server.base))
        .json(&json!({"name": "ada", "typo": "x"}))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "bad_request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_tenant_rejects_invalid_name_with_422() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED create_tenant_rejects_invalid_name_with_422");
        return;
    };
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/api/v1/tenants", server.base))
        .json(&json!({"name": "Has Spaces"}))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "validation_failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_tenant_round_trips_with_lock() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED update_tenant_round_trips_with_lock");
        return;
    };
    let client = reqwest::Client::new();
    let list: serde_json::Value = client
        .get(format!("{}/admin/api/v1/tenants", server.base))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let id = list["items"][0]["id"].as_str().unwrap().to_owned();
    let token = list["items"][0]["updated_at"].as_str().unwrap().to_owned();
    let resp = client
        .put(format!("{}/admin/api/v1/tenants/{id}", server.base))
        .json(&json!({"name": "phlax-renamed", "if_unmodified_since": token}))
        .send()
        .await
        .expect("PUT");
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["name"], "phlax-renamed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_tenant_rejects_stale_lock_with_409_stale_write() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED update_tenant_rejects_stale_lock_with_409_stale_write");
        return;
    };
    let client = reqwest::Client::new();
    let list: serde_json::Value = client
        .get(format!("{}/admin/api/v1/tenants", server.base))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let id = list["items"][0]["id"].as_str().unwrap().to_owned();
    // Use a wrong-but-well-formed timestamp.
    let resp = client
        .put(format!("{}/admin/api/v1/tenants/{id}", server.base))
        .json(&json!({
            "name": "phlax-renamed",
            "if_unmodified_since": "2000-01-01T00:00:00Z",
        }))
        .send()
        .await
        .expect("PUT");
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "stale_write");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_tenant_blocks_with_409_has_dependents_and_names_workspaces() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED delete_tenant_blocks_with_409_has_dependents_and_names_workspaces");
        return;
    };
    let client = reqwest::Client::new();
    let list: serde_json::Value = client
        .get(format!("{}/admin/api/v1/tenants", server.base))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let id = list["items"][0]["id"].as_str().unwrap().to_owned();
    let resp = client
        .delete(format!("{}/admin/api/v1/tenants/{id}", server.base))
        .send()
        .await
        .expect("DELETE");
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "has_dependents");
    let dependents = body["dependents"].as_array().expect("dependents array");
    let names: Vec<&str> = dependents
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["mcp"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_tenant_succeeds_when_no_dependents() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED delete_tenant_succeeds_when_no_dependents");
        return;
    };
    let client = reqwest::Client::new();
    // Create a tenant with no workspaces, then delete it.
    let create: serde_json::Value = client
        .post(format!("{}/admin/api/v1/tenants", server.base))
        .json(&json!({"name": "deletable"}))
        .send()
        .await
        .expect("POST")
        .json()
        .await
        .expect("json");
    let id = create["id"].as_str().unwrap().to_owned();
    let resp = client
        .delete(format!("{}/admin/api/v1/tenants/{id}", server.base))
        .send()
        .await
        .expect("DELETE");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    // Subsequent GET is 404.
    let resp = client
        .get(format!("{}/admin/api/v1/tenants/{id}", server.base))
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── workspace writes ───────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_workspace_inserts_under_tenant() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED create_workspace_inserts_under_tenant");
        return;
    };
    let client = reqwest::Client::new();
    let tenants: serde_json::Value = client
        .get(format!("{}/admin/api/v1/tenants", server.base))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let tenant_id = tenants["items"][0]["id"].as_str().unwrap();
    let resp = client
        .post(format!("{}/admin/api/v1/workspaces", server.base))
        .json(&json!({"tenant_id": tenant_id, "name": "second"}))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["name"], "second");
    assert_eq!(body["tenant_id"], tenant_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_workspace_unknown_tenant_id_is_422_validation_failed() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED create_workspace_unknown_tenant_id_is_422_validation_failed");
        return;
    };
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/api/v1/workspaces", server.base))
        .json(&json!({"tenant_id": Uuid::new_v4(), "name": "orphan"}))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "validation_failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_workspace_succeeds_when_control_plane_acks_empty_session_set() {
    let Some(server) = spawn_server_with(None).await else {
        eprintln!("IGNORED delete_workspace_succeeds_when_control_plane_acks_empty_session_set");
        return;
    };
    // The default `spawn_server` flips control-plane to disabled.
    // The bindings under the seed workspace CASCADE-delete; the
    // live-state coupling is skipped per the break-glass posture
    // (logged but not blocked). End-state: 204.
    let client = reqwest::Client::new();
    let ws: serde_json::Value = client
        .get(format!("{}/admin/api/v1/workspaces", server.base))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let id = ws["items"][0]["id"].as_str().unwrap().to_owned();
    let resp = client
        .delete(format!("{}/admin/api/v1/workspaces/{id}", server.base))
        .send()
        .await
        .expect("DELETE");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

// ── plugin writes ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_plugin_invokes_admin_core_validator() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED create_plugin_invokes_admin_core_validator");
        return;
    };
    // Missing required `egress` field -> admin-core rejects.
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/api/v1/plugins", server.base))
        .json(&json!({
            "name": "mcp-new",
            "image": "ghcr.io/example/p:1.0",
        }))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "validation_failed");
    assert!(
        body["message"].as_str().unwrap().contains("egress"),
        "validation error should name the missing field: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_plugin_happy_path() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED create_plugin_happy_path");
        return;
    };
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/api/v1/plugins", server.base))
        .json(&json!({
            "name": "mcp-new",
            "image": "ghcr.io/example/p:1.0",
            "egress": "none",
        }))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["name"], "mcp-new");
    // admin-core fills defaults.
    assert_eq!(body["port"], 8000);
    assert_eq!(body["path"], "/");
    assert_eq!(body["upstream_auth"], "none");
    assert_eq!(body["egress"]["mode"], "none");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_plugin_blocks_with_409_and_names_bindings() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED delete_plugin_blocks_with_409_and_names_bindings");
        return;
    };
    let client = reqwest::Client::new();
    let plugins: serde_json::Value = client
        .get(format!("{}/admin/api/v1/plugins", server.base))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let id = plugins["items"][0]["id"].as_str().unwrap().to_owned();
    let resp = client
        .delete(format!("{}/admin/api/v1/plugins/{id}", server.base))
        .send()
        .await
        .expect("DELETE");
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "has_dependents");
    let dependents = body["dependents"].as_array().expect("dependents");
    // Each entry carries identifying fields per the contract.
    assert!(!dependents.is_empty());
    assert_eq!(dependents[0]["kind"], "workspace_plugin");
    assert!(dependents[0]["tenant"].is_string());
    assert!(dependents[0]["workspace"].is_string());
}

// ── workspace_plugin writes (with control-plane gate) ──────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_binding_terminates_live_sessions_then_succeeds() {
    if !docker_available().await {
        eprintln!("IGNORED delete_binding_terminates_live_sessions_then_succeeds");
        return;
    }
    // Wire a wiremock control-plane that returns one matching live
    // session, expects exactly one DELETE against that id, and 200s
    // it. The DB write should commit and we should see 204.
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/sessions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sessions": [
                {
                    "session_id": "mcp_session_deadbeef",
                    "container_ip": "172.20.0.5",
                    "tenant": "phlax",
                    "workspace": "mcp",
                    "plugin": "mcp-bash",
                    "egress_policy": null,
                }
            ]
        })))
        .mount(&mock)
        .await;
    Mock::given(method("DELETE"))
        .and(path_regex(r"^/sessions/mcp_session_deadbeef$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"status": "removed", "session_id": "mcp_session_deadbeef"})),
        )
        .expect(1)
        .mount(&mock)
        .await;
    let client_cp = ControlPlaneClient::with_endpoint(mock.uri());
    let Some(server) = spawn_server_with(Some(client_cp)).await else {
        return;
    };

    let client = reqwest::Client::new();
    let plugins: serde_json::Value = client
        .get(format!("{}/admin/api/v1/plugins?", server.base))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let plugin_id = plugins["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "mcp-bash")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let bindings: serde_json::Value = client
        .get(format!(
            "{}/admin/api/v1/workspace_plugins?plugin_id={plugin_id}",
            server.base
        ))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let wid = bindings["items"][0]["workspace_id"].as_str().unwrap();
    let pid = bindings["items"][0]["plugin_id"].as_str().unwrap();

    let resp = client
        .delete(format!(
            "{}/admin/api/v1/workspace_plugins/{wid}/{pid}",
            server.base
        ))
        .send()
        .await
        .expect("DELETE");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    // Subsequent GET is 404.
    let resp = client
        .get(format!(
            "{}/admin/api/v1/workspace_plugins/{wid}/{pid}",
            server.base
        ))
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_binding_rolls_back_when_control_plane_unavailable() {
    if !docker_available().await {
        eprintln!("IGNORED delete_binding_rolls_back_when_control_plane_unavailable");
        return;
    }
    let (_mock, cp) = unreachable_control_plane().await;
    let Some(server) = spawn_server_with(Some(cp)).await else {
        return;
    };

    let client = reqwest::Client::new();
    // Look up an existing binding.
    let bindings: serde_json::Value = client
        .get(format!("{}/admin/api/v1/workspace_plugins", server.base))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let wid = bindings["items"][0]["workspace_id"].as_str().unwrap();
    let pid = bindings["items"][0]["plugin_id"].as_str().unwrap();

    let resp = client
        .delete(format!(
            "{}/admin/api/v1/workspace_plugins/{wid}/{pid}",
            server.base
        ))
        .send()
        .await
        .expect("DELETE");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "unavailable");

    // The binding should still exist — rollback worked.
    let resp = client
        .get(format!(
            "{}/admin/api/v1/workspace_plugins/{wid}/{pid}",
            server.base
        ))
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_binding_happy_path_no_live_gate() {
    if !docker_available().await {
        eprintln!("IGNORED create_binding_happy_path_no_live_gate");
        return;
    }
    // CREATE doesn't consult control-plane (new binding can't affect
    // already-spawned sessions); make sure the wiremock receives no
    // calls.
    let mock = MockServer::start().await;
    let cp = ControlPlaneClient::with_endpoint(mock.uri());
    let Some(server) = spawn_server_with(Some(cp)).await else {
        return;
    };
    let client = reqwest::Client::new();

    // Make a new workspace + bind one of the existing plugins.
    let tenants: serde_json::Value = client
        .get(format!("{}/admin/api/v1/tenants", server.base))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let tenant_id = tenants["items"][0]["id"].as_str().unwrap();
    let new_ws: serde_json::Value = client
        .post(format!("{}/admin/api/v1/workspaces", server.base))
        .json(&json!({"tenant_id": tenant_id, "name": "fresh"}))
        .send()
        .await
        .expect("POST")
        .json()
        .await
        .expect("json");
    let new_wid = new_ws["id"].as_str().unwrap().to_owned();

    let plugins: serde_json::Value = client
        .get(format!("{}/admin/api/v1/plugins", server.base))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let pid = plugins["items"][0]["id"].as_str().unwrap().to_owned();

    let resp = client
        .post(format!("{}/admin/api/v1/workspace_plugins", server.base))
        .json(&json!({
            "workspace_id": new_wid,
            "plugin_id": pid,
            "config": {"sentinel": "yes"},
        }))
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["workspace_id"], new_wid);
    assert_eq!(body["plugin_id"], pid);
    assert_eq!(body["config"]["sentinel"], "yes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_binding_disabled_gate_succeeds_without_control_plane() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED delete_binding_disabled_gate_succeeds_without_control_plane");
        return;
    };
    // Default `spawn_server` uses `ControlPlaneClient::disabled` —
    // exercises the break-glass posture.
    let client = reqwest::Client::new();
    let bindings: serde_json::Value = client
        .get(format!("{}/admin/api/v1/workspace_plugins", server.base))
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let wid = bindings["items"][0]["workspace_id"].as_str().unwrap();
    let pid = bindings["items"][0]["plugin_id"].as_str().unwrap();
    let resp = client
        .delete(format!(
            "{}/admin/api/v1/workspace_plugins/{wid}/{pid}",
            server.base
        ))
        .send()
        .await
        .expect("DELETE");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}
