//! Read-side handlers: list + by-id over every entity.
//!
//! # Route table (Phase 2 reshape — botworkz/space#311)
//!
//! * **Admin-gated** — `GET /api/tenants`, `GET /api/tenants/{id}`,
//!   `GET /api/plugins`, `GET /api/plugins/{id}`.
//!   Require `x-botwork-admin` header (injected by auth-broker);
//!   absent = 403 `admin_required`.
//!
//! * **Tenant-scoped** — everything under `/api/tenant/{tenant}/…`.
//!   The `{tenant}` path segment is the human-readable tenant name.
//!   Handlers verify that `x-botwork-tenant` header == path tenant;
//!   mismatch or absent header = 403 `cross_tenant_forbidden`.
//!
//! * **List** — returns `{ "items": [...], "total": N }`. The wrapping
//!   struct is deliberate: it lets pagination land (`?limit=&offset=`,
//!   `next_cursor`) as a pure-additive change.
//! * **By-id** — returns the entity model serialised verbatim.
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
//!   session", but that's a control-plane / session-broker concern.
//!   The workspace_plugin live-state gate is the template; we'll add
//!   it when there's a concrete UI use case.

use std::str::FromStr;

use axum::extract::{Path, Query, State};
use axum::http::header::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use botwork_entity::{agent_session, plugin, session_worker, tenant, workspace, workspace_plugin};

use crate::handler::{
    bad_request, check_tenant_consistency, require_admin, resolve_tenant_id, ApiError, ApiErrorExt,
    AppState,
};

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
        // Admin-gated: tenant list/detail and global plugin list/detail.
        .route("/api/tenants", get(list_tenants))
        .route("/api/tenants/:id", get(get_tenant))
        .route("/api/plugins", get(list_plugins))
        .route("/api/plugins/:id", get(get_plugin))
        // Tenant-scoped: path carries {tenant} name; consistency with
        // x-botwork-tenant header is checked in each handler.
        .route("/api/tenant/:tenant/workspaces", get(list_workspaces))
        .route("/api/tenant/:tenant/workspaces/:id", get(get_workspace))
        .route(
            "/api/tenant/:tenant/workspace_plugins",
            get(list_workspace_plugins),
        )
        .route(
            "/api/tenant/:tenant/workspace_plugins/:workspace_id/:plugin_id",
            get(get_workspace_plugin),
        )
        .route(
            "/api/tenant/:tenant/agent_sessions",
            get(list_agent_sessions),
        )
        .route(
            "/api/tenant/:tenant/agent_sessions/:id",
            get(get_agent_session),
        )
        .route(
            "/api/tenant/:tenant/session_workers",
            get(list_session_workers),
        )
        .route(
            "/api/tenant/:tenant/session_workers/:id",
            get(get_session_worker),
        )
}

// ── tenant ─────────────────────────────────────────────────────────

/// Admin-gated: list all tenants.
async fn list_tenants(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&headers)?;
    // Order by name so the response is deterministic across calls.
    let items = tenant::Entity::find()
        .order_by_asc(tenant::Column::Name)
        .all(state.db.as_ref())
        .await?;
    Ok(Json(ListResponse::from_vec(items)))
}

/// Admin-gated: get a single tenant by UUID.
async fn get_tenant(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&headers)?;
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
    /// Optional filter by workspace UUID (further narrows results).
    workspace_id: Option<String>,
}

/// Tenant-scoped: list workspaces for `{tenant}`.
async fn list_workspaces(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tenant_name): Path<String>,
    Query(params): Query<WorkspaceListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    check_tenant_consistency(&headers, &tenant_name)?;
    let tenant_id = resolve_tenant_id(state.db.as_ref(), &tenant_name).await?;

    let mut query = workspace::Entity::find().filter(workspace::Column::TenantId.eq(tenant_id));

    if let Some(raw) = params.workspace_id {
        let ws_id = Uuid::from_str(&raw)
            .map_err(|err| bad_request("invalid workspace_id query param", err))?;
        query = query.filter(workspace::Column::Id.eq(ws_id));
    }

    let items = query
        .order_by_asc(workspace::Column::Name)
        .all(state.db.as_ref())
        .await?;
    Ok(Json(ListResponse::from_vec(items)))
}

