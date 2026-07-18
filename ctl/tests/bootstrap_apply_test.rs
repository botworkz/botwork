//! Wiremock-stubbed integration tests for `botctl bootstrap`.
//!
//! Spins an in-process wiremock server that stands in for `botwork-api`
//! and exercises the full `apply()` path (and the individual client
//! methods it delegates to). Every test asserts both the correct URL
//! shape and the correct auth headers for Phase 2 of space#311:
//!
//! * Admin-gated routes (`/api/tenants`, `/api/plugins`, …) must carry
//!   `x-botwork-admin: <operator>`.
//! * Tenant-scoped routes (`/api/tenant/{tenant}/…`) must carry
//!   `x-botwork-tenant: <tenant>`.
//!
//! The blocking `AdminClient` is driven from a `spawn_blocking` task so
//! we stay on the tokio multi-thread executor without deadlocking.

use botwork_ctl::bootstrap::apply::{apply, ApplyOutcome};
use botwork_ctl::bootstrap::client::AdminClient;
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const OPERATOR: &str = "bootstrap-test";
const TENANT: &str = "phlax";

// Fixed UUIDs used throughout the fixtures.
const TENANT_ID: &str = "00000000-0000-0000-0000-000000000001";
const WORKSPACE_ID: &str = "00000000-0000-0000-0000-000000000002";
const PLUGIN_ID: &str = "00000000-0000-0000-0000-000000000003";
const UPDATED_AT: &str = "2024-01-01T00:00:00Z";

// ── helpers ─────────────────────────────────────────────────────────

/// A minimal `BootstrapConfig` expressed as YAML that the test suite
/// can parse into a validated config. One plugin, one tenant, one
/// workspace, one binding.
const SAMPLE_YAML: &str = r#"
plugins:
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:1.0
  egress: none

tenants:
- name: phlax
  workspaces:
  - name: mcp
    plugins:
    - name: mcp-bash
"#;

fn parsed_config() -> botwork_api_core::config::BootstrapConfig {
    let raw: botwork_api_core::config::BootstrapConfigRaw =
        serde_yaml::from_str(SAMPLE_YAML).expect("yaml parse");
    botwork_api_core::config::BootstrapConfig::from_raw(raw).expect("validate")
}

fn client(endpoint: &str) -> AdminClient {
    AdminClient::new(endpoint, OPERATOR).expect("client")
}

// ── stub builders ───────────────────────────────────────────────────

/// Stub: `GET /api/plugins` returns an empty list.
fn stub_list_plugins_empty() -> Mock {
    Mock::given(method("GET"))
        .and(path("/api/plugins"))
        .and(header("x-botwork-admin", OPERATOR))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[],"total":0})))
}

/// Stub: `GET /api/plugins` returns one existing plugin with the same
/// fields as `SAMPLE_YAML`, so the diff is a no-op.
fn stub_list_plugins_existing() -> Mock {
    Mock::given(method("GET"))
        .and(path("/api/plugins"))
        .and(header("x-botwork-admin", OPERATOR))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "id": PLUGIN_ID,
                "name": "mcp-bash",
                "image": "ghcr.io/example/mcp-bash:1.0",
                "port": 8000,
                "path": "/",
                "upstream_auth": "none",
                "env": [],
                "resources": null,
                "egress": {"mode":"none"},
                "updated_at": UPDATED_AT
            }],
            "total": 1
        })))
}

/// Stub: `POST /api/plugins` creates the plugin and returns it.
fn stub_create_plugin() -> Mock {
    Mock::given(method("POST"))
        .and(path("/api/plugins"))
        .and(header("x-botwork-admin", OPERATOR))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": PLUGIN_ID,
            "name": "mcp-bash",
            "image": "ghcr.io/example/mcp-bash:1.0",
            "port": 8000,
            "path": "/",
            "upstream_auth": "none",
            "env": [],
            "resources": null,
            "egress": {"mode":"none"},
            "updated_at": UPDATED_AT
        })))
}

/// Stub: `GET /api/tenants` returns an empty list.
fn stub_list_tenants_empty() -> Mock {
    Mock::given(method("GET"))
        .and(path("/api/tenants"))
        .and(header("x-botwork-admin", OPERATOR))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[],"total":0})))
}

/// Stub: `GET /api/tenants` returns one existing tenant.
fn stub_list_tenants_existing() -> Mock {
    Mock::given(method("GET"))
        .and(path("/api/tenants"))
        .and(header("x-botwork-admin", OPERATOR))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "id": TENANT_ID,
                "name": TENANT,
                "updated_at": UPDATED_AT
            }],
            "total": 1
        })))
}

