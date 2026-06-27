//! End-to-end wire-contract tests for the v0 surface.
//!
//! Spins up a real postgres via testcontainers, runs the schema
//! migrations, applies a known-good `bootstrap.yaml` so each entity
//! has at least one row, then binds api on a random local port
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

use botwork_api::{build_router, AppState, ControlPlaneClient, SecretStoreClient};
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
    // Exposed so tests can seed tables that api itself can't
    // (agent_session, session_worker — both are session-broker's
    // write surface; api only reads them). Tests insert
    // straight through sea-orm rather than spinning up
    // session-broker as an additional fixture.
    db: Arc<DatabaseConnection>,
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
/// api on a random localhost port. Returns `None` when docker
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
    // ship in production so api reads against realistic shape.
    let raw: BootstrapConfigRaw = serde_yaml::from_str(SAMPLE_YAML).expect("bootstrap yaml parse");
    let cfg = BootstrapConfig::from_raw(raw).expect("bootstrap validate");
    apply(&db, &cfg).await.expect("bootstrap apply");

    let db_arc = Arc::new(db);
    let state = AppState {
        db: db_arc.clone(),
        control_plane: control_plane.unwrap_or_else(ControlPlaneClient::disabled),
        secret_store: SecretStoreClient::disabled(),
    };
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Some(Server {
        base: format!("http://{addr}"),
        db: db_arc,
        _handle: handle,
        _pg: pg,
    })
}

async fn spawn_server() -> Option<Server> {
    spawn_server_with(None).await
}

/// Build a wiremock server that 5xxs every request. Used by the
/// failure-path test to assert the DB write rolls back and api
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
        .get(format!("{}/api/health", server.base))
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
        .get(format!("{}/api/tenants", server.base))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
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
        .get(format!("{}/api/tenants", server.base))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET list")
        .json()
        .await
        .expect("json");
    let id = list["items"][0]["id"].as_str().expect("id").to_owned();
    let single: serde_json::Value = client
        .get(format!("{}/api/tenants/{id}", server.base))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
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
        .get(format!("{}/api/tenants/{id}", server.base))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
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
        .get(format!("{}/api/tenants/not-a-uuid", server.base))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
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
        .get(format!("{}/api/tenant/phlax/workspaces", server.base))
        .header("x-botwork-tenant", "phlax")
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

/// Cross-tenant denial: path tenant does not match the `x-botwork-tenant`
/// header injected by auth-broker. Should return 403 with
/// `error.code = "cross_tenant_forbidden"`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_tenant_request_is_403() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED cross_tenant_request_is_403");
        return;
    };
    // Path says `phlax` but header says `other` — mismatch → 403.
    let resp = reqwest::Client::new()
        .get(format!("{}/api/tenant/phlax/workspaces", server.base))
        .header("x-botwork-tenant", "other")
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "cross_tenant_forbidden");
}