/// Tenant-scoped: get a single workspace by UUID.
async fn get_workspace(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((tenant_name, id)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    check_tenant_consistency(&headers, &tenant_name)?;
    let tenant_id = resolve_tenant_id(state.db.as_ref(), &tenant_name).await?;
    let id = Uuid::from_str(&id).map_err(|err| bad_request("invalid workspace id", err))?;
    let row = workspace::Entity::find_by_id(id)
        .one(state.db.as_ref())
        .await?
        .or_not_found("workspace", format!("no workspace with id {id}"))?;
    // Ownership check: workspace must belong to the path tenant.
    if row.tenant_id != tenant_id {
        return Err(ApiError::not_found(
            "workspace",
            format!("no workspace with id {id} under tenant {tenant_name:?}"),
        ));
    }
    Ok(Json(row))
}

// ── plugin ─────────────────────────────────────────────────────────

/// Admin-gated: list all plugins (global, not tenant-scoped).
async fn list_plugins(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&headers)?;
    let items = plugin::Entity::find()
        .order_by_asc(plugin::Column::Name)
        .all(state.db.as_ref())
        .await?;
    Ok(Json(ListResponse::from_vec(items)))
}

/// Admin-gated: get a single plugin by UUID.
async fn get_plugin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&headers)?;
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

/// Tenant-scoped: list bindings for `{tenant}`.
async fn list_workspace_plugins(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tenant_name): Path<String>,
    Query(params): Query<WorkspacePluginListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    check_tenant_consistency(&headers, &tenant_name)?;
    // Parse query-param UUIDs BEFORE any DB short-circuit so a garbage
    // value still surfaces as 400 even when the tenant has no
    // workspaces yet.
    let workspace_filter = match params.workspace_id {
        Some(raw) => Some(
            Uuid::from_str(&raw)
                .map_err(|err| bad_request("invalid workspace_id query param", err))?,
        ),
        None => None,
    };
    let plugin_filter = match params.plugin_id {
        Some(raw) => Some(
            Uuid::from_str(&raw)
                .map_err(|err| bad_request("invalid plugin_id query param", err))?,
        ),
        None => None,
    };

    let tenant_id = resolve_tenant_id(state.db.as_ref(), &tenant_name).await?;

    // Collect workspace IDs belonging to this tenant so we can filter
    // workspace_plugins to only those under this tenant.
    let tenant_workspace_ids: Vec<Uuid> = workspace::Entity::find()
        .filter(workspace::Column::TenantId.eq(tenant_id))
        .all(state.db.as_ref())
        .await?
        .into_iter()
        .map(|w| w.id)
        .collect();

    if tenant_workspace_ids.is_empty() {
        return Ok(Json(ListResponse::from_vec(vec![])));
    }

    let mut query = workspace_plugin::Entity::find()
        .filter(workspace_plugin::Column::WorkspaceId.is_in(tenant_workspace_ids));

    if let Some(workspace_id) = workspace_filter {
        query = query.filter(workspace_plugin::Column::WorkspaceId.eq(workspace_id));
    }
    if let Some(plugin_id) = plugin_filter {
        query = query.filter(workspace_plugin::Column::PluginId.eq(plugin_id));
    }
    let items = query
        .order_by_asc(workspace_plugin::Column::WorkspaceId)
        .order_by_asc(workspace_plugin::Column::PluginId)
        .all(state.db.as_ref())
        .await?;
    Ok(Json(ListResponse::from_vec(items)))
}

/// Tenant-scoped: get a single binding by composite PK.
async fn get_workspace_plugin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((tenant_name, workspace_id, plugin_id)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    check_tenant_consistency(&headers, &tenant_name)?;
    let tenant_id = resolve_tenant_id(state.db.as_ref(), &tenant_name).await?;
    let workspace_id = Uuid::from_str(&workspace_id)
        .map_err(|err| bad_request("invalid workspace_id path param", err))?;
    let plugin_id = Uuid::from_str(&plugin_id)
        .map_err(|err| bad_request("invalid plugin_id path param", err))?;

    // Ownership check: workspace must belong to the path tenant.
    let workspace = workspace::Entity::find_by_id(workspace_id)
        .one(state.db.as_ref())
        .await?
        .or_not_found("workspace", format!("no workspace with id {workspace_id}"))?;
    if workspace.tenant_id != tenant_id {
        return Err(ApiError::not_found(
            "workspace_plugin",
            format!("no binding for workspace {workspace_id} under tenant {tenant_name:?}"),
        ));
    }

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
    /// Filter to sessions belonging to a single workspace.
    workspace_id: Option<String>,
    /// Filter to one lifecycle state.
    state: Option<String>,
}

