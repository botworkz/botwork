//! HTTP handler for `POST /resolve`.
//!
//! Wire contract (post-PR2):
//!
//! Request:
//!     `{ "tenant": "<tenant>", "workspace": "<ws>", "plugin": "<name>" }`
//!
//! Response 200:
//!     `{ "image", "port", "path", "upstream_auth",
//!        "resources": { "cpus"?, "memory"?, "pids"? },
//!        "env": [ { "name", "value" }, … ],
//!        "config_blob"?: "<compact JSON string>",
//!        "egress": { … } }`
//!
//! Errors share a single envelope:
//!     `{ "error": "<machine code>", "message": "<human detail>" }`
//!
//! The handler does NO content validation on the row — bootstrap is
//! the gate. We only validate the *request* fields (the three names)
//! so a malformed call produces a clean 400 rather than a SQL injection
//! attempt's-worth of fallout. session-broker is the only producer in
//! v0; the regex matches the rule it generates by.

use std::sync::Arc;
use std::sync::OnceLock;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use regex::Regex;
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, JoinType, QueryFilter, QuerySelect, RelationTrait,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use botwork_entity::{plugin, tenant, workspace, workspace_plugin};

const PREFIX: &str = "[config-broker]";

/// Tenant / workspace / plugin name shape — `[a-z][a-z0-9-]{0,30}`.
/// Matches what bootstrap enforces on the write side and what
/// session-broker generates by.
const NAME_RE: &str = r"^[a-z][a-z0-9-]{0,30}$";

fn name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(NAME_RE).expect("valid name regex"))
}

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<DatabaseConnection>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResolveRequest {
    tenant: Option<String>,
    workspace: Option<String>,
    plugin: Option<String>,
}

#[derive(Debug, Serialize)]
struct EnvEntry {
    name: String,
    value: String,
}

#[derive(Debug, Serialize, Default)]
struct ResourcesView {
    #[serde(skip_serializing_if = "Option::is_none")]
    cpus: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pids: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ResolveResponse {
    image: String,
    port: u16,
    path: String,
    upstream_auth: String,
    #[serde(default)]
    resources: ResourcesView,
    env: Vec<EnvEntry>,
    /// Compact JSON. Omitted (not `""`/`{}`) when there's no per-
    /// binding `config:`.
    #[serde(skip_serializing_if = "Option::is_none")]
    config_blob: Option<String>,
    /// Normalised egress wire shape as written to the DB by bootstrap.
    egress: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
}

fn error_response(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    let body = ErrorBody {
        error: code,
        message: message.into(),
    };
    (status, Json(body)).into_response()
}

pub(crate) async fn resolve(
    State(state): State<AppState>,
    body: Result<Json<ResolveRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        warn!("{PREFIX} resolve: invalid_request — missing or unparseable JSON body");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "request body must be a JSON object",
        );
    };

    let Some(tenant) = payload.tenant.as_deref() else {
        warn!("{PREFIX} resolve: invalid_request — missing 'tenant'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing required field 'tenant'",
        );
    };
    let Some(workspace_name) = payload.workspace.as_deref() else {
        warn!("{PREFIX} resolve: invalid_request — missing 'workspace'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing required field 'workspace'",
        );
    };
    let Some(plugin_name) = payload.plugin.as_deref() else {
        warn!("{PREFIX} resolve: invalid_request — missing 'plugin'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing required field 'plugin'",
        );
    };

    if !name_re().is_match(tenant) {
        warn!("{PREFIX} resolve: invalid_request — bad tenant '{tenant}'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("invalid tenant '{tenant}': must match {NAME_RE}"),
        );
    }
    if !name_re().is_match(workspace_name) {
        warn!("{PREFIX} resolve: invalid_workspace — bad workspace '{workspace_name}'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_workspace",
            format!("invalid workspace '{workspace_name}': must match {NAME_RE}"),
        );
    }
    if !name_re().is_match(plugin_name) {
        warn!("{PREFIX} resolve: invalid_request — bad plugin name '{plugin_name}'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("invalid plugin '{plugin_name}': must match {NAME_RE}"),
        );
    }

    match lookup(&state.db, tenant, workspace_name, plugin_name).await {
        Ok(Some(row)) => {
            info!(
                "{PREFIX} resolve: ok tenant={tenant} workspace={workspace_name} plugin={plugin_name}"
            );
            (StatusCode::OK, Json(row)).into_response()
        }
        Ok(None) => {
            warn!(
                "{PREFIX} resolve: unknown_plugin tenant={tenant} workspace={workspace_name} plugin={plugin_name}"
            );
            error_response(
                StatusCode::NOT_FOUND,
                "unknown_plugin",
                format!(
                    "no binding for tenant '{tenant}' workspace '{workspace_name}' plugin '{plugin_name}'"
                ),
            )
        }
        Err(err) => {
            warn!("{PREFIX} resolve: internal — {err}");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                format!("resolve failed: {err}"),
            )
        }
    }
}