/// Missing `x-botwork-tenant` header on tenant-scoped endpoint returns 400.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tenant_scoped_endpoint_without_header_is_400() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED tenant_scoped_endpoint_without_header_is_400");
        return;
    };
    let resp = reqwest::Client::new()
        .get(format!("{}/api/tenant/phlax/workspaces", server.base))
        // No x-botwork-tenant header → handler calls check_tenant_consistency
        // which returns 400 (missing header, not mismatch).
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
        .get(format!("{}/api/plugins", server.base))
        .header("x-botwork-admin", "true")
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
        .get(format!("{}/api/plugins", server.base))
        .header("x-botwork-admin", "true")
        .send()
        .await
        .expect("GET list")
        .json()
        .await
        .expect("json");
    let id = list["items"][0]["id"].as_str().expect("id").to_owned();
    let resp = client
        .get(format!("{}/api/plugins/{id}", server.base))
        .header("x-botwork-admin", "true")
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
        .get(format!("{}/api/tenant/phlax/workspace_plugins", server.base))
        .header("x-botwork-tenant", "phlax")
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
        .get(format!("{}/api/plugins", server.base))
        .header("x-botwork-admin", "true")
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
            "{}/api/tenant/phlax/workspace_plugins?plugin_id={bash_id}",
            server.base
        ))
        .header("x-botwork-tenant", "phlax")
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
        .get(format!("{}/api/tenant/phlax/workspace_plugins", server.base))
        .header("x-botwork-tenant", "phlax")
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
            "{}/api/tenant/phlax/workspace_plugins/{wid}/{pid}",
            server.base
        ))
        .header("x-botwork-tenant", "phlax")
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
            "{}/api/tenant/phlax/workspace_plugins/{wid}/{pid}",
            server.base
        ))
        .header("x-botwork-tenant", "phlax")
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
        .post(format!("{}/api/tenants", server.base))
        .json(&json!({"name": "ada"}))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
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
        location.starts_with("/api/tenants/"),
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
        .post(format!("{}/api/tenants", server.base))
        .json(&json!({"name": "phlax"}))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
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
        .post(format!("{}/api/tenants", server.base))
        .json(&json!({"name": "ada", "typo": "x"}))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "bad_request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_tenant_rejects_invalid_name_with_400() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED create_tenant_rejects_invalid_name_with_400");
        return;
    };
    // "Has Spaces" fails the name regex (^[A-Za-z0-9_-]{1,63}$).
    let resp = reqwest::Client::new()
        .post(format!("{}/api/tenants", server.base))
        .json(&json!({"name": "Has Spaces"}))
        .header("x-botwork-admin", "true")
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "invalid_name");
}

/// Tenant names in the reserved set (`admin`, `api`, etc.) return 400
/// with `error = "reserved_name"`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_tenant_rejects_reserved_name_with_400() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED create_tenant_rejects_reserved_name_with_400");
        return;
    };
    let resp = reqwest::Client::new()
        .post(format!("{}/api/tenants", server.base))
        .json(&json!({"name": "admin"}))
        .header("x-botwork-admin", "true")
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "reserved_name");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_tenant_round_trips_with_lock() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED update_tenant_round_trips_with_lock");
        return;
    };
    let client = reqwest::Client::new();
    let list: serde_json::Value = client
        .get(format!("{}/api/tenants", server.base))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let id = list["items"][0]["id"].as_str().unwrap().to_owned();
    let token = list["items"][0]["updated_at"].as_str().unwrap().to_owned();
    let resp = client
        .put(format!("{}/api/tenants/{id}", server.base))
        .json(&json!({"name": "phlax-renamed", "if_unmodified_since": token}))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
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
        .get(format!("{}/api/tenants", server.base))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let id = list["items"][0]["id"].as_str().unwrap().to_owned();
    // Use a wrong-but-well-formed timestamp.
    let resp = client
        .put(format!("{}/api/tenants/{id}", server.base))
        .json(&json!({
            "name": "phlax-renamed",
            "if_unmodified_since": "2000-01-01T00:00:00Z",
        }))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
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
        .get(format!("{}/api/tenants", server.base))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let id = list["items"][0]["id"].as_str().unwrap().to_owned();
    let resp = client
        .delete(format!("{}/api/tenants/{id}", server.base))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
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
        .post(format!("{}/api/tenants", server.base))
        .json(&json!({"name": "deletable"}))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("POST")
        .json()
        .await
        .expect("json");
    let id = create["id"].as_str().unwrap().to_owned();
    let resp = client
        .delete(format!("{}/api/tenants/{id}", server.base))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("DELETE");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    // Subsequent GET is 404.
    let resp = client
        .get(format!("{}/api/tenants/{id}", server.base))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
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
        .get(format!("{}/api/tenants", server.base))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let tenant_id = tenants["items"][0]["id"].as_str().unwrap();
    let resp = client
        .post(format!("{}/api/tenant/phlax/workspaces", server.base))
        .json(&json!({"name": "second"}))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["name"], "second");
    assert_eq!(body["tenant_id"], tenant_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_workspace_unknown_tenant_returns_404() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED create_workspace_unknown_tenant_returns_404");
        return;
    };
    // Tenant `nobody` does not exist. Path-borne tenant is the authority;
    // the DB lookup in resolve_tenant_id returns 404.
    // The x-botwork-tenant header must match the path tenant.
    let resp = reqwest::Client::new()
        .post(format!("{}/api/tenant/nobody/workspaces", server.base))
        .json(&json!({"name": "orphan"}))
        .header("x-botwork-tenant", "nobody")
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
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
        .get(format!("{}/api/tenant/phlax/workspaces", server.base))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let id = ws["items"][0]["id"].as_str().unwrap().to_owned();
    let resp = client
        .delete(format!("{}/api/tenant/phlax/workspaces/{id}", server.base))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("DELETE");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