/// Tenant-scoped: list agent sessions for `{tenant}`.
async fn list_agent_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tenant_name): Path<String>,
    Query(params): Query<AgentSessionListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    check_tenant_consistency(&headers, &tenant_name)?;
    // Parse query-param UUIDs BEFORE the DB lookup so garbage still
    // surfaces as 400 regardless of seed state.
    let workspace_filter = match params.workspace_id {
        Some(raw) => Some(
            Uuid::from_str(&raw)
                .map_err(|err| bad_request("invalid workspace_id query param", err))?,
        ),
        None => None,
    };

    let tenant_id = resolve_tenant_id(state.db.as_ref(), &tenant_name).await?;

    let mut query =
        agent_session::Entity::find().filter(agent_session::Column::TenantId.eq(tenant_id));

    if let Some(workspace_id) = workspace_filter {
        query = query.filter(agent_session::Column::WorkspaceId.eq(workspace_id));
    }
    if let Some(state_val) = params.state {
        query = query.filter(agent_session::Column::State.eq(state_val));
    }
    // Order by last_active_at DESC: most-recently-active first.
    let items = query
        .order_by_desc(agent_session::Column::LastActiveAt)
        .order_by_asc(agent_session::Column::Id)
        .all(state.db.as_ref())
        .await?;
    Ok(Json(ListResponse::from_vec(items)))
}

/// Tenant-scoped: get a single agent session by UUID.
async fn get_agent_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((tenant_name, id)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    check_tenant_consistency(&headers, &tenant_name)?;
    let tenant_id = resolve_tenant_id(state.db.as_ref(), &tenant_name).await?;
    let id = Uuid::from_str(&id).map_err(|err| bad_request("invalid agent_session id", err))?;
    let row = agent_session::Entity::find_by_id(id)
        .one(state.db.as_ref())
        .await?
        .or_not_found("agent_session", format!("no agent_session with id {id}"))?;
    // Ownership check: session must belong to the path tenant.
    if row.tenant_id != tenant_id {
        return Err(ApiError::not_found(
            "agent_session",
            format!("no agent_session with id {id} under tenant {tenant_name:?}"),
        ));
    }
    Ok(Json(row))
}

// ── session_worker ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SessionWorkerListQuery {
    /// Filter to workers bound to a single agent session.
    agent_session_id: Option<String>,
    /// Filter to workers for a single plugin.
    plugin_id: Option<String>,
    /// `live=true`  → reaped_at IS NULL.
    /// `live=false` → reaped_at IS NOT NULL.
    /// Omitted means "no filter on reaped_at".
    live: Option<bool>,
}