/// Stub: `POST /api/tenants` creates the tenant and returns it.
fn stub_create_tenant() -> Mock {
    Mock::given(method("POST"))
        .and(path("/api/tenants"))
        .and(header("x-botwork-admin", OPERATOR))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": TENANT_ID,
            "name": TENANT,
            "updated_at": UPDATED_AT
        })))
}

/// Stub: `GET /api/tenant/phlax/workspaces` returns an empty list.
fn stub_list_workspaces_empty() -> Mock {
    Mock::given(method("GET"))
        .and(path(format!("/api/tenant/{TENANT}/workspaces")))
        .and(header("x-botwork-tenant", TENANT))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[],"total":0})))
}

/// Stub: `GET /api/tenant/phlax/workspaces` returns one existing workspace.
fn stub_list_workspaces_existing() -> Mock {
    Mock::given(method("GET"))
        .and(path(format!("/api/tenant/{TENANT}/workspaces")))
        .and(header("x-botwork-tenant", TENANT))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "id": WORKSPACE_ID,
                "tenant_id": TENANT_ID,
                "name": "mcp",
                "updated_at": UPDATED_AT
            }],
            "total": 1
        })))
}

/// Stub: `POST /api/tenant/phlax/workspaces` creates workspace.
fn stub_create_workspace() -> Mock {
    Mock::given(method("POST"))
        .and(path(format!("/api/tenant/{TENANT}/workspaces")))
        .and(header("x-botwork-tenant", TENANT))
        .and(header("x-botwork-admin", OPERATOR))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": WORKSPACE_ID,
            "tenant_id": TENANT_ID,
            "name": "mcp",
            "updated_at": UPDATED_AT
        })))
}

/// Stub: `GET /api/tenant/phlax/workspace_plugins?workspace_id=...` returns empty.
fn stub_list_workspace_plugins_empty() -> Mock {
    Mock::given(method("GET"))
        .and(path(format!("/api/tenant/{TENANT}/workspace_plugins")))
        .and(header("x-botwork-tenant", TENANT))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[],"total":0})))
}

/// Stub: `GET /api/tenant/phlax/workspace_plugins?workspace_id=...` returns one binding.
fn stub_list_workspace_plugins_existing() -> Mock {
    Mock::given(method("GET"))
        .and(path(format!("/api/tenant/{TENANT}/workspace_plugins")))
        .and(header("x-botwork-tenant", TENANT))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "workspace_id": WORKSPACE_ID,
                "plugin_id": PLUGIN_ID,
                "config": null,
                "updated_at": UPDATED_AT
            }],
            "total": 1
        })))
}

/// Stub: `POST /api/tenant/phlax/workspace_plugins` creates a binding.
fn stub_create_workspace_plugin() -> Mock {
    Mock::given(method("POST"))
        .and(path(format!("/api/tenant/{TENANT}/workspace_plugins")))
        .and(header("x-botwork-tenant", TENANT))
        .and(header("x-botwork-admin", OPERATOR))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "workspace_id": WORKSPACE_ID,
            "plugin_id": PLUGIN_ID,
            "config": null,
            "updated_at": UPDATED_AT
        })))
}

// ── full apply tests ─────────────────────────────────────────────────

/// Full apply from empty state: every entity must be created once.
#[tokio::test]
async fn apply_creates_all_from_empty_state() {
    let server = MockServer::start().await;

    stub_list_plugins_empty().expect(1).mount(&server).await;
    stub_create_plugin().expect(1).mount(&server).await;
    stub_list_tenants_empty().expect(1).mount(&server).await;
    stub_create_tenant().expect(1).mount(&server).await;
    stub_list_workspaces_empty().expect(1).mount(&server).await;
    stub_create_workspace().expect(1).mount(&server).await;
    stub_list_workspace_plugins_empty()
        .expect(1)
        .mount(&server)
        .await;
    stub_create_workspace_plugin()
        .expect(1)
        .mount(&server)
        .await;

    let endpoint = server.uri();
    let config = parsed_config();
    let outcome: ApplyOutcome = tokio::task::spawn_blocking(move || {
        let c = client(&endpoint);
        apply(&c, &config, false).expect("apply ok")
    })
    .await
    .expect("join");

    assert_eq!(outcome.plugins_created, 1);
    assert_eq!(outcome.plugins_updated, 0);
    assert_eq!(outcome.tenants_created, 1);
    assert_eq!(outcome.workspaces_created, 1);
    assert_eq!(outcome.bindings_created, 1);
    assert_eq!(outcome.bindings_updated, 0);
    // wiremock verifies `.expect(N)` counts at drop
}