// ── plugin writes ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_plugin_invokes_api_core_validator() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED create_plugin_invokes_api_core_validator");
        return;
    };
    // Missing required `egress` field -> api-core rejects.
    let resp = reqwest::Client::new()
        .post(format!("{}/api/plugins", server.base))
        .json(&json!({
            "name": "mcp-new",
            "image": "ghcr.io/example/p:1.0",
        }))
        .header("x-botwork-admin", "true")
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
        .post(format!("{}/api/plugins", server.base))
        .json(&json!({
            "name": "mcp-new",
            "image": "ghcr.io/example/p:1.0",
            "egress": "none",
        }))
        .header("x-botwork-admin", "true")
        .send()
        .await
        .expect("POST");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["name"], "mcp-new");
    // api-core fills defaults.
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
        .get(format!("{}/api/plugins", server.base))
        .header("x-botwork-admin", "true")
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let id = plugins["items"][0]["id"].as_str().unwrap().to_owned();
    let resp = client
        .delete(format!("{}/api/plugins/{id}", server.base))
        .header("x-botwork-admin", "true")
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
        .get(format!("{}/api/plugins?", server.base))
        .header("x-botwork-admin", "true")
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
            "{}/api/tenant/phlax/workspace_plugins?plugin_id={plugin_id}",
            server.base
        ))
        .header("x-botwork-tenant", "phlax")
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
            "{}/api/tenant/phlax/workspace_plugins/{wid}/{pid}",
            server.base
        ))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("DELETE");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    // Subsequent GET is 404.
    let resp = client
        .get(format!(
            "{}/api/tenant/phlax/workspace_plugins/{wid}/{pid}",
            server.base
        ))
        .header("x-botwork-tenant", "phlax")
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
        .get(format!("{}/api/tenant/phlax/workspace_plugins", server.base))
        .header("x-botwork-tenant", "phlax")
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
            "{}/api/tenant/phlax/workspace_plugins/{wid}/{pid}",
            server.base
        ))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("DELETE");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "unavailable");

    // The binding should still exist — rollback worked.
    let resp = client
        .get(format!(
            "{}/api/tenant/phlax/workspace_plugins/{wid}/{pid}",
            server.base
        ))
        .header("x-botwork-tenant", "phlax")
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
        .get(format!("{}/api/tenants", server.base))
        .header("x-botwork-admin", "true")
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let tenant_id = tenants["items"][0]["id"].as_str().unwrap();
    let new_ws: serde_json::Value = client
        .post(format!("{}/api/tenant/phlax/workspaces", server.base))
        .json(&json!({"name": "fresh"}))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("POST")
        .json()
        .await
        .expect("json");
    let new_wid = new_ws["id"].as_str().unwrap().to_owned();

    let plugins: serde_json::Value = client
        .get(format!("{}/api/plugins", server.base))
        .header("x-botwork-admin", "true")
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    let pid = plugins["items"][0]["id"].as_str().unwrap().to_owned();

    let resp = client
        .post(format!("{}/api/tenant/phlax/workspace_plugins", server.base))
        .json(&json!({
            "workspace_id": new_wid,
            "plugin_id": pid,
            "config": {"sentinel": "yes"},
        }))
        .header("x-botwork-tenant", "phlax")
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
        .get(format!("{}/api/tenant/phlax/workspace_plugins", server.base))
        .header("x-botwork-tenant", "phlax")
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
            "{}/api/tenant/phlax/workspace_plugins/{wid}/{pid}",
            server.base
        ))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("DELETE");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

