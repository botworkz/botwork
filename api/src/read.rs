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
//!   gives us `Serialize` for free; api does NOT introduce a
//!   separate DTO in v0.
//!
//! Filters on the list endpoints are intentionally minimal:
//!
//! * `workspaces` accepts `?tenant_id=<uuid>`,
//! * `workspace_plugins` accepts `?workspace_id=<uuid>` and
//!   `?plugin_id=<uuid>` (combinable).
//! * `agent_sessions` accepts `?tenant_id=<uuid>`,
//!   `?workspace_id=<uuid>`, and `?state=<string>` (combinable).
//! * `session_workers` accepts `?agent_session_id=<uuid>`,
//!   `?plugin_id=<uuid>`, and `?live=true|false` (combinable —
//!   `live=true` filters to `reaped_at IS NULL`).
//!
//! tenant + plugin have no filters today: there are <10 rows of
//! each in any realistic deployment.
//!
//! Composite-PK route convention: `workspace_plugin` is addressed as
//! `/{workspace_id}/{plugin_id}` (two path segments). The plugin/
//! tenant URLs use a single `{id}` segment so the by-id pattern is
//! distinguishable at the router level. `agent_session` and
//! `session_worker` have single-uuid PKs and follow the simple
//! `/{id}` shape.
//!
//! # Why agent_session and session_worker are READ-ONLY
//!
//! Both tables are written by session-broker as agents spawn, register,
//! and die. Operator-driven CRUD on them is mostly nonsensical:
//!
//! * **Create** — sessions and workers come into existence through the
//!   spawn path, not through api. There is no shape for "please
//!   create a session row out of thin air".
//! * **Update** — session-broker owns the lifecycle (state transitions
//!   on agent_session, reaped_at on session_worker). api PUTs
//!   would race with the writer.
//! * **Delete** — could legitimately mean "force-terminate this live
//!   session", but that's a control-plane / session-broker concern
//!   (coordinate with the live container, not just yank a row). The
//!   workspace_plugin live-state gate is the template; we'll add it
//!   when there's a concrete UI use case. Skipping for now.
//!
//! So this layer exposes list + by-id only. Future-selves can add
//! delete-with-live-gate when ui needs it; this PR keeps the
//! surface honest about what's currently safe to do through HTTP.

use std::str::FromStr;

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use botwork_entity::{agent_session, plugin, session_worker, tenant, workspace, workspace_plugin};

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
        .route("/admin/api/v1/agent_sessions", get(list_agent_sessions))
        .route("/admin/api/v1/agent_sessions/:id", get(get_agent_session))
        .route("/admin/api/v1/session_workers", get(list_session_workers))
        .route("/admin/api/v1/session_workers/:id", get(get_session_worker))
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

// ── agent_session ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AgentSessionListQuery {
    /// Filter to sessions belonging to a single tenant.
    tenant_id: Option<String>,
    /// Filter to sessions belonging to a single workspace.
    workspace_id: Option<String>,
    /// Filter to one lifecycle state. Wire shape matches the
    /// agent_session::state module constants verbatim
    /// (`active` / `grace` / `inactive` / `teardown_requested` /
    /// `purged`). Unknown values return an empty list rather than
    /// 400 — api doesn't own the state vocabulary, the row
    /// writer does. A typo just means "no rows match".
    state: Option<String>,
}

async fn list_agent_sessions(
    State(state): State<AppState>,
    Query(params): Query<AgentSessionListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let mut query = agent_session::Entity::find();
    if let Some(raw) = params.tenant_id {
        let tenant_id = Uuid::from_str(&raw)
            .map_err(|err| bad_request("invalid tenant_id query param", err))?;
        query = query.filter(agent_session::Column::TenantId.eq(tenant_id));
    }
    if let Some(raw) = params.workspace_id {
        let workspace_id = Uuid::from_str(&raw)
            .map_err(|err| bad_request("invalid workspace_id query param", err))?;
        query = query.filter(agent_session::Column::WorkspaceId.eq(workspace_id));
    }
    if let Some(state_val) = params.state {
        query = query.filter(agent_session::Column::State.eq(state_val));
    }
    // Order by last_active_at DESC: the operator answer to "what's
    // happening right now?" puts the most-recently-active sessions
    // at the top. Tiebreak on id (Uuid) so the response is
    // deterministic when two rows share a timestamp (which they
    // can — Postgres timestamptz has microsecond resolution, and
    // session-broker bumps last_active_at in batches).
    let items = query
        .order_by_desc(agent_session::Column::LastActiveAt)
        .order_by_asc(agent_session::Column::Id)
        .all(state.db.as_ref())
        .await?;
    Ok(Json(ListResponse::from_vec(items)))
}

async fn get_agent_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let id = Uuid::from_str(&id).map_err(|err| bad_request("invalid agent_session id", err))?;
    let row = agent_session::Entity::find_by_id(id)
        .one(state.db.as_ref())
        .await?
        .or_not_found("agent_session", format!("no agent_session with id {id}"))?;
    Ok(Json(row))
}

// ── session_worker ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SessionWorkerListQuery {
    /// Filter to workers bound to a single agent session. Note that
    /// `session_worker.agent_session_id` is nullable (spawn-to-first-
    /// request window), so workers with no session yet are
    /// unconditionally excluded by this filter — they're not
    /// addressable by session anyway.
    agent_session_id: Option<String>,
    /// Filter to workers for a single plugin.
    plugin_id: Option<String>,
    /// `live=true`  → reaped_at IS NULL  (container is live or
    ///                 believed-live; routing is allowed).
    /// `live=false` → reaped_at IS NOT NULL (container is reaped;
    ///                 row is audit-only, awaiting janitor sweep).
    /// Omitted means "no filter on reaped_at".
    live: Option<bool>,
}

async fn list_session_workers(
    State(state): State<AppState>,
    Query(params): Query<SessionWorkerListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let mut query = session_worker::Entity::find();
    if let Some(raw) = params.agent_session_id {
        let agent_session_id = Uuid::from_str(&raw)
            .map_err(|err| bad_request("invalid agent_session_id query param", err))?;
        query = query.filter(session_worker::Column::AgentSessionId.eq(agent_session_id));
    }
    if let Some(raw) = params.plugin_id {
        let plugin_id = Uuid::from_str(&raw)
            .map_err(|err| bad_request("invalid plugin_id query param", err))?;
        query = query.filter(session_worker::Column::PluginId.eq(plugin_id));
    }
    if let Some(live) = params.live {
        query = if live {
            query.filter(session_worker::Column::ReapedAt.is_null())
        } else {
            query.filter(session_worker::Column::ReapedAt.is_not_null())
        };
    }
    // Order by spawned_at DESC: same posture as agent_sessions —
    // most-recent activity first is the right default for an
    // operator browsing live state. Tiebreak on id for
    // determinism (multiple workers can spawn in the same tick).
    let items = query
        .order_by_desc(session_worker::Column::SpawnedAt)
        .order_by_asc(session_worker::Column::Id)
        .all(state.db.as_ref())
        .await?;
    Ok(Json(ListResponse::from_vec(items)))
}

async fn get_session_worker(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let id = Uuid::from_str(&id).map_err(|err| bad_request("invalid session_worker id", err))?;
    let row = session_worker::Entity::find_by_id(id)
        .one(state.db.as_ref())
        .await?
        .or_not_found("session_worker", format!("no session_worker with id {id}"))?;
    Ok(Json(row))
}
