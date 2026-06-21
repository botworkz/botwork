//! Read-side handlers: list + by-id over every entity.
//!
//! Wire shapes:
//!
//! * **List** — `GET /admin/api/v1/<entity>` returns
//!   `{ "items": [...], "total": N }`. The wrapping struct is
//!   deliberate: it lets pagination land (`?limit=&offset=`,
//!   `next_cursor`) as a pure-additive change. Naive `[...]` would
//!   force a wire break.
//! * **By-id** — `GET /admin/api/v1/<entity>/{id}` returns the
//!   entity model serialised verbatim. SeaORM's `DeriveEntityModel`
//!   gives us `Serialize` for free; admin-api does NOT introduce a
//!   separate DTO in v0.
//!
//! Filters on the list endpoints are intentionally minimal:
//!
//! * `workspaces` accepts `?tenant_id=<uuid>`,
//! * `workspace_plugins` accepts `?workspace_id=<uuid>` and
//!   `?plugin_id=<uuid>` (combinable).
//!
//! tenant + plugin have no filters today: there are <10 rows of
//! each in any realistic deployment.
//!
//! Composite-PK route convention: `workspace_plugin` is addressed as
//! `/{workspace_id}/{plugin_id}` (two path segments). The plugin/
//! tenant URLs use a single `{id}` segment so the by-id pattern is
//! distinguishable at the router level.

use std::str::FromStr;

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use botwork_entity::{plugin, tenant, workspace, workspace_plugin};

use crate::handler::{bad_request, ApiError, ApiErrorExt, AppState};

/// Wire-shape for every list endpoint. `items` is serialised
/// verbatim from the entity model; `total` is the row count after
/// filtering (NOT the unfiltered table size).
#[derive(Debug, Serialize)]
struct ListResponse<T> {
    items: Vec<T>,
    total: usize,
}

impl<T> ListResponse<T> {
    fn from_vec(items: Vec<T>) -> Self {
        let total = items.len();
        Self { items, total }
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/v1/tenants", get(list_tenants))
        .route("/admin/api/v1/tenants/:id", get(get_tenant))
        .route("/admin/api/v1/workspaces", get(list_workspaces))
        .route("/admin/api/v1/workspaces/:id", get(get_workspace))
        .route("/admin/api/v1/plugins", get(list_plugins))
        .route("/admin/api/v1/plugins/:id", get(get_plugin))
        .route(
            "/admin/api/v1/workspace_plugins",
            get(list_workspace_plugins),
        )
        .route(
            "/admin/api/v1/workspace_plugins/:workspace_id/:plugin_id",
            get(get_workspace_plugin),
        )
}

// ── tenant ─────────────────────────────────────────────────────────

async fn list_tenants(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    // Order by name so the response is deterministic across calls —
    // doesn't matter much at 4 rows, but it's the right default for
    // any list endpoint.
    let items = tenant::Entity::find()
        .order_by_asc(tenant::Column::Name)
        .all(state.db.as_ref())
        .await?;
    Ok(Json(ListResponse::from_vec(items)))
}

async fn get_tenant(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let id = Uuid::from_str(&id).map_err(|err| bad_request("invalid tenant id", err))?;
    let row = tenant::Entity::find_by_id(id)
        .one(state.db.as_ref())
        .await?
        .or_not_found("tenant", format!("no tenant with id {id}"))?;
    Ok(Json(row))
}

// ── workspace ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WorkspaceListQuery {
    /// Optional filter by parent tenant. Wire shape is `?tenant_id=<uuid>`.
    tenant_id: Option<String>,
}

async fn list_workspaces(
    State(state): State<AppState>,
    Query(params): Query<WorkspaceListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let mut query = workspace::Entity::find();
    if let Some(raw) = params.tenant_id {
        let tenant_id = Uuid::from_str(&raw)
            .map_err(|err| bad_request("invalid tenant_id query param", err))?;
        query = query.filter(workspace::Column::TenantId.eq(tenant_id));
    }
    // Order by (tenant_id, name): puts each tenant's workspaces
    // together in name order, which matches what an operator would
    // expect from a grouped list view.
    let items = query
        .order_by_asc(workspace::Column::TenantId)
        .order_by_asc(workspace::Column::Name)
        .all(state.db.as_ref())
        .await?;
    Ok(Json(ListResponse::from_vec(items)))
}

async fn get_workspace(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let id = Uuid::from_str(&id).map_err(|err| bad_request("invalid workspace id", err))?;
    let row = workspace::Entity::find_by_id(id)
        .one(state.db.as_ref())
        .await?
        .or_not_found("workspace", format!("no workspace with id {id}"))?;
    Ok(Json(row))
}

// ── plugin ─────────────────────────────────────────────────────────

async fn list_plugins(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let items = plugin::Entity::find()
        .order_by_asc(plugin::Column::Name)
        .all(state.db.as_ref())
        .await?;
    Ok(Json(ListResponse::from_vec(items)))
}

async fn get_plugin(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let id = Uuid::from_str(&id).map_err(|err| bad_request("invalid plugin id", err))?;
    let row = plugin::Entity::find_by_id(id)
        .one(state.db.as_ref())
        .await?
        .or_not_found("plugin", format!("no plugin with id {id}"))?;
    Ok(Json(row))
}

// ── workspace_plugin (binding) ─────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WorkspacePluginListQuery {
    workspace_id: Option<String>,
    plugin_id: Option<String>,
}

async fn list_workspace_plugins(
    State(state): State<AppState>,
    Query(params): Query<WorkspacePluginListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let mut query = workspace_plugin::Entity::find();
    if let Some(raw) = params.workspace_id {
        let workspace_id = Uuid::from_str(&raw)
            .map_err(|err| bad_request("invalid workspace_id query param", err))?;
        query = query.filter(workspace_plugin::Column::WorkspaceId.eq(workspace_id));
    }
    if let Some(raw) = params.plugin_id {
        let plugin_id = Uuid::from_str(&raw)
            .map_err(|err| bad_request("invalid plugin_id query param", err))?;
        query = query.filter(workspace_plugin::Column::PluginId.eq(plugin_id));
    }
    let items = query
        .order_by_asc(workspace_plugin::Column::WorkspaceId)
        .order_by_asc(workspace_plugin::Column::PluginId)
        .all(state.db.as_ref())
        .await?;
    Ok(Json(ListResponse::from_vec(items)))
}

async fn get_workspace_plugin(
    State(state): State<AppState>,
    Path((workspace_id, plugin_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let workspace_id = Uuid::from_str(&workspace_id)
        .map_err(|err| bad_request("invalid workspace_id path param", err))?;
    let plugin_id = Uuid::from_str(&plugin_id)
        .map_err(|err| bad_request("invalid plugin_id path param", err))?;
    let row = workspace_plugin::Entity::find_by_id((workspace_id, plugin_id))
        .one(state.db.as_ref())
        .await?
        .or_not_found(
            "workspace_plugin",
            format!("no binding for (workspace={workspace_id}, plugin={plugin_id})"),
        )?;
    Ok(Json(row))
}