// ── agent_session reads ────────────────────────────────────────────
//
// session-broker is the writer of agent_session + session_worker; the
// api side only reads them. Tests insert rows directly via
// sea-orm rather than spinning up session-broker as a fixture, both
// because it's the minimum surface for the assertions api makes
// (does the route exist, does the filter narrow correctly, does
// `?live=` walk the reaped_at predicate the right direction) and
// because session-broker's writer is itself end-to-end-tested by
// `session-broker/tests/agent_session_writethrough_test.rs`.

/// Helper: seed `n` agent_session rows under the SAMPLE_YAML
/// `(phlax, mcp)` triple, each in the requested state and with
/// monotonically increasing `last_active_at`. Returns the inserted
/// row ids in insertion order.
async fn seed_agent_sessions(db: &DatabaseConnection, states: &[&str]) -> Vec<Uuid> {
    use botwork_entity::{agent_session, tenant, workspace};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

    let tenant_row = tenant::Entity::find()
        .filter(tenant::Column::Name.eq("phlax"))
        .one(db)
        .await
        .expect("tenant query")
        .expect("seeded tenant");
    let ws_row = workspace::Entity::find()
        .filter(workspace::Column::TenantId.eq(tenant_row.id))
        .filter(workspace::Column::Name.eq("mcp"))
        .one(db)
        .await
        .expect("workspace query")
        .expect("seeded workspace");

    let mut ids = Vec::with_capacity(states.len());
    let base = chrono::Utc::now();
    for (i, st) in states.iter().enumerate() {
        let id = Uuid::new_v4();
        // last_active_at stride is 1s so DESC ordering is unambiguous;
        // the actual stride doesn't matter beyond "strictly increasing".
        let last_active = base + chrono::Duration::seconds(i as i64);
        let row = agent_session::ActiveModel {
            id: Set(id),
            tenant_id: Set(tenant_row.id),
            workspace_id: Set(ws_row.id),
            agent_session_id: Set(format!("agent-{i}")),
            state: Set((*st).to_string()),
            created_at: Set(last_active),
            last_active_at: Set(last_active),
            reactivation_count: Set(0),
        };
        row.insert(db).await.expect("insert agent_session");
        ids.push(id);
    }
    ids
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_agent_sessions_returns_seeded_rows_sorted_by_last_active_desc() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED list_agent_sessions_returns_seeded_rows_sorted_by_last_active_desc");
        return;
    };
    let ids = seed_agent_sessions(&server.db, &["active", "grace", "inactive"]).await;
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/api/tenant/phlax/agent_sessions", server.base))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    assert_eq!(body["total"], 3);
    // Insertion order was [active, grace, inactive] with strictly-
    // increasing last_active_at. DESC sort → inactive first, active last.
    let returned_ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        returned_ids,
        vec![
            ids[2].to_string().as_str(),
            ids[1].to_string().as_str(),
            ids[0].to_string().as_str(),
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_agent_sessions_filters_by_state() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED list_agent_sessions_filters_by_state");
        return;
    };
    seed_agent_sessions(&server.db, &["active", "active", "grace", "inactive"]).await;
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!(
            "{}/api/tenant/phlax/agent_sessions?state=active",
            server.base
        ))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    assert_eq!(body["total"], 2);
    for row in body["items"].as_array().unwrap() {
        assert_eq!(row["state"], "active");
    }
}

/// Agent sessions are tenant-scoped via the path tenant — the path
/// tenant is used to filter sessions, not a query param.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_agent_sessions_returns_tenant_scoped_rows() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED list_agent_sessions_returns_tenant_scoped_rows");
        return;
    };
    seed_agent_sessions(&server.db, &["active"]).await;
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/api/tenant/phlax/agent_sessions", server.base))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    assert_eq!(body["total"], 1);
}