/// Tenant-scoped: list session workers for `{tenant}`.
///
/// Workers are linked to tenants indirectly through agent_session.
/// This handler first collects the agent_session IDs for the tenant
/// then filters session_workers by those IDs.
async fn list_session_workers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tenant_name): Path<String>,
    Query(params): Query<SessionWorkerListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    check_tenant_consistency(&headers, &tenant_name)?;
    // Parse query-param UUIDs BEFORE the DB short-circuit so a garbage
    // value still surfaces as 400 even when the tenant has no
    // sessions yet.
    let session_filter = match params.agent_session_id {
        Some(raw) => Some(
            Uuid::from_str(&raw)
                .map_err(|err| bad_request("invalid agent_session_id query param", err))?,
        ),
        None => None,
    };
    let plugin_filter = match params.plugin_id {
        Some(raw) => Some(
            Uuid::from_str(&raw)
                .map_err(|err| bad_request("invalid plugin_id query param", err))?,
        ),
        None => None,
    };

    let tenant_id = resolve_tenant_id(state.db.as_ref(), &tenant_name).await?;

    // Collect agent_session IDs for this tenant so we can filter workers.
    let session_ids: Vec<Uuid> = agent_session::Entity::find()
        .filter(agent_session::Column::TenantId.eq(tenant_id))
        .all(state.db.as_ref())
        .await?
        .into_iter()
        .map(|s| s.id)
        .collect();

    if session_ids.is_empty() {
        return Ok(Json(ListResponse::from_vec(vec![])));
    }

    let mut query = session_worker::Entity::find()
        .filter(session_worker::Column::AgentSessionId.is_in(session_ids));

    if let Some(agent_session_id) = session_filter {
        query = query.filter(session_worker::Column::AgentSessionId.eq(agent_session_id));
    }
    if let Some(plugin_id) = plugin_filter {
        query = query.filter(session_worker::Column::PluginId.eq(plugin_id));
    }
    if let Some(live) = params.live {
        query = if live {
            query.filter(session_worker::Column::ReapedAt.is_null())
        } else {
            query.filter(session_worker::Column::ReapedAt.is_not_null())
        };
    }
    // Order by spawned_at DESC: most-recent activity first.
    let items = query
        .order_by_desc(session_worker::Column::SpawnedAt)
        .order_by_asc(session_worker::Column::Id)
        .all(state.db.as_ref())
        .await?;
    Ok(Json(ListResponse::from_vec(items)))
}