/// Resolve a `(tenant, workspace, plugin)` triple to the wire-shape
/// descriptor. Returns `Ok(None)` for "no such binding"; everything
/// else is a real DB error.
///
/// Implementation note: we run two queries (plugin + binding) rather
/// than a single JOIN-with-SELECT-cols because SeaORM's join shape
/// here would need a custom `FromQueryResult` to capture columns
/// from both sides. Two queries are simpler, hit the same indexes,
/// and the latency-sensitive hot path doesn't measurably care in
/// v0. Revisit if the request rate ever justifies the optimisation.
async fn lookup(
    db: &DatabaseConnection,
    tenant_name: &str,
    workspace_name: &str,
    plugin_name: &str,
) -> Result<Option<ResolveResponse>, sea_orm::DbErr> {
    // Find the binding row by walking tenant -> workspace -> plugin.
    let binding: Option<(workspace_plugin::Model, Option<plugin::Model>)> =
        workspace_plugin::Entity::find()
            .find_also_related(plugin::Entity)
            .join(
                JoinType::InnerJoin,
                workspace_plugin::Relation::Workspace.def(),
            )
            .join(JoinType::InnerJoin, workspace::Relation::Tenant.def())
            .filter(plugin::Column::Name.eq(plugin_name))
            .filter(workspace::Column::Name.eq(workspace_name))
            .filter(tenant::Column::Name.eq(tenant_name))
            .one(db)
            .await?;

    let Some((binding_row, plugin_row)) = binding else {
        return Ok(None);
    };
    let Some(plugin_row) = plugin_row else {
        // Join returned no plugin row — the FK guarantees this can't
        // happen, but treat as "no binding" rather than panic so a
        // future schema change can't crash the broker.
        return Ok(None);
    };

    Ok(Some(render(&plugin_row, &binding_row)?))
}

