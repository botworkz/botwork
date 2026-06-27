//! End-to-end wire-contract tests for the secrets write surface.
//!
//! Spins up a real postgres via testcontainers (required by AppState
//! even though secrets are not stored in postgres), plus a wiremock
//! server that stands in for the secret-store backend. The wiremock
//! half doesn't need docker — all stub HTTP happens in-process.
//!
//! Fixture / docker-gate shape mirrors `tests/integration.rs`:
//! docker absence prints an IGNORED line and returns early rather
//! than failing, since the postgres container is needed for AppState.
//!
//! Test bodies do NOT log or assert on `value_b64` — the secret
//! value is opaque at this layer and must never appear in CI output.
//!
//! Phase 2 reshape (botworkz/space#311): secrets endpoints moved from
//! `/admin/api/v1/secrets[...]` to `/api/tenant/{tenant}/secrets[...]`.
//! Tenant comes from the URL path; the `x-botwork-tenant` header must
//! match the path tenant (auth-broker invariant). Missing/mismatched
//! header → 403 `cross_tenant_forbidden`. Body schema is unchanged.

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
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const POSTGRES_TAG: &str = "16-alpine";

/// Minimal fixture: just enough for AppState to be valid.
const SAMPLE_YAML: &str = r#"
tenants:
- name: phlax
  workspaces:
  - name: mcp
    plugins:
    - name: mcp-bash

plugins:
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:1.0
  egress: none
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

/// Spin postgres + migrations + a wiremock-backed secret-store client,
/// bind api on a random port.
///
/// `secret_store` is the client to inject; the caller controls whether
/// it points at a wiremock or is `disabled()`.
async fn spawn_server(secret_store: SecretStoreClient) -> Option<Server> {
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

    let raw: BootstrapConfigRaw = serde_yaml::from_str(SAMPLE_YAML).expect("bootstrap yaml parse");
    let cfg = BootstrapConfig::from_raw(raw).expect("bootstrap validate");
    apply(&db, &cfg).await.expect("bootstrap apply");

    let state = AppState {
        db: Arc::new(db),
        control_plane: ControlPlaneClient::disabled(),
        secret_store,
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

// ── create_secret ───────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_secret_happy_path() {
    let mock = MockServer::start().await;
    // Register the strict wiremock expectation only after the docker probe.
    // If docker is unreachable we intentionally return early, and mounting
    // `.expect(1)` before that point would panic at MockServer drop-time.
    let client = SecretStoreClient::with_endpoint(mock.uri());
    let Some(server) = spawn_server(client).await else {
        eprintln!("IGNORED create_secret_happy_path: docker not reachable");
        return;
    };
    Mock::given(method("POST"))
        .and(path("/secrets"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"stored": "github.com/pat", "created": true})),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/api/tenant/phlax/secrets", server.base))
        .header("x-botwork-tenant", "phlax")
        .json(&json!({
            "service": "github",
            "name": "pat",
            "kind": "token",
            "value_b64": "dG9rZW4="
        }))
        .send()
        .await
        .expect("POST");

    assert_eq!(resp.status(), StatusCode::CREATED);

    // Extract Location header before consuming the response body.
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["stored"], "github.com/pat");
    assert_eq!(body["created"], true);

    // Phase 2: Location header carries the tenant in the URL path.
    assert_eq!(location, "/api/tenant/phlax/secrets/github/pat");

    // Verify wiremock saw a request with tenant in body (unchanged
    // wire contract between api and the secret-store backend).
    let reqs = mock.received_requests().await.unwrap_or_default();
    assert_eq!(reqs.len(), 1);
    let sent: serde_json::Value =
        serde_json::from_slice(&reqs[0].body).expect("backend request body");
    assert_eq!(sent["tenant"], "phlax");
    assert_eq!(sent["service"], "github");
    assert_eq!(sent["name"], "pat");
    // value_b64 presence is verified but never printed.
    assert!(sent.get("value_b64").is_some());
}