/// Tenant-scoped: get a single session worker by UUID.
async fn get_session_worker(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((tenant_name, id)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    check_tenant_consistency(&headers, &tenant_name)?;
    let tenant_id = resolve_tenant_id(state.db.as_ref(), &tenant_name).await?;
    let id = Uuid::from_str(&id).map_err(|err| bad_request("invalid session_worker id", err))?;
    let row = session_worker::Entity::find_by_id(id)
        .one(state.db.as_ref())
        .await?
        .or_not_found("session_worker", format!("no session_worker with id {id}"))?;
    // Ownership check: worker must link to a session owned by the path tenant.
    // Workers in the spawn-to-first-bind window have agent_session_id = NULL;
    // those are excluded from tenant-scoped views since their tenant is unknown.
    let Some(session_id) = row.agent_session_id else {
        return Err(ApiError::not_found(
            "session_worker",
            format!("no session_worker with id {id} under tenant {tenant_name:?}"),
        ));
    };
    let session = agent_session::Entity::find_by_id(session_id)
        .one(state.db.as_ref())
        .await?
        .or_not_found(
            "agent_session",
            format!("agent_session {session_id} not found"),
        )?;
    if session.tenant_id != tenant_id {
        return Err(ApiError::not_found(
            "session_worker",
            format!("no session_worker with id {id} under tenant {tenant_name:?}"),
        ));
    }
    Ok(Json(row))
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use http::{Request, StatusCode};
    use sea_orm::{DatabaseBackend, DbErr, MockDatabase};
    use tower::ServiceExt;

    use super::*;

    fn admin_get(path: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(path)
            .header(crate::handler::ADMIN_HEADER, "ops")
            .body(Body::empty())
            .expect("request")
    }

    fn anonymous_get(path: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(path)
            .body(Body::empty())
            .expect("request")
    }

    fn tenant_get(path: &str, tenant: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(path)
            .header(crate::handler::TENANT_HEADER, tenant)
            .body(Body::empty())
            .expect("request")
    }

    fn tenant_row(id: Uuid, name: &str) -> tenant::Model {
        tenant::Model {
            id,
            name: name.to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn workspace_row(id: Uuid, tenant_id: Uuid, name: &str) -> workspace::Model {
        workspace::Model {
            id,
            tenant_id,
            name: name.to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn plugin_row(id: Uuid, name: &str) -> plugin::Model {
        plugin::Model {
            id,
            name: name.to_string(),
            image: "ghcr.io/example/mcp-fetch:1.0".to_string(),
            port: 8000,
            path: "/mcp".to_string(),
            upstream_auth: "none".to_string(),
            env: serde_json::json!([]),
            resources: None,
            egress: serde_json::json!({ "mode": "none" }),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            current_facet_id: None,
        }
    }

    fn agent_session_row(id: Uuid, tenant_id: Uuid, workspace_id: Uuid) -> agent_session::Model {
        agent_session::Model {
            id,
            tenant_id,
            workspace_id,
            agent_session_id: "session-1".to_string(),
            state: botwork_entity::agent_session::state::ACTIVE.to_string(),
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            reactivation_count: 0,
        }
    }

    fn session_worker_row(
        id: Uuid,
        session_id: Option<Uuid>,
        plugin_id: Uuid,
    ) -> session_worker::Model {
        session_worker::Model {
            id,
            agent_session_id: session_id,
            plugin_id,
            container_name: "mcp_session_x".to_string(),
            container_ip: "10.0.0.2".to_string(),
            mcp_session_id: "mcp-session-id".to_string(),
            spawned_at: chrono::Utc::now(),
            reaped_at: None,
        }
    }

    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        serde_json::from_slice(&body).expect("json body")
    }

    #[tokio::test]
    async fn list_tenants_uses_mock_database_results() {
        let tenant_row = tenant::Model {
            id: Uuid::new_v4(),
            name: "phlax".to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres).append_query_results([vec![tenant_row]]),
        );
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(admin_get("/api/tenants"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(json["total"], 1);
        assert_eq!(json["items"][0]["name"], "phlax");
    }

    #[tokio::test]
    async fn list_tenants_maps_db_errors_to_internal() {
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_errors([DbErr::Custom("boom".to_string())]),
        );
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(admin_get("/api/tenants"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(json["error"]["code"], "internal");
    }

    #[tokio::test]
    async fn list_tenants_requires_admin_header() {
        let state = crate::test_support::app_state_with_mock_db(MockDatabase::new(
            DatabaseBackend::Postgres,
        ));
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(anonymous_get("/api/tenants"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let json = json_body(response).await;
        assert_eq!(json["error"]["code"], "admin_required");
    }

    #[tokio::test]
    async fn get_tenant_invalid_uuid_is_bad_request() {
        let state = crate::test_support::app_state_with_mock_db(MockDatabase::new(
            DatabaseBackend::Postgres,
        ));
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(admin_get("/api/tenants/not-a-uuid"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = json_body(response).await;
        assert_eq!(json["error"]["code"], "bad_request");
    }

    #[tokio::test]
    async fn get_tenant_returns_mocked_row() {
        let id = Uuid::new_v4();
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![tenant_row(id, "phlax")]]),
        );
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(admin_get(&format!("/api/tenants/{id}")))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["id"], id.to_string());
        assert_eq!(json["name"], "phlax");
    }

    #[tokio::test]
    async fn list_workspaces_requires_tenant_header_match() {
        let state = crate::test_support::app_state_with_mock_db(MockDatabase::new(
            DatabaseBackend::Postgres,
        ));
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(anonymous_get("/api/tenant/phlax/workspaces"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let json = json_body(response).await;
        assert_eq!(json["error"]["code"], "cross_tenant_forbidden");
    }

    #[tokio::test]
    async fn list_workspaces_invalid_workspace_id_is_bad_request() {
        let tenant_id = Uuid::new_v4();
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![tenant_row(tenant_id, "phlax")]]),
        );
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(tenant_get(
                "/api/tenant/phlax/workspaces?workspace_id=not-a-uuid",
                "phlax",
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = json_body(response).await;
        assert_eq!(json["error"]["code"], "bad_request");
    }

    #[tokio::test]
    async fn get_workspace_returns_not_found_on_cross_tenant_ownership() {
        let path_tenant_id = Uuid::new_v4();
        let other_tenant_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![tenant_row(path_tenant_id, "phlax")]])
                .append_query_results([vec![workspace_row(workspace_id, other_tenant_id, "mcp")]]),
        );
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(tenant_get(
                &format!("/api/tenant/phlax/workspaces/{workspace_id}"),
                "phlax",
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let json = json_body(response).await;
        assert_eq!(json["error"]["code"], "not_found");
    }

    #[tokio::test]
    async fn list_plugins_requires_admin_header() {
        let state = crate::test_support::app_state_with_mock_db(MockDatabase::new(
            DatabaseBackend::Postgres,
        ));
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(anonymous_get("/api/plugins"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let json = json_body(response).await;
        assert_eq!(json["error"]["code"], "admin_required");
    }

    #[tokio::test]
    async fn get_plugin_returns_mocked_row() {
        let plugin_id = Uuid::new_v4();
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![plugin_row(plugin_id, "mcp-fetch")]]),
        );
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(admin_get(&format!("/api/plugins/{plugin_id}")))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["id"], plugin_id.to_string());
        assert_eq!(json["name"], "mcp-fetch");
    }

    #[tokio::test]
    async fn list_workspace_plugins_invalid_query_param_is_bad_request() {
        let state = crate::test_support::app_state_with_mock_db(MockDatabase::new(
            DatabaseBackend::Postgres,
        ));
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(tenant_get(
                "/api/tenant/phlax/workspace_plugins?plugin_id=garbage",
                "phlax",
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = json_body(response).await;
        assert_eq!(json["error"]["code"], "bad_request");
    }

    #[tokio::test]
    async fn list_workspace_plugins_empty_workspace_set_short_circuits() {
        let tenant_id = Uuid::new_v4();
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![tenant_row(tenant_id, "phlax")]])
                .append_query_results([Vec::<workspace::Model>::new()]),
        );
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(tenant_get("/api/tenant/phlax/workspace_plugins", "phlax"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["total"], 0);
        assert_eq!(json["items"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn get_workspace_plugin_invalid_workspace_uuid_is_bad_request() {
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![tenant_row(Uuid::new_v4(), "phlax")]]),
        );
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(tenant_get(
                "/api/tenant/phlax/workspace_plugins/not-a-uuid/also-not-uuid",
                "phlax",
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = json_body(response).await;
        assert_eq!(json["error"]["code"], "bad_request");
    }

    #[tokio::test]
    async fn list_agent_sessions_invalid_workspace_id_is_bad_request() {
        let state = crate::test_support::app_state_with_mock_db(MockDatabase::new(
            DatabaseBackend::Postgres,
        ));
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(tenant_get(
                "/api/tenant/phlax/agent_sessions?workspace_id=garbage",
                "phlax",
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = json_body(response).await;
        assert_eq!(json["error"]["code"], "bad_request");
    }

    #[tokio::test]
    async fn get_agent_session_returns_not_found_on_cross_tenant_ownership() {
        let path_tenant_id = Uuid::new_v4();
        let other_tenant_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![tenant_row(path_tenant_id, "phlax")]])
                .append_query_results([vec![agent_session_row(
                    session_id,
                    other_tenant_id,
                    workspace_id,
                )]]),
        );
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(tenant_get(
                &format!("/api/tenant/phlax/agent_sessions/{session_id}"),
                "phlax",
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let json = json_body(response).await;
        assert_eq!(json["error"]["code"], "not_found");
    }

    #[tokio::test]
    async fn list_session_workers_invalid_plugin_id_is_bad_request() {
        let state = crate::test_support::app_state_with_mock_db(MockDatabase::new(
            DatabaseBackend::Postgres,
        ));
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(tenant_get(
                "/api/tenant/phlax/session_workers?plugin_id=garbage",
                "phlax",
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = json_body(response).await;
        assert_eq!(json["error"]["code"], "bad_request");
    }

    #[tokio::test]
    async fn list_session_workers_empty_session_set_short_circuits() {
        let tenant_id = Uuid::new_v4();
        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![tenant_row(tenant_id, "phlax")]])
                .append_query_results([Vec::<agent_session::Model>::new()]),
        );
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(tenant_get("/api/tenant/phlax/session_workers", "phlax"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["total"], 0);
        assert_eq!(json["items"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn get_session_worker_returns_not_found_on_cross_tenant_ownership() {
        let path_tenant_id = Uuid::new_v4();
        let other_tenant_id = Uuid::new_v4();
        let worker_id = Uuid::new_v4();
        let plugin_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();

        let state = crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![tenant_row(path_tenant_id, "phlax")]])
                .append_query_results([vec![session_worker_row(
                    worker_id,
                    Some(session_id),
                    plugin_id,
                )]])
                .append_query_results([vec![agent_session_row(
                    session_id,
                    other_tenant_id,
                    workspace_id,
                )]]),
        );
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(tenant_get(
                &format!("/api/tenant/phlax/session_workers/{worker_id}"),
                "phlax",
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let json = json_body(response).await;
        assert_eq!(json["error"]["code"], "not_found");
    }
}