/// Cross-tenant access via agent_sessions returns 403.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_sessions_cross_tenant_is_403() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED agent_sessions_cross_tenant_is_403");
        return;
    };
    let resp = reqwest::Client::new()
        .get(format!("{}/api/tenant/phlax/agent_sessions", server.base))
        .header("x-botwork-tenant", "other")
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "cross_tenant_forbidden");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_agent_session_round_trips() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED get_agent_session_round_trips");
        return;
    };
    let ids = seed_agent_sessions(&server.db, &["active"]).await;
    let id = ids[0];
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/api/tenant/phlax/agent_sessions/{id}", server.base))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    assert_eq!(body["id"], id.to_string());
    assert_eq!(body["state"], "active");
    assert_eq!(body["agent_session_id"], "agent-0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_agent_session_unknown_id_is_404() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED get_agent_session_unknown_id_is_404");
        return;
    };
    let id = Uuid::new_v4();
    let resp = reqwest::Client::new()
        .get(format!("{}/api/tenant/phlax/agent_sessions/{id}", server.base))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_agent_session_invalid_uuid_is_400() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED get_agent_session_invalid_uuid_is_400");
        return;
    };
    let resp = reqwest::Client::new()
        .get(format!(
            "{}/api/tenant/phlax/agent_sessions/not-a-uuid",
            server.base
        ))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ── session_worker reads ───────────────────────────────────────────