/// Phase 2: tenant is the URL path segment; the `x-botwork-tenant`
/// header must be present AND match. A missing header on a
/// tenant-scoped endpoint returns 403 `cross_tenant_forbidden`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_secret_missing_tenant_header() {
    // No wiremock needed — auth check is api-local and no
    // request should reach the backend.
    let client = SecretStoreClient::with_endpoint("http://127.0.0.1:1");
    let Some(server) = spawn_server(client).await else {
        eprintln!("IGNORED create_secret_missing_tenant_header: docker not reachable");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("{}/api/tenant/phlax/secrets", server.base))
        // deliberately no x-botwork-tenant header
        .json(&json!({
            "service": "github",
            "name": "pat",
            "kind": "token",
            "value_b64": "dG9rZW4="
        }))
        .send()
        .await
        .expect("POST");

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "cross_tenant_forbidden");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_secret_already_exists() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/secrets"))
        .respond_with(
            ResponseTemplate::new(409)
                .set_body_json(json!({"error": "already_exists", "message": "secret already exists; use overwrite: true to replace"})),
        )
        .mount(&mock)
        .await;

    let client = SecretStoreClient::with_endpoint(mock.uri());
    let Some(server) = spawn_server(client).await else {
        eprintln!("IGNORED create_secret_already_exists: docker not reachable");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("{}/api/tenant/phlax/secrets", server.base))
        .header("x-botwork-tenant", "phlax")
        .json(&json!({
            "service": "github",
            "name": "pat",
            "kind": "token",
            "value_b64": "dG9rZW4="
        }))
        .send()
        .await
        .expect("POST");

    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "already_exists");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_secret_backend_unavailable() {
    // Stub returns 503 — same as if the backend is down.
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/secrets"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&mock)
        .await;

    let client = SecretStoreClient::with_endpoint(mock.uri());
    let Some(server) = spawn_server(client).await else {
        eprintln!("IGNORED create_secret_backend_unavailable: docker not reachable");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("{}/api/tenant/phlax/secrets", server.base))
        .header("x-botwork-tenant", "phlax")
        .json(&json!({
            "service": "github",
            "name": "pat",
            "kind": "token",
            "value_b64": "dG9rZW4="
        }))
        .send()
        .await
        .expect("POST");

    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "unavailable");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("secret-store"),
        "error message should mention secret-store"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_secret_backend_disabled() {
    // SecretStoreClient::disabled() triggers the break-glass path.
    // No docker needed since validation happens before any backend call,
    // but AppState still requires a DB so we still need postgres.
    let Some(server) = spawn_server(SecretStoreClient::disabled()).await else {
        eprintln!("IGNORED create_secret_backend_disabled: docker not reachable");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("{}/api/tenant/phlax/secrets", server.base))
        .header("x-botwork-tenant", "phlax")
        .json(&json!({
            "service": "github",
            "name": "pat",
            "kind": "token",
            "value_b64": "dG9rZW4="
        }))
        .send()
        .await
        .expect("POST");

    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "unavailable");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("break-glass"),
        "error message should mention break-glass"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_secret_bad_service_name() {
    // Validation is api-local. The backend must NOT receive a request.
    let mock = MockServer::start().await;
    // No stub mounted — any request reaching the mock would be
    // unexpected and wiremock would return 404.

    let client = SecretStoreClient::with_endpoint(mock.uri());
    let Some(server) = spawn_server(client).await else {
        eprintln!("IGNORED create_secret_bad_service_name: docker not reachable");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("{}/api/tenant/phlax/secrets", server.base))
        .header("x-botwork-tenant", "phlax")
        .json(&json!({
            // forward slash → path traversal class; require_secret_component rejects.
            "service": "bad/name",
            "name": "pat",
            "kind": "token",
            "value_b64": "dG9rZW4="
        }))
        .send()
        .await
        .expect("POST");

    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "validation_failed");

    // Backend must NOT have received a request.
    let reqs = mock.received_requests().await.unwrap_or_default();
    assert!(
        reqs.is_empty(),
        "backend should not receive a request when validation fails"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_secret_rejects_path_traversal_component() {
    let mock = MockServer::start().await;
    let client = SecretStoreClient::with_endpoint(mock.uri());
    let Some(server) = spawn_server(client).await else {
        eprintln!("IGNORED create_secret_rejects_path_traversal_component: docker not reachable");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("{}/api/tenant/phlax/secrets", server.base))
        .header("x-botwork-tenant", "phlax")
        .json(&json!({
            "service": "../etc/passwd",
            "name": "pat",
            "kind": "token",
            "value_b64": "dG9rZW4="
        }))
        .send()
        .await
        .expect("POST");

    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "validation_failed");

    let reqs = mock.received_requests().await.unwrap_or_default();
    assert!(
        reqs.is_empty(),
        "backend should not receive a request when validation fails"
    );
}

// ── delete_secret ───────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_secret_happy_path() {
    let mock = MockServer::start().await;
    // Register the strict wiremock expectation only after the docker probe.
    // If docker is unreachable we intentionally return early, and mounting
    // `.expect(1)` before that point would panic at MockServer drop-time.
    let client = SecretStoreClient::with_endpoint(mock.uri());
    let Some(server) = spawn_server(client).await else {
        eprintln!("IGNORED delete_secret_happy_path: docker not reachable");
        return;
    };
    // Wire contract with the backend is unchanged: tenant still goes as
    // a query param to the backend, regardless of how the api receives it.
    Mock::given(method("DELETE"))
        .and(path("/secrets/github/pat"))
        .and(query_param("tenant", "phlax"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&mock)
        .await;

    let resp = reqwest::Client::new()
        .delete(format!(
            "{}/api/tenant/phlax/secrets/github/pat",
            server.base
        ))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("DELETE");

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    // No body on 204.
    let text = resp.text().await.expect("text");
    assert!(text.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_secret_not_found() {
    let mock = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/secrets/github/pat"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"error": "not_found", "message": "secret not found"})),
        )
        .mount(&mock)
        .await;

    let client = SecretStoreClient::with_endpoint(mock.uri());
    let Some(server) = spawn_server(client).await else {
        eprintln!("IGNORED delete_secret_not_found: docker not reachable");
        return;
    };

    let resp = reqwest::Client::new()
        .delete(format!(
            "{}/api/tenant/phlax/secrets/github/pat",
            server.base
        ))
        .header("x-botwork-tenant", "phlax")
        .send()
        .await
        .expect("DELETE");

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "not_found");
}

/// Phase 2: a missing `x-botwork-tenant` header on the tenant-scoped
/// secrets endpoint returns 403 `cross_tenant_forbidden`, same as
/// every other tenant-scoped route.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_secret_missing_tenant_header() {
    // No backend call expected — the consistency check is first.
    let client = SecretStoreClient::with_endpoint("http://127.0.0.1:1");
    let Some(server) = spawn_server(client).await else {
        eprintln!("IGNORED delete_secret_missing_tenant_header: docker not reachable");
        return;
    };

    let resp = reqwest::Client::new()
        .delete(format!(
            "{}/api/tenant/phlax/secrets/github/pat",
            server.base
        ))
        // deliberately no x-botwork-tenant header
        .send()
        .await
        .expect("DELETE");

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["error"], "cross_tenant_forbidden");
}