fn render(
    plugin_row: &plugin::Model,
    binding_row: &workspace_plugin::Model,
) -> Result<ResolveResponse, sea_orm::DbErr> {
    let port = u16::try_from(plugin_row.port).map_err(|_| {
        sea_orm::DbErr::Custom(format!(
            "plugin '{}' has out-of-range port {} in DB; bootstrap should have constrained 1..=65535",
            plugin_row.name, plugin_row.port,
        ))
    })?;

    // env is `jsonb` array of {name, value}. Already in wire shape
    // courtesy of bootstrap; we just decode into the typed view.
    let env = match plugin_row.env.as_array() {
        Some(arr) => arr
            .iter()
            .filter_map(|entry| {
                let name = entry.get("name")?.as_str()?.to_string();
                let value = entry.get("value")?.as_str()?.to_string();
                Some(EnvEntry { name, value })
            })
            .collect(),
        None => Vec::new(),
    };

    // resources is `jsonb` `{cpus?, memory?, pids?}` or NULL.
    let resources = match &plugin_row.resources {
        None => ResourcesView::default(),
        Some(v) => ResourcesView {
            cpus: v.get("cpus").and_then(|c| c.as_str()).map(String::from),
            memory: v.get("memory").and_then(|c| c.as_str()).map(String::from),
            pids: v
                .get("pids")
                .and_then(|c| c.as_u64())
                .and_then(|n| u32::try_from(n).ok()),
        },
    };

    // Per-binding `config` -> compact JSON string for the env var.
    // Treat empty object the same as absent; bootstrap normalises this
    // away on the write side but be belt-and-braces.
    let config_blob = match &binding_row.config {
        None => None,
        Some(v) if matches!(v.as_object(), Some(m) if m.is_empty()) => None,
        Some(v) => Some(
            serde_json::to_string(v)
                .map_err(|e| sea_orm::DbErr::Custom(format!("config re-serialise: {e}")))?,
        ),
    };

    Ok(ResolveResponse {
        image: plugin_row.image.clone(),
        port,
        path: plugin_row.path.clone(),
        upstream_auth: plugin_row.upstream_auth.clone(),
        resources,
        env,
        config_blob,
        egress: plugin_row.egress.clone(),
    })
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/resolve", post(resolve))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use http::{Request, StatusCode};
    use sea_orm::{DatabaseBackend, DbErr, MockDatabase};
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn resolve_request(tenant: &str, workspace: &str, plugin: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/resolve")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "tenant": tenant,
                    "workspace": workspace,
                    "plugin": plugin,
                }))
                .expect("json body"),
            ))
            .expect("request")
    }

    fn resolve_request_partial(json: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/resolve")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&json).expect("json body")))
            .expect("request")
    }

    fn empty_state() -> AppState {
        crate::test_support::app_state_with_mock_db(MockDatabase::new(DatabaseBackend::Postgres))
    }

    fn now_binding(plugin_id: Uuid, workspace_id: Uuid) -> workspace_plugin::Model {
        let now = chrono::Utc::now();
        workspace_plugin::Model {
            workspace_id,
            plugin_id,
            config: Some(serde_json::json!({ "url": "https://example.com" })),
            created_at: now,
            updated_at: now,
        }
    }

    fn now_plugin(plugin_id: Uuid) -> plugin::Model {
        let now = chrono::Utc::now();
        plugin::Model {
            id: plugin_id,
            name: "mcp-fetch".to_string(),
            image: "ghcr.io/example/mcp-fetch:1.0".to_string(),
            port: 8001,
            path: "/mcp".to_string(),
            upstream_auth: "none".to_string(),
            env: serde_json::json!([{ "name": "LOG_LEVEL", "value": "info" }]),
            resources: Some(serde_json::json!({ "memory": "4g" })),
            egress: serde_json::json!({ "mode": "none" }),
            created_at: now,
            updated_at: now,
            current_facet_id: None,
        }
    }

    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        serde_json::from_slice(&bytes).expect("json body")
    }

    // ── existing tests ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn resolve_uses_mock_database_results() {
        let plugin_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let binding = now_binding(plugin_id, workspace_id);
        let plugin = now_plugin(plugin_id);
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![(binding, Some(plugin))]]),
        );
        let app = build_router(state);

        let response = app
            .oneshot(resolve_request("phlax", "mcp", "mcp-fetch"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["image"], "ghcr.io/example/mcp-fetch:1.0");
        assert_eq!(json["port"], 8001);
        assert_eq!(json["env"][0]["name"], "LOG_LEVEL");
    }

    #[tokio::test]
    async fn resolve_maps_db_error_to_internal_response() {
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_errors([DbErr::Custom("boom".to_string())]),
        );
        let app = build_router(state);

        let response = app
            .oneshot(resolve_request("phlax", "mcp", "mcp-fetch"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let json = json_body(response).await;
        assert_eq!(json["error"], "internal");
    }

    // ── bad-input (pre-DB) tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn resolve_rejects_missing_json_body() {
        let app = build_router(empty_state());
        let request = Request::builder()
            .method("POST")
            .uri("/resolve")
            .body(Body::empty())
            .expect("request");

        let response = app.oneshot(request).await.expect("response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = json_body(response).await;
        assert_eq!(json["error"], "invalid_request");
    }

    #[tokio::test]
    async fn resolve_rejects_missing_tenant() {
        let app = build_router(empty_state());
        let request = resolve_request_partial(serde_json::json!({
            "workspace": "mcp",
            "plugin": "mcp-fetch",
        }));

        let response = app.oneshot(request).await.expect("response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = json_body(response).await;
        assert_eq!(json["error"], "invalid_request");
        assert!(
            json["message"].as_str().unwrap_or("").contains("tenant"),
            "message should mention 'tenant'"
        );
    }

    #[tokio::test]
    async fn resolve_rejects_missing_workspace() {
        let app = build_router(empty_state());
        let request = resolve_request_partial(serde_json::json!({
            "tenant": "phlax",
            "plugin": "mcp-fetch",
        }));

        let response = app.oneshot(request).await.expect("response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = json_body(response).await;
        assert_eq!(json["error"], "invalid_request");
        assert!(
            json["message"].as_str().unwrap_or("").contains("workspace"),
            "message should mention 'workspace'"
        );
    }

    #[tokio::test]
    async fn resolve_rejects_missing_plugin() {
        let app = build_router(empty_state());
        let request = resolve_request_partial(serde_json::json!({
            "tenant": "phlax",
            "workspace": "mcp",
        }));

        let response = app.oneshot(request).await.expect("response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = json_body(response).await;
        assert_eq!(json["error"], "invalid_request");
        assert!(
            json["message"].as_str().unwrap_or("").contains("plugin"),
            "message should mention 'plugin'"
        );
    }

    #[tokio::test]
    async fn resolve_rejects_invalid_tenant_shape() {
        let app = build_router(empty_state());

        let response = app
            .oneshot(resolve_request("UPPER_CASE", "mcp", "mcp-fetch"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = json_body(response).await;
        assert_eq!(json["error"], "invalid_request");
        assert!(
            json["message"].as_str().unwrap_or("").contains("tenant"),
            "message should mention 'tenant'"
        );
    }

    #[tokio::test]
    async fn resolve_rejects_invalid_workspace_shape() {
        let app = build_router(empty_state());

        let response = app
            .oneshot(resolve_request("phlax", "bad workspace", "mcp-fetch"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = json_body(response).await;
        assert_eq!(json["error"], "invalid_workspace");
        assert!(
            json["message"].as_str().unwrap_or("").contains("workspace"),
            "message should mention 'workspace'"
        );
    }

    #[tokio::test]
    async fn resolve_rejects_invalid_plugin_shape() {
        let app = build_router(empty_state());

        let response = app
            .oneshot(resolve_request("phlax", "mcp", "1starts-with-digit"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = json_body(response).await;
        assert_eq!(json["error"], "invalid_request");
        assert!(
            json["message"].as_str().unwrap_or("").contains("plugin"),
            "message should mention 'plugin'"
        );
    }

    // ── not-found paths ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn resolve_returns_not_found_for_empty_result() {
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres).append_query_results([
                vec![] as Vec<(workspace_plugin::Model, Option<plugin::Model>)>
            ]),
        );
        let app = build_router(state);

        let response = app
            .oneshot(resolve_request("phlax", "mcp", "mcp-fetch"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let json = json_body(response).await;
        assert_eq!(json["error"], "unknown_plugin");
    }

    #[tokio::test]
    async fn resolve_returns_not_found_when_plugin_row_absent() {
        let plugin_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let binding = now_binding(plugin_id, workspace_id);
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![(binding, None::<plugin::Model>)]]),
        );
        let app = build_router(state);

        let response = app
            .oneshot(resolve_request("phlax", "mcp", "mcp-fetch"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let json = json_body(response).await;
        assert_eq!(json["error"], "unknown_plugin");
    }

    // ── response-shape tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn resolve_omits_config_blob_when_config_is_null() {
        let now = chrono::Utc::now();
        let plugin_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let binding = workspace_plugin::Model {
            workspace_id,
            plugin_id,
            config: None,
            created_at: now,
            updated_at: now,
        };
        let plugin = now_plugin(plugin_id);
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![(binding, Some(plugin))]]),
        );
        let app = build_router(state);

        let response = app
            .oneshot(resolve_request("phlax", "mcp", "mcp-fetch"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert!(
            json.get("config_blob").is_none(),
            "config_blob should be absent when config is null"
        );
    }

    #[tokio::test]
    async fn resolve_omits_config_blob_when_config_is_empty_object() {
        let now = chrono::Utc::now();
        let plugin_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let binding = workspace_plugin::Model {
            workspace_id,
            plugin_id,
            config: Some(serde_json::json!({})),
            created_at: now,
            updated_at: now,
        };
        let plugin = now_plugin(plugin_id);
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![(binding, Some(plugin))]]),
        );
        let app = build_router(state);

        let response = app
            .oneshot(resolve_request("phlax", "mcp", "mcp-fetch"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert!(
            json.get("config_blob").is_none(),
            "config_blob should be absent for empty config object"
        );
    }

    #[tokio::test]
    async fn resolve_includes_all_resource_fields() {
        let plugin_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let binding = now_binding(plugin_id, workspace_id);
        let plugin = plugin::Model {
            resources: Some(serde_json::json!({ "cpus": "0.5", "memory": "256m", "pids": 64 })),
            ..now_plugin(plugin_id)
        };
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![(binding, Some(plugin))]]),
        );
        let app = build_router(state);

        let response = app
            .oneshot(resolve_request("phlax", "mcp", "mcp-fetch"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["resources"]["cpus"], "0.5");
        assert_eq!(json["resources"]["memory"], "256m");
        assert_eq!(json["resources"]["pids"], 64);
    }

    #[tokio::test]
    async fn resolve_omits_null_resource_fields() {
        let plugin_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let binding = now_binding(plugin_id, workspace_id);
        let plugin = plugin::Model {
            resources: None,
            ..now_plugin(plugin_id)
        };
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![(binding, Some(plugin))]]),
        );
        let app = build_router(state);

        let response = app
            .oneshot(resolve_request("phlax", "mcp", "mcp-fetch"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        // resources is present as an object but all optional fields are absent
        assert!(json["resources"].is_object());
        assert!(json["resources"].get("cpus").is_none());
        assert!(json["resources"].get("memory").is_none());
        assert!(json["resources"].get("pids").is_none());
    }

    #[tokio::test]
    async fn resolve_env_skips_malformed_entries() {
        let plugin_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let binding = now_binding(plugin_id, workspace_id);
        // Mix valid entries with entries that are missing name/value fields.
        let plugin = plugin::Model {
            env: serde_json::json!([
                { "name": "GOOD", "value": "yes" },
                { "name": "NO_VALUE" },
                "not-an-object",
                { "value": "no-name" },
                { "name": "ALSO_GOOD", "value": "also-yes" },
            ]),
            ..now_plugin(plugin_id)
        };
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![(binding, Some(plugin))]]),
        );
        let app = build_router(state);

        let response = app
            .oneshot(resolve_request("phlax", "mcp", "mcp-fetch"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        let env = json["env"].as_array().expect("env array");
        assert_eq!(
            env.len(),
            2,
            "only the two well-formed entries should survive"
        );
        assert_eq!(env[0]["name"], "GOOD");
        assert_eq!(env[1]["name"], "ALSO_GOOD");
    }

    #[tokio::test]
    async fn resolve_maps_out_of_range_port_to_internal_error() {
        let plugin_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let binding = now_binding(plugin_id, workspace_id);
        let plugin = plugin::Model {
            port: 99_999, // out of u16 range → render() returns DbErr::Custom
            ..now_plugin(plugin_id)
        };
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![(binding, Some(plugin))]]),
        );
        let app = build_router(state);

        let response = app
            .oneshot(resolve_request("phlax", "mcp", "mcp-fetch"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let json = json_body(response).await;
        assert_eq!(json["error"], "internal");
    }

    #[tokio::test]
    async fn build_router_rejects_wrong_method() {
        let app = build_router(empty_state());
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/resolve")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