/// Re-run with matching state is a no-op: all lists return current
/// rows, no POSTs or PUTs are issued.
#[tokio::test]
async fn apply_is_idempotent_when_state_already_matches() {
    let server = MockServer::start().await;

    stub_list_plugins_existing().expect(1).mount(&server).await;
    stub_list_tenants_existing().expect(1).mount(&server).await;
    stub_list_workspaces_existing()
        .expect(1)
        .mount(&server)
        .await;
    stub_list_workspace_plugins_existing()
        .expect(1)
        .mount(&server)
        .await;
    // No POST/PUT mocks: wiremock returns 404 for any unexpected
    // write, which would panic the apply() call.

    let endpoint = server.uri();
    let config = parsed_config();
    let outcome = tokio::task::spawn_blocking(move || {
        let c = client(&endpoint);
        apply(&c, &config, false).expect("apply ok")
    })
    .await
    .expect("join");

    assert_eq!(outcome.plugins_created, 0);
    assert_eq!(outcome.plugins_updated, 0);
    assert_eq!(outcome.tenants_created, 0);
    assert_eq!(outcome.workspaces_created, 0);
    assert_eq!(outcome.bindings_created, 0);
    assert_eq!(outcome.bindings_updated, 0);
}

/// Dry run: only the read-side GETs fire, no writes regardless of
/// whether state is empty.
#[tokio::test]
async fn apply_dry_run_issues_no_writes() {
    let server = MockServer::start().await;

    stub_list_plugins_empty().expect(1).mount(&server).await;
    stub_list_tenants_empty().expect(1).mount(&server).await;
    // Workspace and binding lists are skipped for nil-id tenants in
    // dry-run mode (the apply short-circuits on Uuid::nil()).

    let endpoint = server.uri();
    let config = parsed_config();
    let outcome = tokio::task::spawn_blocking(move || {
        let c = client(&endpoint);
        apply(&c, &config, true).expect("apply ok")
    })
    .await
    .expect("join");

    assert_eq!(
        outcome.plugins_created, 1,
        "dry-run still counts planned creates"
    );
    assert_eq!(
        outcome.tenants_created, 1,
        "dry-run still counts planned creates"
    );
    // No writes were issued: the server has no POST/PUT stubs, and
    // wiremock would 404 any write that slipped through.
}

// ── URL and header contract unit tests ──────────────────────────────

/// `GET /api/plugins` carries `x-botwork-admin` and hits the Phase 2
/// URL, not the retired `/admin/api/v1/plugins`.
#[tokio::test]
async fn list_plugins_uses_admin_url_with_admin_header() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/plugins"))
        .and(header("x-botwork-admin", OPERATOR))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[],"total":0})))
        .expect(1)
        .mount(&server)
        .await;

    let endpoint = server.uri();
    tokio::task::spawn_blocking(move || client(&endpoint).list_plugins().expect("ok"))
        .await
        .expect("join");
}

/// `GET /api/tenants` carries `x-botwork-admin`.
#[tokio::test]
async fn list_tenants_uses_admin_url_with_admin_header() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/tenants"))
        .and(header("x-botwork-admin", OPERATOR))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[],"total":0})))
        .expect(1)
        .mount(&server)
        .await;

    let endpoint = server.uri();
    tokio::task::spawn_blocking(move || client(&endpoint).list_tenants().expect("ok"))
        .await
        .expect("join");
}

/// `GET /api/tenant/{tenant}/workspaces` carries `x-botwork-tenant`
/// matching the path segment.
#[tokio::test]
async fn list_workspaces_uses_tenant_url_with_tenant_header() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path(format!("/api/tenant/{TENANT}/workspaces")))
        .and(header("x-botwork-tenant", TENANT))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[],"total":0})))
        .expect(1)
        .mount(&server)
        .await;

    let endpoint = server.uri();
    tokio::task::spawn_blocking(move || client(&endpoint).list_workspaces(TENANT).expect("ok"))
        .await
        .expect("join");
}