/// Helper: insert a session_worker row pinned to the supplied
/// plugin (must be a name from SAMPLE_YAML — today `mcp-bash` or
/// `mcp-fetch`) and the supplied agent_session (or null). Returns
/// the inserted row id.
///
/// Why the plugin name is a parameter: the schema enforces
/// `UNIQUE(agent_session_id, plugin_id) WHERE reaped_at IS NULL`
/// — at most one *live* worker per (session, plugin) pair. Tests
/// that seed multiple live workers under the same session must
/// pick different plugins, or one of the inserts will trip the
/// constraint. The two seeded plugins (mcp-bash, mcp-fetch) are
/// enough for every test we have today.
async fn seed_session_worker(
    db: &DatabaseConnection,
    agent_session_id: Option<Uuid>,
    plugin_name: &str,
    container_name: &str,
    reaped: bool,
    spawned_offset_secs: i64,
) -> Uuid {
    use botwork_entity::{plugin, session_worker};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

    let plugin_row = plugin::Entity::find()
        .filter(plugin::Column::Name.eq(plugin_name))
        .one(db)
        .await
        .expect("plugin query")
        .expect("seeded plugin");

    let id = Uuid::new_v4();
    let spawned = chrono::Utc::now() + chrono::Duration::seconds(spawned_offset_secs);
    let reaped_at = if reaped { Some(spawned) } else { None };
    session_worker::ActiveModel {
        id: Set(id),
        agent_session_id: Set(agent_session_id),
        plugin_id: Set(plugin_row.id),
        container_name: Set(container_name.to_string()),
        container_ip: Set("172.20.0.42".to_string()),
        mcp_session_id: Set(String::new()),
        spawned_at: Set(spawned),
        reaped_at: Set(reaped_at),
    }
    .insert(db)
    .await
    .expect("insert session_worker");
    id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_session_workers_returns_seeded_rows_sorted_by_spawned_desc() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED list_session_workers_returns_seeded_rows_sorted_by_spawned_desc");
        return;
    };
    let session_ids = seed_agent_sessions(&server.db, &["active"]).await;
    // a + b are both LIVE under session_ids[0] — they MUST pin different
    // plugins or the partial UNIQUE index (agent_session_id, plugin_id)
    // WHERE reaped_at IS NULL trips on insert. c is reaped, so it can
    // collide with a's (session, plugin) safely.
    let a = seed_session_worker(
        &server.db,
        Some(session_ids[0]),
        "mcp-bash",
        "mcp_session_a",
        false,
        0,
    )
    .await;
    let b = seed_session_worker(
        &server.db,
        Some(session_ids[0]),
        "mcp-fetch",
        "mcp_session_b",
        false,
        1,
    )
    .await;
    let c = seed_session_worker(
        &server.db,
        Some(session_ids[0]),
        "mcp-bash",
        "mcp_session_c",
        true,
        2,
    )
    .await;
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/api/tenant/phlax/session_workers", server.base))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    assert_eq!(body["total"], 3);
    // spawned_offset_secs was 0/1/2 → c is newest, a is oldest. DESC order.
    let returned: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        returned,
        vec![
            c.to_string().as_str(),
            b.to_string().as_str(),
            a.to_string().as_str(),
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_session_workers_filters_by_live_true_drops_reaped() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED list_session_workers_filters_by_live_true_drops_reaped");
        return;
    };
    let session_ids = seed_agent_sessions(&server.db, &["active"]).await;
    // _reaped is reaped, so it doesn't compete with `live` for the
    // live-uniq slot — both can pin the same plugin.
    let live = seed_session_worker(
        &server.db,
        Some(session_ids[0]),
        "mcp-bash",
        "mcp_session_live",
        false,
        0,
    )
    .await;
    let _reaped = seed_session_worker(
        &server.db,
        Some(session_ids[0]),
        "mcp-bash",
        "mcp_session_reaped",
        true,
        1,
    )
    .await;
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!(
            "{}/api/tenant/phlax/session_workers?live=true",
            server.base
        ))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["id"], live.to_string());
    assert!(body["items"][0]["reaped_at"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_session_workers_filters_by_live_false_drops_live() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED list_session_workers_filters_by_live_false_drops_live");
        return;
    };
    let session_ids = seed_agent_sessions(&server.db, &["active"]).await;
    // Same as the live=true sibling test: _live and reaped don't
    // collide because reaped's reaped_at is NOT NULL, so the partial
    // UNIQUE index doesn't see it.
    let _live = seed_session_worker(
        &server.db,
        Some(session_ids[0]),
        "mcp-bash",
        "mcp_session_live",
        false,
        0,
    )
    .await;
    let reaped = seed_session_worker(
        &server.db,
        Some(session_ids[0]),
        "mcp-bash",
        "mcp_session_reaped",
        true,
        1,
    )
    .await;
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!(
            "{}/api/tenant/phlax/session_workers?live=false",
            server.base
        ))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["id"], reaped.to_string());
    assert!(!body["items"][0]["reaped_at"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_session_workers_filters_by_agent_session_id() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED list_session_workers_filters_by_agent_session_id");
        return;
    };
    let session_ids = seed_agent_sessions(&server.db, &["active", "active"]).await;
    // Different agent sessions → different (session, plugin) keys, so
    // all three can pin the same plugin without tripping the partial
    // UNIQUE index. orphan's agent_session_id is NULL which the index
    // excludes entirely (`WHERE … AND agent_session_id IS NOT NULL`).
    let a_worker = seed_session_worker(
        &server.db,
        Some(session_ids[0]),
        "mcp-bash",
        "mcp_session_a",
        false,
        0,
    )
    .await;
    let _b_worker = seed_session_worker(
        &server.db,
        Some(session_ids[1]),
        "mcp-bash",
        "mcp_session_b",
        false,
        1,
    )
    .await;
    let _orphan =
        seed_session_worker(&server.db, None, "mcp-bash", "mcp_session_orphan", false, 2).await;
    let target = session_ids[0];
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!(
            "{}/api/tenant/phlax/session_workers?agent_session_id={target}",
            server.base
        ))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["id"], a_worker.to_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_session_workers_rejects_garbage_agent_session_id_filter() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED list_session_workers_rejects_garbage_agent_session_id_filter");
        return;
    };
    let resp = reqwest::Client::new()
        .get(format!(
            "{}/api/tenant/phlax/session_workers?agent_session_id=not-a-uuid",
            server.base
        ))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_session_worker_round_trips() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED get_session_worker_round_trips");
        return;
    };
    let session_ids = seed_agent_sessions(&server.db, &["active"]).await;
    let id = seed_session_worker(
        &server.db,
        Some(session_ids[0]),
        "mcp-bash",
        "mcp_session_x",
        false,
        0,
    )
    .await;
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/api/tenant/phlax/session_workers/{id}", server.base))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("json");
    assert_eq!(body["id"], id.to_string());
    assert_eq!(body["container_name"], "mcp_session_x");
    assert!(body["reaped_at"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_session_worker_unknown_id_is_404() {
    let Some(server) = spawn_server().await else {
        eprintln!("IGNORED get_session_worker_unknown_id_is_404");
        return;
    };
    let id = Uuid::new_v4();
    let resp = reqwest::Client::new()
        .get(format!("{}/api/tenant/phlax/session_workers/{id}", server.base))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
