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
    body: Option<Json<ResolveRequest>>,
) -> Response {
    let Some(Json(payload)) = body else {
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