/// `GET /api/tenant/{tenant}/workspace_plugins?workspace_id=…` carries
/// `x-botwork-tenant` and includes the query param.
#[tokio::test]
async fn list_workspace_plugins_uses_tenant_url_with_query_param() {
    let server = MockServer::start().await;

    let workspace_uuid: uuid::Uuid = WORKSPACE_ID.parse().unwrap();

    Mock::given(method("GET"))
        .and(path(format!("/api/tenant/{TENANT}/workspace_plugins")))
        .and(query_param("workspace_id", WORKSPACE_ID))
        .and(header("x-botwork-tenant", TENANT))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[],"total":0})))
        .expect(1)
        .mount(&server)
        .await;

    let endpoint = server.uri();
    tokio::task::spawn_blocking(move || {
        client(&endpoint)
            .list_workspace_plugins(TENANT, workspace_uuid)
            .expect("ok")
    })
    .await
    .expect("join");
}

/// `POST /api/tenant/{tenant}/workspaces` body no longer contains
/// `tenant_id` — the tenant is path-borne in Phase 2.
#[tokio::test]
async fn create_workspace_body_has_no_tenant_id_field() {
    let server = MockServer::start().await;

    // Verify the body contains `name` but NOT `tenant_id`. We do this
    // by registering a mock that accepts the correct body shape and
    // asserting it fires exactly once. Since `CreateWorkspace` no
    // longer has a `tenant_id` field, the serialized body is simply
    // `{"name":"mcp"}`; `body_partial_json` confirms `name` is there,
    // and the type system guarantees `tenant_id` can't appear.
    Mock::given(method("POST"))
        .and(path(format!("/api/tenant/{TENANT}/workspaces")))
        .and(header("x-botwork-tenant", TENANT))
        .and(header("x-botwork-admin", OPERATOR))
        .and(wiremock::matchers::body_partial_json(
            json!({"name": "mcp"}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": WORKSPACE_ID,
            "tenant_id": TENANT_ID,
            "name": "mcp",
            "updated_at": UPDATED_AT
        })))
        .expect(1)
        .mount(&server)
        .await;

    let endpoint = server.uri();
    tokio::task::spawn_blocking(move || {
        use botwork_ctl::bootstrap::client::CreateWorkspace;
        client(&endpoint)
            .create_workspace(TENANT, &CreateWorkspace { name: "mcp" })
            .expect("ok")
    })
    .await
    .expect("join");
}

/// `POST /api/tenant/{tenant}/workspace_plugins` carries both
/// `x-botwork-tenant` and `x-botwork-admin` (the latter for audit log).
#[tokio::test]
async fn create_workspace_plugin_carries_both_headers() {
    let server = MockServer::start().await;

    let workspace_uuid: uuid::Uuid = WORKSPACE_ID.parse().unwrap();
    let plugin_uuid: uuid::Uuid = PLUGIN_ID.parse().unwrap();

    Mock::given(method("POST"))
        .and(path(format!("/api/tenant/{TENANT}/workspace_plugins")))
        .and(header("x-botwork-tenant", TENANT))
        .and(header("x-botwork-admin", OPERATOR))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "workspace_id": WORKSPACE_ID,
            "plugin_id": PLUGIN_ID,
            "config": null,
            "updated_at": UPDATED_AT
        })))
        .expect(1)
        .mount(&server)
        .await;

    let endpoint = server.uri();
    tokio::task::spawn_blocking(move || {
        use botwork_ctl::bootstrap::client::CreateWorkspacePlugin;
        client(&endpoint)
            .create_workspace_plugin(
                TENANT,
                &CreateWorkspacePlugin {
                    workspace_id: workspace_uuid,
                    plugin_id: plugin_uuid,
                    config: None,
                },
            )
            .expect("ok")
    })
    .await
    .expect("join");
}

/// A 404 from the old `/admin/api/v1/…` path surfaces as a `ClientError::Http`
/// (status 404), not a transport error — confirming that if someone
/// accidentally points the client at the retired URL space the error
/// is clear rather than silent.
#[tokio::test]
async fn old_url_prefix_returns_http_error_not_transport() {
    use botwork_ctl::bootstrap::client::ClientError;

    let server = MockServer::start().await;
    // No mocks: wiremock returns 404 for every request by default.

    let endpoint = server.uri();
    let err = tokio::task::spawn_blocking(move || client(&endpoint).list_plugins())
        .await
        .expect("join")
        .expect_err("should fail with 404");

    match err {
        ClientError::Http { status, .. } => assert_eq!(status, 404),
        other => panic!("expected Http 404, got {other:?}"),
    }
}
