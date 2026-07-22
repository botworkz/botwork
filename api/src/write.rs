//! Write-side handlers: POST / PUT / DELETE over every entity.
//!
//! # Shared shape
//!
//! Every write is mechanically the same:
//!
//! 1. Parse the body via [`AdminJson<T>`] (envelope-shaped 400 on
//!    malformed JSON / unknown field).
//! 2. Validate (api-core for plugin specs; per-handler regex /
//!    range checks for the other entities).
//! 3. Open a transaction.
//! 4. (`PUT`/`DELETE` only) Read the current row inside the txn,
//!    422 if missing, compare `updated_at` to the supplied lock
//!    token, 409 `stale_write` on mismatch.
//! 5. (`DELETE` only on tenant + plugin) Run the dependency
//!    preflight; 409 `has_dependents` with the dependent identities
//!    when blocked.
//! 6. Apply the mutation.
//! 7. (`workspace_plugin` writes that affect live state) Run the
//!    control-plane gate. On failure roll back and return 503.
//! 8. Commit. Emit a structured tracing event.
//! 9. Respond — 201 + `Location` for POST, 200 with the row for PUT,
//!    204 for DELETE.
//!
//! # Optimistic-lock token
//!
//! `updated_at` is the token. PUT and DELETE bodies for the
//! single-PK entities carry an `if_unmodified_since` field; the
//! handler compares it to the live `updated_at` (RFC3339 round-trip
//! via chrono) inside the transaction. Mismatch → 409 `stale_write`.
//! Same token for both PUT and DELETE so the UI flow is uniform:
//! GET → render → if PUT/DELETE → include the token.
//!
//! workspace_plugin DELETE/PUT take the token as a query param
//! (`?if_unmodified_since=...`) because the composite-PK path
//! already carries two UUIDs and a third query param keeps things
//! visually distinct.
//!
//! # Delete-guards
//!
//! `tenant` and `plugin` have inbound FKs with `ON DELETE RESTRICT`.
//! Before issuing the DELETE we count dependents and return 409
//! `has_dependents` with the identifying fields of each blocker.
//! `workspace` cascades to bindings (per the schema's `ON DELETE
//! CASCADE`); we surface the dependent count in a tracing event but
//! don't refuse the delete — the schema already says it's safe.

use std::str::FromStr;

use axum::extract::{Path, Query, State};
use axum::http::header::{HeaderMap, LOCATION};
use axum::http::{HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, post, put};
use axum::{Json, Router};
use botwork_api_core::names::{
    normalise_name, validate_plugin_name, validate_tenant_name, validate_workspace_name,
};
use botwork_api_core::plugin_spec::{validate_one, RawPluginEntry};
use botwork_entity::{plugin, tenant, workspace, workspace_plugin};
use chrono::{DateTime, Utc};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseConnection,
    EntityTrait, JoinType, QueryFilter, TransactionTrait,
};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use tracing::info;
use uuid::Uuid;

use crate::control_plane::{outcome_summary, terminate_live_sessions, GateError};
use crate::handler::{
    bad_request, check_tenant_consistency, operator, parse_body, require_admin, resolve_tenant_id,
    ApiError, ApiErrorExt, AppState, PREFIX,
};
use crate::secret_store::{PutSecretRequest, SecretStoreError};
use crate::session_broker::signal_evict;

// ── name validation helpers ───────────────────────────────────────
//
// Delegates to `botwork-api-core::names` which vendors the canonical
// grammar from `botwork-extra/auth-broker/src/grammar.rs`.
// see api-core/src/names.rs for the full spec.

/// Case-insensitive uniqueness check: returns true if any tenant
/// whose LOWER(name) == normalised already exists.
/// `exclude_id` skips the current row (for rename-to-self on PUT).
async fn tenant_name_taken(
    db: &impl sea_orm::ConnectionTrait,
    normalised: &str,
    exclude_id: Option<uuid::Uuid>,
) -> Result<bool, ApiError> {
    use sea_orm::{FromQueryResult, Statement};

    #[derive(FromQueryResult)]
    struct Row {
        cnt: i64,
    }

    let backend = db.get_database_backend();
    // Intentionally parameterised: `normalised` is operator-supplied input and
    // must never be interpolated into SQL text.
    let (sql, values): (&str, Vec<sea_orm::Value>) = match exclude_id {
        None => (
            "SELECT COUNT(*) AS cnt FROM tenant WHERE LOWER(name) = $1",
            vec![normalised.into()],
        ),
        Some(id) => (
            "SELECT COUNT(*) AS cnt FROM tenant WHERE LOWER(name) = $1 AND id != $2",
            vec![normalised.into(), id.into()],
        ),
    };
    let stmt = Statement::from_sql_and_values(backend, sql, values);
    let row = Row::find_by_statement(stmt)
        .one(db)
        .await
        .map_err(|err| ApiError::Internal {
            detail: format!("db: {err}"),
        })?;
    Ok(row.map(|r| r.cnt > 0).unwrap_or(false))
}

/// Case-insensitive uniqueness check for workspace names within a tenant.
/// `(tenant_id, LOWER(name))` must be unique.
async fn workspace_name_taken(
    db: &impl sea_orm::ConnectionTrait,
    tenant_id: uuid::Uuid,
    normalised: &str,
    exclude_id: Option<uuid::Uuid>,
) -> Result<bool, ApiError> {
    use sea_orm::{FromQueryResult, Statement};

    #[derive(FromQueryResult)]
    struct Row {
        cnt: i64,
    }

    let backend = db.get_database_backend();
    // Intentionally parameterised: `normalised` is operator-supplied input and
    // must never be interpolated into SQL text.
    let (sql, values): (&str, Vec<sea_orm::Value>) = match exclude_id {
        None => (
            "SELECT COUNT(*) AS cnt FROM workspace WHERE tenant_id = $1 AND LOWER(name) = $2",
            vec![tenant_id.into(), normalised.into()],
        ),
        Some(id) => (
            "SELECT COUNT(*) AS cnt FROM workspace WHERE tenant_id = $1 AND LOWER(name) = $2 AND id != $3",
            vec![tenant_id.into(), normalised.into(), id.into()],
        ),
    };
    let stmt = Statement::from_sql_and_values(backend, sql, values);
    let row = Row::find_by_statement(stmt)
        .one(db)
        .await
        .map_err(|err| ApiError::Internal {
            detail: format!("db: {err}"),
        })?;
    Ok(row.map(|r| r.cnt > 0).unwrap_or(false))
}

/// Case-insensitive uniqueness check for plugin names (global).
async fn plugin_name_taken(
    db: &impl sea_orm::ConnectionTrait,
    normalised: &str,
    exclude_id: Option<uuid::Uuid>,
) -> Result<bool, ApiError> {
    use sea_orm::{FromQueryResult, Statement};

    #[derive(FromQueryResult)]
    struct Row {
        cnt: i64,
    }

    let backend = db.get_database_backend();
    // Intentionally parameterised: `normalised` is operator-supplied input and
    // must never be interpolated into SQL text.
    let (sql, values): (&str, Vec<sea_orm::Value>) = match exclude_id {
        None => (
            "SELECT COUNT(*) AS cnt FROM plugin WHERE LOWER(name) = $1",
            vec![normalised.into()],
        ),
        Some(id) => (
            "SELECT COUNT(*) AS cnt FROM plugin WHERE LOWER(name) = $1 AND id != $2",
            vec![normalised.into(), id.into()],
        ),
    };
    let stmt = Statement::from_sql_and_values(backend, sql, values);
    let row = Row::find_by_statement(stmt)
        .one(db)
        .await
        .map_err(|err| ApiError::Internal {
            detail: format!("db: {err}"),
        })?;
    Ok(row.map(|r| r.cnt > 0).unwrap_or(false))
}

/// Basic non-empty validation for secret service/name components.
/// The secret-store backend is the authority on the full set of rules;
/// this is just a frontend sanity check to reject obviously-bad inputs.
fn require_secret_component(field: &str, value: &str) -> Result<String, ApiError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ApiError::validation_failed(format!(
            "{field} must be a non-blank string"
        )));
    }
    // Sanity-bound the input before the backend sees it. The backend is
    // authoritative on full grammar; this blocks obvious traversal/null-byte
    // patterns and unbounded component size.
    if trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains('\0')
        || trimmed.starts_with('.')
        || trimmed.len() > 128
    {
        return Err(ApiError::validation_failed(format!(
            "{field} contains forbidden characters or is too long"
        )));
    }
    Ok(trimmed.to_string())
}

/// Compare a client-supplied `if_unmodified_since` to a live
/// `updated_at`. Returns Err with `stale_write` when they don't
/// match.
///
/// Uses RFC3339 with sub-second precision — chrono's default
/// `to_rfc3339()` round-trip. Postgres' `timestamp with time zone`
/// preserves microsecond precision, so a value the client got via
/// our own GET will round-trip cleanly.
pub(crate) fn check_lock(
    submitted: &DateTime<Utc>,
    live: &DateTime<Utc>,
    label: &str,
) -> Result<(), ApiError> {
    if submitted.timestamp_micros() != live.timestamp_micros() {
        return Err(ApiError::stale_write(format!(
            "{label}: if_unmodified_since {submitted} doesn't match live {live}; \
             re-fetch and retry"
        )));
    }
    Ok(())
}

// ── audit logging ──────────────────────────────────────────────────

/// One-line structured audit event per write. v0 surfaces the tuple
/// (operator, verb, entity, id, result); the audit-table RFE lifts
/// the same fields into a row.
fn audit_event(op: &str, verb: &str, entity: &str, id: impl std::fmt::Display, extra: &str) {
    info!("{PREFIX} audit operator={op:?} verb={verb} entity={entity} id={id} {extra}");
}

// ── router ─────────────────────────────────────────────────────────

pub fn router() -> Router<AppState> {
    Router::new()
        // Admin-gated tenant CRUD.
        .route("/api/tenants", post(create_tenant))
        .route("/api/tenants/{id}", put(update_tenant))
        .route("/api/tenants/{id}", delete(delete_tenant))
        // Admin-gated plugin CRUD (plugins are globally shared resources).
        .route("/api/plugins", post(create_plugin))
        .route("/api/plugins/{id}", put(update_plugin))
        .route("/api/plugins/{id}", delete(delete_plugin))
        // Tenant-scoped workspace CRUD.
        .route("/api/tenant/{tenant}/workspaces", post(create_workspace))
        .route(
            "/api/tenant/{tenant}/workspaces/{id}",
            put(update_workspace),
        )
        .route(
            "/api/tenant/{tenant}/workspaces/{id}",
            delete(delete_workspace),
        )
        // Tenant-scoped binding CRUD.
        .route(
            "/api/tenant/{tenant}/workspace_plugins",
            post(create_workspace_plugin),
        )
        .route(
            "/api/tenant/{tenant}/workspace_plugins/{workspace_id}/{plugin_id}",
            put(update_workspace_plugin),
        )
        .route(
            "/api/tenant/{tenant}/workspace_plugins/{workspace_id}/{plugin_id}",
            delete(delete_workspace_plugin),
        )
        // Tenant-scoped secrets.
        .route("/api/tenant/{tenant}/secrets", post(create_secret))
        .route(
            "/api/tenant/{tenant}/secrets/{service}/{name}",
            delete(delete_secret),
        )
}

// ── tenant ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TenantCreate {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TenantUpdate {
    name: String,
    if_unmodified_since: DateTime<Utc>,
}

async fn create_tenant(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(raw_body): Json<JsonValue>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&headers)?;
    let body: TenantCreate = parse_body(raw_body)?;
    let name = body.name.trim().to_string();
    // Validate format (regex) and reserved-name list.
    validate_tenant_name(&name).map_err(ApiError::from)?;
    let op = operator(&headers);

    let row = state.store.create_tenant(name.clone()).await?;
    let created_id = row.id;

    audit_event(&op, "create", "tenant", row.id, &format!("name={name:?}"));

    let mut response = (StatusCode::CREATED, Json(row)).into_response();
    response.headers_mut().insert(
        LOCATION,
        HeaderValue::from_str(&format!("/api/tenants/{created_id}")).expect("uuid is ascii"),
    );
    Ok(response)
}

async fn update_tenant(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(raw_body): Json<JsonValue>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&headers)?;
    let body: TenantUpdate = parse_body(raw_body)?;
    let id = Uuid::from_str(&id).map_err(|err| bad_request("invalid tenant id", err))?;
    let name = body.name.trim().to_string();
    validate_tenant_name(&name).map_err(ApiError::from)?;
    let op = operator(&headers);

    let row = state
        .store
        .update_tenant(id, name.clone(), body.if_unmodified_since)
        .await?;

    audit_event(&op, "update", "tenant", id, &format!("name={name:?}"));
    Ok((StatusCode::OK, Json(row)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteQuery {
    if_unmodified_since: Option<DateTime<Utc>>,
}

async fn delete_tenant(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&headers)?;
    let id = Uuid::from_str(&id).map_err(|err| bad_request("invalid tenant id", err))?;
    let op = operator(&headers);

    let live = state.store.delete_tenant(id, q.if_unmodified_since).await?;

    audit_event(
        &op,
        "delete",
        "tenant",
        id,
        &format!("name={:?}", live.name),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn db_create_tenant(
    db: &DatabaseConnection,
    name: String,
) -> Result<tenant::Model, ApiError> {
    let normalised = normalise_name(&name);
    if tenant_name_taken(db, &normalised, None).await? {
        return Err(ApiError::already_exists(format!(
            "tenant with name {name:?} already exists (case-insensitive)"
        )));
    }

    let id = Uuid::new_v4();
    let now = Utc::now();
    tenant::ActiveModel {
        id: Set(id),
        name: Set(name),
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(db)
    .await
    .map_err(ApiError::from)
}

pub(crate) async fn db_update_tenant(
    db: &DatabaseConnection,
    id: Uuid,
    name: String,
    if_unmodified_since: DateTime<Utc>,
) -> Result<tenant::Model, ApiError> {
    let tx = db.begin().await?;
    let live = tenant::Entity::find_by_id(id)
        .one(&tx)
        .await?
        .or_not_found("tenant", format!("no tenant with id {id}"))?;
    check_lock(&if_unmodified_since, &live.updated_at, "tenant")?;

    let normalised = normalise_name(&name);
    if tenant_name_taken(&tx, &normalised, Some(live.id)).await? {
        return Err(ApiError::already_exists(format!(
            "tenant with name {name:?} already exists (case-insensitive)"
        )));
    }

    let mut active: tenant::ActiveModel = live.into();
    active.name = Set(name);
    active.updated_at = Set(Utc::now());
    let row = active.update(&tx).await?;
    tx.commit().await?; // MockDatabase cannot model commit; exercised by api/tests/integration.rs against real Postgres.
    Ok(row)
}

pub(crate) async fn db_delete_tenant(
    db: &DatabaseConnection,
    id: Uuid,
    if_unmodified_since: Option<DateTime<Utc>>,
) -> Result<tenant::Model, ApiError> {
    let tx = db.begin().await?;
    let live = tenant::Entity::find_by_id(id)
        .one(&tx)
        .await?
        .or_not_found("tenant", format!("no tenant with id {id}"))?;
    if let Some(lock) = if_unmodified_since {
        check_lock(&lock, &live.updated_at, "tenant")?;
    }
    let dependents = workspace::Entity::find()
        .filter(workspace::Column::TenantId.eq(id))
        .all(&tx)
        .await?;
    if !dependents.is_empty() {
        let names: Vec<String> = dependents.iter().map(|w| w.name.clone()).collect();
        return Err(ApiError::has_dependents(format!(
            "tenant '{}' still has {} workspace(s): {:?}",
            live.name,
            dependents.len(),
            names
        )));
    }
    tenant::Entity::delete_by_id(id).exec(&tx).await?;
    tx.commit().await?; // MockDatabase cannot model commit; exercised by api/tests/integration.rs against real Postgres.
    Ok(live)
}

// ── workspace ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceCreate {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceUpdate {
    name: String,
    if_unmodified_since: DateTime<Utc>,
}

async fn create_workspace(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tenant_name): Path<String>,
    Json(raw_body): Json<JsonValue>,
) -> Result<impl IntoResponse, ApiError> {
    check_tenant_consistency(&headers, &tenant_name)?;
    let body: WorkspaceCreate = parse_body(raw_body)?;
    let name = body.name.trim().to_string();
    validate_workspace_name(&name).map_err(ApiError::from)?;
    let op = operator(&headers);

    // Resolve tenant name to UUID (path-borne tenant is the source of truth).
    let tenant_id = resolve_tenant_id(&state.store, &tenant_name).await?;

    // Case-insensitive uniqueness within the tenant: (tenant_id, LOWER(name)).
    let normalised = normalise_name(&name);
    if workspace_name_taken(state.db.as_ref(), tenant_id, &normalised, None).await? {
        return Err(ApiError::already_exists(format!(
            "workspace with name {name:?} already exists under tenant {tenant_name:?} (case-insensitive)"
        )));
    }

    let id = Uuid::new_v4();
    let now = Utc::now();
    // Store the un-normalised name (preserve case); uniqueness is enforced
    // separately via LOWER(name) checks.
    let row = workspace::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant_id),
        name: Set(name.clone()),
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(state.db.as_ref())
    .await?;

    audit_event(
        &op,
        "create",
        "workspace",
        row.id,
        &format!("tenant={tenant_name:?} name={name:?}"),
    );
    let mut response = (StatusCode::CREATED, Json(row)).into_response();
    response.headers_mut().insert(
        LOCATION,
        HeaderValue::from_str(&format!("/api/tenant/{tenant_name}/workspaces/{id}"))
            .expect("tenant_name and uuid are ascii-safe"),
    );
    Ok(response)
}

async fn update_workspace(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((tenant_name, id)): Path<(String, String)>,
    Json(raw_body): Json<JsonValue>,
) -> Result<impl IntoResponse, ApiError> {
    check_tenant_consistency(&headers, &tenant_name)?;
    let body: WorkspaceUpdate = parse_body(raw_body)?;
    let id = Uuid::from_str(&id).map_err(|err| bad_request("invalid workspace id", err))?;
    let name = body.name.trim().to_string();
    validate_workspace_name(&name).map_err(ApiError::from)?;
    let op = operator(&headers);

    let tenant_id = resolve_tenant_id(&state.store, &tenant_name).await?;
    let tx = state.db.begin().await?;
    let live = workspace::Entity::find_by_id(id)
        .one(&tx)
        .await?
        .or_not_found("workspace", format!("no workspace with id {id}"))?;
    // Ownership check.
    if live.tenant_id != tenant_id {
        return Err(ApiError::not_found(
            "workspace",
            format!("no workspace with id {id} under tenant {tenant_name:?}"),
        ));
    }
    check_lock(&body.if_unmodified_since, &live.updated_at, "workspace")?;

    // Case-insensitive uniqueness on rename: exclude current workspace.
    let normalised = normalise_name(&name);
    if workspace_name_taken(&tx, tenant_id, &normalised, Some(live.id)).await? {
        return Err(ApiError::already_exists(format!(
            "workspace with name {name:?} already exists under tenant {tenant_name:?} (case-insensitive)"
        )));
    }

    let now = Utc::now();
    let mut active: workspace::ActiveModel = live.into();
    // Store the un-normalised name (preserve case); uniqueness is enforced
    // separately via LOWER(name) checks.
    active.name = Set(name.clone());
    active.updated_at = Set(now);
    let row = active.update(&tx).await?;
    tx.commit().await?;

    audit_event(
        &op,
        "update",
        "workspace",
        id,
        &format!("tenant={tenant_name:?} name={name:?}"),
    );
    Ok((StatusCode::OK, Json(row)))
}

async fn delete_workspace(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((tenant_name, id)): Path<(String, String)>,
    Query(q): Query<DeleteQuery>,
) -> Result<impl IntoResponse, ApiError> {
    check_tenant_consistency(&headers, &tenant_name)?;
    let id = Uuid::from_str(&id).map_err(|err| bad_request("invalid workspace id", err))?;
    let op = operator(&headers);

    let tenant_id = resolve_tenant_id(&state.store, &tenant_name).await?;
    let tx = state.db.begin().await?;
    let live = workspace::Entity::find_by_id(id)
        .one(&tx)
        .await?
        .or_not_found("workspace", format!("no workspace with id {id}"))?;
    // Ownership check.
    if live.tenant_id != tenant_id {
        return Err(ApiError::not_found(
            "workspace",
            format!("no workspace with id {id} under tenant {tenant_name:?}"),
        ));
    }
    if let Some(lock) = q.if_unmodified_since {
        check_lock(&lock, &live.updated_at, "workspace")?;
    }

    // workspace_plugin.workspace_id CASCADEs on workspace delete, so
    // no preflight refusal — but we count bindings for the audit
    // event AND walk them through the control-plane gate so live
    // sessions associated with the cascaded bindings are torn down.
    //
    // The cascade applies before the commit (the FK is enforced
    // inside the txn), so the live-state coordination MUST run
    // before the DELETE. Otherwise we'd lose the (workspace, plugin)
    // pairs we need to ask control-plane about.
    let bindings = workspace_plugin::Entity::find()
        .filter(workspace_plugin::Column::WorkspaceId.eq(id))
        .all(&tx)
        .await?;
    // Resolve plugin names so the live-gate call can pass the
    // (tenant, workspace, plugin) triple control-plane expects.
    let tenant = tenant::Entity::find_by_id(live.tenant_id)
        .one(&tx)
        .await?
        .or_not_found(
            "tenant",
            format!(
                "workspace {id} references missing tenant {}",
                live.tenant_id
            ),
        )?;

    let mut total_terminated = 0;
    for binding in &bindings {
        let plugin_row = plugin::Entity::find_by_id(binding.plugin_id)
            .one(&tx)
            .await?
            .or_not_found(
                "plugin",
                format!("binding {} references missing plugin", binding.plugin_id),
            )?;
        let outcome = terminate_live_sessions(
            &state.control_plane,
            &tenant.name,
            &live.name,
            &plugin_row.name,
        )
        .await;
        info!(
            "{PREFIX} delete workspace: binding ({tenant}, {ws}, {plugin}) {sum}",
            tenant = tenant.name,
            ws = live.name,
            plugin = plugin_row.name,
            sum = outcome_summary(&outcome),
        );
        match outcome {
            Ok(n) => total_terminated += n,
            Err(GateError::Disabled) => {} // break-glass; logged + continue
            Err(GateError::Unavailable(msg)) => {
                // tx is rolled back automatically when we drop without
                // commit; nothing else to undo.
                return Err(ApiError::unavailable(format!(
                    "control-plane unavailable mid-workspace-delete: {msg}; \
                     no rows were deleted"
                )));
            }
        }
    }

    workspace::Entity::delete_by_id(id).exec(&tx).await?;
    tx.commit().await?;

    audit_event(
        &op,
        "delete",
        "workspace",
        id,
        &format!(
            "cascaded_bindings={} live_sessions_terminated={total_terminated}",
            bindings.len()
        ),
    );
    Ok(StatusCode::NO_CONTENT)
}

// ── plugin ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginBody {
    /// Bare per-entry plugin spec, validated by `botwork-api-core`.
    /// We let the spec validator own field-shape; only `name` gets a
    /// regex check up-front so the error path is clear.
    name: String,
    image: Option<String>,
    port: Option<u64>,
    path: Option<String>,
    upstream_auth: Option<String>,
    env: Option<JsonValue>,
    resources: Option<JsonValue>,
    egress: Option<JsonValue>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginUpdateBody {
    name: String,
    image: Option<String>,
    port: Option<u64>,
    path: Option<String>,
    upstream_auth: Option<String>,
    env: Option<JsonValue>,
    resources: Option<JsonValue>,
    egress: Option<JsonValue>,
    if_unmodified_since: DateTime<Utc>,
}

/// api-core's validator takes a `RawPluginEntry` (its yaml-shape
/// input type) with `serde_yaml::Value` for the nested fields.
/// api's JSON inputs use `serde_json::Value`, so we convert
/// once at the boundary. `to_string` round-trip is fine for the
/// sizes involved (env/resources/egress are all <=64KiB by the
/// validator's own rules).
fn json_to_raw_plugin(body: PluginBody) -> Result<RawPluginEntry, ApiError> {
    fn json_to_yaml(v: JsonValue) -> Result<serde_yaml::Value, ApiError> {
        // Roundtrip via JSON text — both crates implement
        // Deserialize from JSON, but serde_yaml's Deserialize is
        // tag-aware in ways that don't compose with serde_json's
        // Value. Text is the lingua franca.
        let s = serde_json::to_string(&v).map_err(|err| {
            // coverage:off — serde_json::to_string is infallible for any JsonValue;
            // this arm exists for defensive completeness only.
            ApiError::bad_request(format!("could not re-serialise nested field: {err}"))
            // coverage:on
        })?;
        serde_yaml::from_str(&s).map_err(|err| {
            // coverage:off — valid JSON text (produced above) always deserialises as
            // serde_yaml::Value; this arm exists for defensive completeness only.
            ApiError::bad_request(format!("nested field not yaml-roundtrip-able: {err}"))
            // coverage:on
        })
    }
    Ok(RawPluginEntry {
        name: body.name,
        image: body.image,
        port: body.port,
        path: body.path,
        upstream_auth: body.upstream_auth,
        env: body.env.map(json_to_yaml).transpose()?,
        resources: body.resources.map(json_to_yaml).transpose()?,
        egress: body.egress.map(json_to_yaml).transpose()?,
        network: None,
    })
}

async fn create_plugin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(raw_body): Json<JsonValue>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&headers)?;
    let body: PluginBody = parse_body(raw_body)?;
    let raw = json_to_raw_plugin(body)?;
    let validated = validate_one(&raw)?; // 422 on rule break
                                         // Additionally validate against the Phase 2 name grammar.
    validate_plugin_name(&validated.name).map_err(ApiError::from)?;
    let op = operator(&headers);

    // Case-insensitive uniqueness check.
    let normalised = normalise_name(&validated.name);
    if plugin_name_taken(state.db.as_ref(), &normalised, None).await? {
        return Err(ApiError::already_exists(format!(
            "plugin with name {:?} already exists (case-insensitive)",
            validated.name
        )));
    }

    let id = Uuid::new_v4();
    let now = Utc::now();
    // Store the un-normalised name (preserve case); uniqueness is enforced
    // separately via LOWER(name) checks.
    let row = plugin::ActiveModel {
        id: Set(id),
        name: Set(validated.name.clone()),
        image: Set(validated.image),
        port: Set(i32::from(validated.port)),
        path: Set(validated.path),
        upstream_auth: Set(validated.upstream_auth),
        env: Set(validated.env),
        resources: Set(validated.resources),
        egress: Set(validated.egress),
        created_at: Set(now),
        updated_at: Set(now),
        // RFE #146: operator-intent row leaves `current_facet_id`
        // NULL on create; the `botwork-image-catalog` oneshot is
        // the only writer of that pointer.
        current_facet_id: sea_orm::ActiveValue::NotSet,
    }
    .insert(state.db.as_ref())
    .await?;

    audit_event(
        &op,
        "create",
        "plugin",
        row.id,
        &format!("name={:?}", row.name),
    );
    let mut response = (StatusCode::CREATED, Json(row)).into_response();
    response.headers_mut().insert(
        LOCATION,
        HeaderValue::from_str(&format!("/api/plugins/{id}")).expect("uuid is ascii"),
    );
    Ok(response)
}

async fn update_plugin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(raw_body): Json<JsonValue>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&headers)?;
    let body: PluginUpdateBody = parse_body(raw_body)?;
    let id = Uuid::from_str(&id).map_err(|err| bad_request("invalid plugin id", err))?;
    let op = operator(&headers);

    // Split out the lock token before we hand the rest to the
    // validator (which doesn't know about it).
    let if_unmodified_since = body.if_unmodified_since;
    let raw_body = PluginBody {
        name: body.name,
        image: body.image,
        port: body.port,
        path: body.path,
        upstream_auth: body.upstream_auth,
        env: body.env,
        resources: body.resources,
        egress: body.egress,
    };
    let raw = json_to_raw_plugin(raw_body)?;
    let validated = validate_one(&raw)?;

    let tx = state.db.begin().await?;
    let live = plugin::Entity::find_by_id(id)
        .one(&tx)
        .await?
        .or_not_found("plugin", format!("no plugin with id {id}"))?;
    check_lock(&if_unmodified_since, &live.updated_at, "plugin")?;

    // Additionally validate plugin name against Phase 2 grammar.
    validate_plugin_name(&validated.name).map_err(ApiError::from)?;
    // Case-insensitive uniqueness on rename: exclude current plugin.
    let normalised = normalise_name(&validated.name);
    if plugin_name_taken(&tx, &normalised, Some(live.id)).await? {
        return Err(ApiError::already_exists(format!(
            "plugin with name {:?} already exists (case-insensitive)",
            validated.name
        )));
    }

    let now = Utc::now();
    let mut active: plugin::ActiveModel = live.into();
    // Store the un-normalised name (preserve case); uniqueness is enforced
    // separately via LOWER(name) checks.
    active.name = Set(validated.name.clone());
    active.image = Set(validated.image);
    active.port = Set(i32::from(validated.port));
    active.path = Set(validated.path);
    active.upstream_auth = Set(validated.upstream_auth);
    active.env = Set(validated.env);
    active.resources = Set(validated.resources);
    active.egress = Set(validated.egress);
    active.updated_at = Set(now);
    let row = active.update(&tx).await?;
    tx.commit().await?;

    audit_event(&op, "update", "plugin", id, &format!("name={:?}", row.name));
    Ok((StatusCode::OK, Json(row)))
}

async fn delete_plugin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&headers)?;
    let id = Uuid::from_str(&id).map_err(|err| bad_request("invalid plugin id", err))?;
    let op = operator(&headers);

    let tx = state.db.begin().await?;
    let live = plugin::Entity::find_by_id(id)
        .one(&tx)
        .await?
        .or_not_found("plugin", format!("no plugin with id {id}"))?;
    if let Some(lock) = q.if_unmodified_since {
        check_lock(&lock, &live.updated_at, "plugin")?;
    }

    // Delete-guard: workspace_plugin.plugin_id is RESTRICT. List
    // bindings and resolve their (tenant, workspace) names for the
    // dependent payload.
    let bindings = workspace_plugin::Entity::find()
        .filter(workspace_plugin::Column::PluginId.eq(id))
        .find_also_related(workspace::Entity)
        .all(&tx)
        .await?;
    if !bindings.is_empty() {
        let mut summary = Vec::with_capacity(bindings.len());
        for (binding, ws_opt) in &bindings {
            let ws = ws_opt.as_ref().ok_or_else(|| ApiError::Internal {
                detail: format!(
                    "binding for workspace_id={} has no workspace row",
                    binding.workspace_id
                ),
            })?;
            let tenant_row = tenant::Entity::find_by_id(ws.tenant_id)
                .one(&tx)
                .await?
                .or_not_found(
                    "tenant",
                    format!(
                        "workspace {} references missing tenant {}",
                        ws.id, ws.tenant_id
                    ),
                )?;
            summary.push(format!("{}/{}", tenant_row.name, ws.name));
        }
        return Err(ApiError::has_dependents(format!(
            "plugin '{}' still bound in {} workspace(s): {:?}",
            live.name,
            bindings.len(),
            summary
        )));
    }

    plugin::Entity::delete_by_id(id).exec(&tx).await?;
    tx.commit().await?;

    audit_event(
        &op,
        "delete",
        "plugin",
        id,
        &format!("name={:?}", live.name),
    );
    Ok(StatusCode::NO_CONTENT)
}

// ── workspace_plugin (binding) ─────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspacePluginCreate {
    workspace_id: Uuid,
    plugin_id: Uuid,
    config: Option<JsonValue>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspacePluginUpdate {
    /// `config: null` clears the binding's per-binding config.
    /// Absent field = no change. To distinguish, callers must
    /// include the field explicitly with `null`.
    config: Option<JsonValue>,
    if_unmodified_since: DateTime<Utc>,
}

/// Resolve a (workspace_id, plugin_id) pair into the `(tenant,
/// workspace, plugin)` name triple control-plane keys by. Used by
/// every binding mutation that talks to control-plane.
async fn resolve_triple(
    tx: &sea_orm::DatabaseTransaction,
    workspace_id: Uuid,
    plugin_id: Uuid,
) -> Result<(String, String, String), ApiError> {
    use sea_orm::{FromQueryResult, Statement};
    #[derive(FromQueryResult)]
    struct Row {
        tenant: String,
        workspace: String,
        plugin: String,
    }
    let backend = tx.get_database_backend();
    let stmt = Statement::from_sql_and_values(
        backend,
        r#"
        SELECT t.name AS tenant,
               w.name AS workspace,
               p.name AS plugin
        FROM workspace w
        JOIN tenant t ON t.id = w.tenant_id
        CROSS JOIN plugin p
        WHERE w.id = $1 AND p.id = $2
        "#,
        vec![workspace_id.into(), plugin_id.into()],
    );
    let row = Row::find_by_statement(stmt).one(tx).await?.or_not_found(
        "workspace_plugin",
        format!(
            "could not resolve (workspace={workspace_id}, plugin={plugin_id}) — \
                 one of the rows is missing"
        ),
    )?;
    Ok((row.tenant, row.workspace, row.plugin))
}

async fn create_workspace_plugin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tenant_name): Path<String>,
    Json(raw_body): Json<JsonValue>,
) -> Result<impl IntoResponse, ApiError> {
    check_tenant_consistency(&headers, &tenant_name)?;
    let body: WorkspacePluginCreate = parse_body(raw_body)?;
    let op = operator(&headers);

    let tx = state.db.begin().await?;
    // Verify both parents and resolve names for the control-plane
    // gate / audit event in one shot.
    let (tenant_name, workspace_name, plugin_name) =
        resolve_triple(&tx, body.workspace_id, body.plugin_id).await?;

    if workspace_plugin::Entity::find_by_id((body.workspace_id, body.plugin_id))
        .one(&tx)
        .await?
        .is_some()
    {
        return Err(ApiError::already_exists(format!(
            "binding (workspace={}, plugin={}) already exists",
            body.workspace_id, body.plugin_id
        )));
    }

    let config = match body.config {
        Some(JsonValue::Null) | None => None,
        Some(JsonValue::Object(map)) if map.is_empty() => None,
        Some(v @ JsonValue::Object(_)) => Some(v),
        Some(other) => {
            return Err(ApiError::validation_failed(format!(
                "binding 'config' must be a JSON object (got {})",
                json_type(&other)
            )));
        }
    };

    let now = Utc::now();
    let row = workspace_plugin::ActiveModel {
        workspace_id: Set(body.workspace_id),
        plugin_id: Set(body.plugin_id),
        config: Set(config.clone()),
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(&tx)
    .await?;

    // No live-state coupling on CREATE: a brand new binding can't
    // affect existing sessions (they predate it). control-plane
    // will receive policy via the existing spawn path on the next
    // request that resolves to this binding.

    tx.commit().await?;

    audit_event(
        &op,
        "create",
        "workspace_plugin",
        format!("{}/{}", body.workspace_id, body.plugin_id),
        &format!(
            "triple=({tenant_name}/{workspace_name}/{plugin_name}) has_config={}",
            config.is_some()
        ),
    );
    let mut response = (StatusCode::CREATED, Json(row)).into_response();
    response.headers_mut().insert(
        LOCATION,
        HeaderValue::from_str(&format!(
            "/api/tenant/{tenant_name}/workspace_plugins/{}/{}",
            body.workspace_id, body.plugin_id
        ))
        .expect("tenant_name and uuid are ascii-safe"),
    );
    Ok(response)
}

async fn update_workspace_plugin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((tenant_name, workspace_id, plugin_id)): Path<(String, String, String)>,
    Json(raw_body): Json<JsonValue>,
) -> Result<impl IntoResponse, ApiError> {
    check_tenant_consistency(&headers, &tenant_name)?;
    let body: WorkspacePluginUpdate = parse_body(raw_body)?;
    let workspace_id = Uuid::from_str(&workspace_id)
        .map_err(|err| bad_request("invalid workspace_id path param", err))?;
    let plugin_id = Uuid::from_str(&plugin_id)
        .map_err(|err| bad_request("invalid plugin_id path param", err))?;
    let op = operator(&headers);

    let tx = state.db.begin().await?;
    let live = workspace_plugin::Entity::find_by_id((workspace_id, plugin_id))
        .one(&tx)
        .await?
        .or_not_found(
            "workspace_plugin",
            format!("no binding for (workspace={workspace_id}, plugin={plugin_id})"),
        )?;
    check_lock(
        &body.if_unmodified_since,
        &live.updated_at,
        "workspace_plugin",
    )?;

    let new_config = match body.config {
        Some(JsonValue::Null) | None => None,
        Some(JsonValue::Object(map)) if map.is_empty() => None,
        Some(v @ JsonValue::Object(_)) => Some(v),
        Some(other) => {
            return Err(ApiError::validation_failed(format!(
                "binding 'config' must be a JSON object (got {})",
                json_type(&other)
            )));
        }
    };

    let now = Utc::now();
    let (tenant_name, workspace_name, plugin_name) =
        resolve_triple(&tx, workspace_id, plugin_id).await?;

    // Live-state coupling: a config change updates the policy
    // control-plane has, so we tear down any live sessions for the
    // triple. Next spawn picks up the new config.
    let live_outcome = terminate_live_sessions(
        &state.control_plane,
        &tenant_name,
        &workspace_name,
        &plugin_name,
    )
    .await;
    if let Err(GateError::Unavailable(msg)) = &live_outcome {
        return Err(ApiError::unavailable(format!(
            "control-plane unavailable during workspace_plugin update: {msg}; no rows changed"
        )));
    }

    let mut active: workspace_plugin::ActiveModel = live.into();
    active.config = Set(new_config.clone());
    active.updated_at = Set(now);
    let row = active.update(&tx).await?;
    tx.commit().await?;

    audit_event(
        &op,
        "update",
        "workspace_plugin",
        format!("{workspace_id}/{plugin_id}"),
        &format!(
            "triple=({tenant_name}/{workspace_name}/{plugin_name}) {sum} has_config={}",
            new_config.is_some(),
            sum = outcome_summary(&live_outcome),
        ),
    );
    Ok((StatusCode::OK, Json(row)))
}

async fn delete_workspace_plugin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((tenant_name, workspace_id, plugin_id)): Path<(String, String, String)>,
    Query(q): Query<DeleteQuery>,
) -> Result<impl IntoResponse, ApiError> {
    check_tenant_consistency(&headers, &tenant_name)?;
    let workspace_id = Uuid::from_str(&workspace_id)
        .map_err(|err| bad_request("invalid workspace_id path param", err))?;
    let plugin_id = Uuid::from_str(&plugin_id)
        .map_err(|err| bad_request("invalid plugin_id path param", err))?;
    let op = operator(&headers);

    let tx = state.db.begin().await?;
    let live = workspace_plugin::Entity::find_by_id((workspace_id, plugin_id))
        .one(&tx)
        .await?
        .or_not_found(
            "workspace_plugin",
            format!("no binding for (workspace={workspace_id}, plugin={plugin_id})"),
        )?;
    if let Some(lock) = q.if_unmodified_since {
        check_lock(&lock, &live.updated_at, "workspace_plugin")?;
    }

    let (tenant_name, workspace_name, plugin_name) =
        resolve_triple(&tx, workspace_id, plugin_id).await?;

    let live_outcome = terminate_live_sessions(
        &state.control_plane,
        &tenant_name,
        &workspace_name,
        &plugin_name,
    )
    .await;
    if let Err(GateError::Unavailable(msg)) = &live_outcome {
        return Err(ApiError::unavailable(format!(
            "control-plane unavailable during workspace_plugin delete: {msg}; no rows deleted"
        )));
    }

    workspace_plugin::Entity::delete_by_id((workspace_id, plugin_id))
        .exec(&tx)
        .await?;
    tx.commit().await?;

    audit_event(
        &op,
        "delete",
        "workspace_plugin",
        format!("{workspace_id}/{plugin_id}"),
        &format!(
            "triple=({tenant_name}/{workspace_name}/{plugin_name}) {sum}",
            sum = outcome_summary(&live_outcome),
        ),
    );
    Ok(StatusCode::NO_CONTENT)
}

// ── secrets ─────────────────────────────────────────────────────────
//
// No optimistic locking on secrets in this PR. The secret-store
// contract doesn't currently expose an `updated_at` token; adding the
// lock token can wait until the backend supports it.

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretCreate {
    service: String,
    name: String,
    kind: String,
    value_b64: String,
    #[serde(default)]
    allowed_consumers: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    overwrite: bool,
}

async fn create_secret(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tenant): Path<String>,
    Json(raw_body): Json<JsonValue>,
) -> Result<impl IntoResponse, ApiError> {
    check_tenant_consistency(&headers, &tenant)?;
    let body: SecretCreate = parse_body(raw_body)?;
    let service = require_secret_component("service", &body.service)?;
    let name = require_secret_component("name", &body.name)?;
    // kind and value_b64 are opaque to api — the backend is the
    // authority on what kinds it accepts and whether the base64 decodes.
    let op = operator(&headers);

    let req = PutSecretRequest {
        tenant: tenant.clone(),
        service: service.clone(),
        name: name.clone(),
        kind: body.kind.clone(),
        value_b64: body.value_b64.clone(),
        allowed_consumers: body.allowed_consumers.clone(),
        tags: body.tags.clone(),
        overwrite: body.overwrite,
    };

    let resp = state
        .secret_store
        .put_secret(req)
        .await
        .map_err(|err| match err {
            SecretStoreError::Disabled => ApiError::unavailable(
                "secret-store is disabled (break-glass); secret was NOT stored",
            ),
            SecretStoreError::Unavailable(msg) => ApiError::unavailable(format!(
                "secret-store unavailable: {msg}; secret was NOT stored"
            )),
            SecretStoreError::AlreadyExists(msg) => ApiError::already_exists(msg),
            SecretStoreError::BadRequest(msg) => ApiError::bad_request(msg),
            // coverage:off — SecretStoreClient::put_secret never returns NotFound;
            // this arm covers the full enum for exhaustiveness.
            SecretStoreError::NotFound(msg) => ApiError::Internal {
                detail: format!("unexpected NotFound from secret-store on POST: {msg}"),
            },
            // coverage:on
        })?;

    audit_event(
        &op,
        "create",
        "secret",
        format!("{service}/{name}"),
        &format!(
            "tenant={tenant:?} kind={kind:?} created={created} overwrite={overwrite}",
            kind = body.kind,
            created = resp.created,
            overwrite = body.overwrite,
        ),
    );

    // Evict stale-credential containers for this tenant so the next
    // request re-enters the spawn path and re-fetches secrets.
    // Fire-and-forget: the secret is already stored; eviction failure
    // is non-fatal (logged as a warning inside signal_evict).
    signal_evict(&state.session_broker, &tenant).await;

    let mut response = (StatusCode::CREATED, Json(resp)).into_response();
    response.headers_mut().insert(
        LOCATION,
        HeaderValue::from_str(&format!("/api/tenant/{tenant}/secrets/{service}/{name}"))
            .expect("tenant, service and name are ascii-safe"),
    );
    Ok(response)
}

async fn delete_secret(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((tenant, service, name)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    check_tenant_consistency(&headers, &tenant)?;
    let service = require_secret_component("service", &service)?;
    let name = require_secret_component("name", &name)?;
    let op = operator(&headers);

    state
        .secret_store
        .delete_secret(&tenant, &service, &name)
        .await
        .map_err(|err| match err {
            SecretStoreError::Disabled => ApiError::unavailable(
                "secret-store is disabled (break-glass); secret was NOT deleted",
            ),
            SecretStoreError::Unavailable(msg) => ApiError::unavailable(format!(
                "secret-store unavailable: {msg}; secret was NOT deleted"
            )),
            SecretStoreError::NotFound(_) => {
                ApiError::not_found("secret", format!("{service}/{name}"))
            }
            SecretStoreError::BadRequest(msg) => ApiError::bad_request(msg),
            // coverage:off — SecretStoreClient::delete_secret never returns AlreadyExists;
            // this arm covers the full enum for exhaustiveness.
            SecretStoreError::AlreadyExists(msg) => ApiError::Internal {
                detail: format!("unexpected AlreadyExists from secret-store on DELETE: {msg}"),
            },
            // coverage:on
        })?;

    audit_event(
        &op,
        "delete",
        "secret",
        format!("{service}/{name}"),
        &format!("tenant={tenant:?}"),
    );

    // Evict stale-credential containers for this tenant so the next
    // request re-enters the spawn path and re-fetches secrets.
    // Fire-and-forget: eviction failure is non-fatal.
    signal_evict(&state.session_broker, &tenant).await;

    Ok(StatusCode::NO_CONTENT)
}

// ── shared helpers ─────────────────────────────────────────────────

fn json_type(v: &JsonValue) -> &'static str {
    match v {
        // coverage:off — Null and Object are matched by dedicated arms in every
        // call site before reaching the `Some(other)` fallthrough that calls this
        // function; these two arms exist for completeness only.
        JsonValue::Null => "null",
        // coverage:on
        JsonValue::Bool(_) => "bool",
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        // coverage:off — see Null note above.
        JsonValue::Object(_) => "object",
        // coverage:on
    }
}

// Suppress unused-import warning until pagination helpers land in a
// follow-up. PaginatorTrait + JoinType + DatabaseConnection are kept
// in the use-block above so the next set of handlers (filtered
// counts, JOIN-shaped lookups) can pull them in without re-shuffling.
#[allow(dead_code)]
fn _imports_kept(_: &DatabaseConnection) -> JoinType {
    JoinType::InnerJoin
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use axum::body::{to_bytes, Body};
    use axum::response::IntoResponse;
    use chrono::SecondsFormat;
    use http::{Request, StatusCode};
    use sea_orm::{DatabaseBackend, MockDatabase, MockExecResult};
    use tower::ServiceExt;
    use uuid::Uuid;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::store::mock::MockApiStore;
    use crate::store::sea_orm_impl::SeaOrmApiStore;
    use crate::{AppState, ControlPlaneClient, SecretStoreClient, SessionBrokerClient};

    fn fixed_time() -> chrono::DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
            .expect("fixed timestamp")
            .with_timezone(&Utc)
    }

    fn admin_request(method: &str, path: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header(crate::handler::ADMIN_HEADER, "ops")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request")
    }

    fn request(method: &str, path: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request")
    }

    fn delete_request(path: &str) -> Request<Body> {
        Request::builder()
            .method("DELETE")
            .uri(path)
            .header(crate::handler::ADMIN_HEADER, "ops")
            .body(Body::empty())
            .expect("request")
    }

    fn tenant_request(
        method: &str,
        path: &str,
        tenant: &str,
        body: serde_json::Value,
    ) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header(crate::handler::TENANT_HEADER, tenant)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request")
    }

    fn tenant_delete_request(path: &str, tenant: &str) -> Request<Body> {
        Request::builder()
            .method("DELETE")
            .uri(path)
            .header(crate::handler::TENANT_HEADER, tenant)
            .body(Body::empty())
            .expect("request")
    }

    fn tenant_row(id: Uuid, name: &str, updated_at: chrono::DateTime<Utc>) -> tenant::Model {
        tenant::Model {
            id,
            name: name.to_string(),
            created_at: updated_at,
            updated_at,
        }
    }

    fn workspace_row(id: Uuid, tenant_id: Uuid, name: &str) -> workspace::Model {
        workspace::Model {
            id,
            tenant_id,
            name: name.to_string(),
            created_at: fixed_time(),
            updated_at: fixed_time(),
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
            created_at: fixed_time(),
            updated_at: fixed_time(),
            current_facet_id: None,
        }
    }

    fn workspace_plugin_row(
        workspace_id: Uuid,
        plugin_id: Uuid,
        config: Option<JsonValue>,
    ) -> workspace_plugin::Model {
        workspace_plugin::Model {
            workspace_id,
            plugin_id,
            config,
            created_at: fixed_time(),
            updated_at: fixed_time(),
        }
    }

    fn count_row(cnt: i64) -> BTreeMap<String, sea_orm::Value> {
        BTreeMap::from([("cnt".to_string(), cnt.into())])
    }

    fn triple_row(tenant: &str, workspace: &str, plugin: &str) -> BTreeMap<String, sea_orm::Value> {
        BTreeMap::from([
            ("tenant".to_string(), tenant.to_string().into()),
            ("workspace".to_string(), workspace.to_string().into()),
            ("plugin".to_string(), plugin.to_string().into()),
        ])
    }

    fn app_state_with_mock_db_and_clients(
        mock: MockDatabase,
        control_plane: ControlPlaneClient,
        secret_store: SecretStoreClient,
        session_broker: SessionBrokerClient,
    ) -> AppState {
        let db = Arc::new(mock.into_connection());
        AppState {
            store: Arc::new(SeaOrmApiStore::new_shared(db.clone())),
            db,
            control_plane,
            secret_store,
            session_broker,
        }
    }

    fn app_state_with_mock_store_db_and_clients(
        store: MockApiStore,
        mock: MockDatabase,
        control_plane: ControlPlaneClient,
        secret_store: SecretStoreClient,
        session_broker: SessionBrokerClient,
    ) -> AppState {
        AppState {
            db: Arc::new(mock.into_connection()),
            store: Arc::new(store),
            control_plane,
            secret_store,
            session_broker,
        }
    }

    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        serde_json::from_slice(&body).expect("json body")
    }

    #[tokio::test]
    async fn create_tenant_requires_admin_header() {
        let state = crate::test_support::app_state_with_mock_store(MockApiStore::new());
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(request(
                "POST",
                "/api/tenants",
                serde_json::json!({ "name": "phlax" }),
            ))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(json_body(response).await["error"]["code"], "admin_required");
    }

    #[tokio::test]
    async fn create_workspace_requires_matching_tenant_header() {
        let state = crate::test_support::app_state_with_mock_store(MockApiStore::new());
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(request(
                "POST",
                "/api/tenant/phlax/workspaces",
                serde_json::json!({ "name": "mcp" }),
            ))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            json_body(response).await["error"]["code"],
            "cross_tenant_forbidden"
        );
    }

    #[tokio::test]
    async fn create_tenant_rejects_unknown_field_and_invalid_shape() {
        let state = crate::test_support::app_state_with_mock_store(MockApiStore::new());
        let app = crate::handler::build_router(state);

        let unknown_response = app
            .clone()
            .oneshot(admin_request(
                "POST",
                "/api/tenants",
                serde_json::json!({ "name": "phlax", "extra": true }),
            ))
            .await
            .expect("response");
        assert_eq!(unknown_response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(unknown_response).await["error"]["code"],
            "bad_request"
        );

        let malformed_response = app
            .oneshot(admin_request(
                "POST",
                "/api/tenants",
                serde_json::json!({ "name": 7 }),
            ))
            .await
            .expect("response");
        assert_eq!(malformed_response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(malformed_response).await["error"]["code"],
            "bad_request"
        );
    }

    #[tokio::test]
    async fn create_tenant_rejects_missing_required_field() {
        let state = crate::test_support::app_state_with_mock_store(MockApiStore::new());
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(admin_request("POST", "/api/tenants", serde_json::json!({})))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(response).await["error"]["code"], "bad_request");
    }

    #[tokio::test]
    async fn create_tenant_rejects_invalid_and_reserved_names() {
        let state = crate::test_support::app_state_with_mock_store(MockApiStore::new());
        let app = crate::handler::build_router(state);

        let invalid_response = app
            .clone()
            .oneshot(admin_request(
                "POST",
                "/api/tenants",
                serde_json::json!({ "name": "bad.name" }),
            ))
            .await
            .expect("response");
        assert_eq!(invalid_response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(invalid_response).await["error"]["code"],
            "invalid_name"
        );

        let reserved_response = app
            .oneshot(admin_request(
                "POST",
                "/api/tenants",
                serde_json::json!({ "name": "admin" }),
            ))
            .await
            .expect("response");
        assert_eq!(reserved_response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(reserved_response).await["error"]["code"],
            "reserved_name"
        );
    }

    #[tokio::test]
    async fn create_plugin_maps_validator_failure_to_422() {
        let state = crate::test_support::app_state_with_mock_store(MockApiStore::new());
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(admin_request(
                "POST",
                "/api/plugins",
                serde_json::json!({
                    "name": "mcp-fetch",
                    "image": "ghcr.io/example/mcp-fetch:1.0",
                    "port": 70000
                }),
            ))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            json_body(response).await["error"]["code"],
            "validation_failed"
        );
    }

    #[tokio::test]
    async fn create_tenant_maps_mock_db_error_to_internal() {
        let state =
            crate::test_support::app_state_with_mock_store(MockApiStore::always_error("boom"));
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(admin_request(
                "POST",
                "/api/tenants",
                serde_json::json!({ "name": "phlax" }),
            ))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(json_body(response).await["error"]["code"], "internal");
    }

    #[tokio::test]
    async fn create_tenant_success_sets_location_and_records_write() {
        let store = MockApiStore::new();
        let state = crate::test_support::app_state_with_mock_store(store.clone());
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(admin_request(
                "POST",
                "/api/tenants",
                serde_json::json!({ "name": "phlax" }),
            ))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::CREATED);
        let location = response
            .headers()
            .get(http::header::LOCATION)
            .expect("location header")
            .to_str()
            .expect("location ascii")
            .to_string();
        let body = json_body(response).await;
        let id = Uuid::parse_str(body["id"].as_str().expect("id as str")).expect("uuid");
        assert_eq!(body["name"], "phlax");
        assert_eq!(location, format!("/api/tenants/{id}"));
        assert_eq!(store.drain_created_tenants().await, vec![id]);
    }

    #[tokio::test]
    async fn update_tenant_rejects_invalid_id_and_missing_lock() {
        let state = crate::test_support::app_state_with_mock_store(MockApiStore::new());
        let app = crate::handler::build_router(state);

        let invalid_id = app
            .clone()
            .oneshot(admin_request(
                "PUT",
                "/api/tenants/not-a-uuid",
                serde_json::json!({
                    "name": "phlax",
                    "if_unmodified_since": fixed_time().to_rfc3339_opts(SecondsFormat::Micros, true)
                }),
            ))
            .await
            .expect("response");
        assert_eq!(invalid_id.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(invalid_id).await["error"]["code"], "bad_request");

        let missing_lock = app
            .oneshot(admin_request(
                "PUT",
                &format!("/api/tenants/{}", Uuid::new_v4()),
                serde_json::json!({ "name": "phlax" }),
            ))
            .await
            .expect("response");
        assert_eq!(missing_lock.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(missing_lock).await["error"]["code"],
            "bad_request"
        );
    }

    #[tokio::test]
    async fn update_tenant_stale_lock_returns_conflict() {
        let id = Uuid::new_v4();
        let live_updated_at = fixed_time();
        let newer_timestamp = live_updated_at + chrono::Duration::seconds(1);
        let state = crate::test_support::app_state_with_mock_store(
            MockApiStore::new().with_tenant(tenant_row(id, "phlax", live_updated_at)),
        );
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(admin_request(
                "PUT",
                &format!("/api/tenants/{id}"),
                serde_json::json!({
                    "name": "phlax",
                    "if_unmodified_since": newer_timestamp.to_rfc3339_opts(SecondsFormat::Micros, true)
                }),
            ))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(json_body(response).await["error"]["code"], "stale_write");
    }

    #[tokio::test]
    async fn update_tenant_with_matching_lock_updates_and_records_write() {
        let id = Uuid::new_v4();
        let updated_at = fixed_time();
        let store = MockApiStore::new().with_tenant(tenant_row(id, "phlax", updated_at));
        let state = crate::test_support::app_state_with_mock_store(store.clone());
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(admin_request(
                "PUT",
                &format!("/api/tenants/{id}"),
                serde_json::json!({
                    "name": "renamed",
                    "if_unmodified_since": updated_at.to_rfc3339_opts(SecondsFormat::Micros, true)
                }),
            ))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["id"], id.to_string());
        assert_eq!(body["name"], "renamed");
        assert_eq!(store.drain_updated_tenants().await, vec![id]);
    }

    #[tokio::test]
    async fn delete_tenant_rejects_invalid_id_and_invalid_lock_query() {
        let state = crate::test_support::app_state_with_mock_store(MockApiStore::new());
        let app = crate::handler::build_router(state);

        let invalid_id = app
            .clone()
            .oneshot(delete_request("/api/tenants/not-a-uuid"))
            .await
            .expect("response");
        assert_eq!(invalid_id.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(invalid_id).await["error"]["code"], "bad_request");

        let invalid_lock = app
            .oneshot(delete_request(&format!(
                "/api/tenants/{}?if_unmodified_since=not-a-timestamp",
                Uuid::new_v4()
            )))
            .await
            .expect("response");
        assert_eq!(invalid_lock.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn delete_tenant_with_matching_lock_and_dependents_returns_has_dependents() {
        let id = Uuid::new_v4();
        let updated_at = fixed_time();
        let workspace_id = Uuid::new_v4();
        let state = crate::test_support::app_state_with_mock_store(
            MockApiStore::new()
                .with_tenant(tenant_row(id, "phlax", updated_at))
                .with_workspace(workspace_row(workspace_id, id, "mcp")),
        );
        let app = crate::handler::build_router(state);
        let lock = updated_at.to_rfc3339_opts(SecondsFormat::Micros, true);

        let response = app
            .oneshot(delete_request(&format!(
                "/api/tenants/{id}?if_unmodified_since={lock}"
            )))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(json_body(response).await["error"]["code"], "has_dependents");
    }

    #[tokio::test]
    async fn delete_tenant_with_no_dependents_proceeds() {
        let id = Uuid::new_v4();
        let updated_at = fixed_time();
        let store = MockApiStore::new().with_tenant(tenant_row(id, "phlax", updated_at));
        let state = crate::test_support::app_state_with_mock_store(store.clone());
        let app = crate::handler::build_router(state);
        let lock = updated_at.to_rfc3339_opts(SecondsFormat::Micros, true);

        let response = app
            .oneshot(delete_request(&format!(
                "/api/tenants/{id}?if_unmodified_since={lock}"
            )))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(store.drain_deleted_tenants().await, vec![id]);
    }

    #[tokio::test]
    async fn create_workspace_rejects_unknown_field_and_invalid_name() {
        let state = crate::test_support::app_state_with_mock_store(MockApiStore::new());
        let app = crate::handler::build_router(state);

        let unknown_field = app
            .clone()
            .oneshot(tenant_request(
                "POST",
                "/api/tenant/phlax/workspaces",
                "phlax",
                serde_json::json!({ "name": "mcp", "extra": true }),
            ))
            .await
            .expect("response");
        assert_eq!(unknown_field.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(unknown_field).await["error"]["code"],
            "bad_request"
        );

        let invalid_name = app
            .oneshot(tenant_request(
                "POST",
                "/api/tenant/phlax/workspaces",
                "phlax",
                serde_json::json!({ "name": "bad.name" }),
            ))
            .await
            .expect("response");
        assert_eq!(invalid_name.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(invalid_name).await["error"]["code"],
            "invalid_name"
        );
    }

    #[tokio::test]
    async fn workspace_and_plugin_mutations_reject_invalid_path_ids() {
        let state = crate::test_support::app_state_with_mock_store(MockApiStore::new());
        let app = crate::handler::build_router(state);
        let lock = fixed_time().to_rfc3339_opts(SecondsFormat::Micros, true);

        let update_workspace = app
            .clone()
            .oneshot(tenant_request(
                "PUT",
                "/api/tenant/phlax/workspaces/not-a-uuid",
                "phlax",
                serde_json::json!({
                    "name": "mcp",
                    "if_unmodified_since": lock
                }),
            ))
            .await
            .expect("response");
        assert_eq!(update_workspace.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(update_workspace).await["error"]["code"],
            "bad_request"
        );

        let delete_workspace = app
            .clone()
            .oneshot(tenant_delete_request(
                "/api/tenant/phlax/workspaces/not-a-uuid",
                "phlax",
            ))
            .await
            .expect("response");
        assert_eq!(delete_workspace.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(delete_workspace).await["error"]["code"],
            "bad_request"
        );

        let update_plugin = app
            .clone()
            .oneshot(admin_request(
                "PUT",
                "/api/plugins/not-a-uuid",
                serde_json::json!({
                    "name": "mcp-fetch",
                    "image": "ghcr.io/example/mcp-fetch:1.0",
                    "port": 8000,
                    "if_unmodified_since": fixed_time().to_rfc3339_opts(SecondsFormat::Micros, true)
                }),
            ))
            .await
            .expect("response");
        assert_eq!(update_plugin.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(update_plugin).await["error"]["code"],
            "bad_request"
        );

        let delete_plugin = app
            .oneshot(delete_request("/api/plugins/not-a-uuid"))
            .await
            .expect("response");
        assert_eq!(delete_plugin.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(delete_plugin).await["error"]["code"],
            "bad_request"
        );
    }

    #[tokio::test]
    async fn create_plugin_rejects_missing_required_name() {
        let state = crate::test_support::app_state_with_mock_store(MockApiStore::new());
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(admin_request(
                "POST",
                "/api/plugins",
                serde_json::json!({
                    "image": "ghcr.io/example/mcp-fetch:1.0",
                    "port": 8000
                }),
            ))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(response).await["error"]["code"], "bad_request");
    }

    #[tokio::test]
    async fn create_and_delete_secret_reject_invalid_components() {
        let state = crate::test_support::app_state_with_mock_store(MockApiStore::new());
        let app = crate::handler::build_router(state);

        let create_invalid = app
            .clone()
            .oneshot(tenant_request(
                "POST",
                "/api/tenant/phlax/secrets",
                "phlax",
                serde_json::json!({
                    "service": "../github",
                    "name": "pat",
                    "kind": "token",
                    "value_b64": "dG9rZW4="
                }),
            ))
            .await
            .expect("response");
        assert_eq!(create_invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            json_body(create_invalid).await["error"]["code"],
            "validation_failed"
        );

        let delete_invalid = app
            .oneshot(tenant_delete_request(
                "/api/tenant/phlax/secrets/github/.env",
                "phlax",
            ))
            .await
            .expect("response");
        assert_eq!(delete_invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            json_body(delete_invalid).await["error"]["code"],
            "validation_failed"
        );
    }

    #[tokio::test]
    async fn workspace_handlers_cover_direct_db_create_and_update_branches() {
        let tenant_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let tenant_store =
            MockApiStore::new().with_tenant(tenant_row(tenant_id, "phlax", fixed_time()));

        let create_state = app_state_with_mock_store_db_and_clients(
            tenant_store.clone(),
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![count_row(0)]])
                .append_query_results([vec![workspace_row(workspace_id, tenant_id, "mcp")]]),
            ControlPlaneClient::disabled(),
            SecretStoreClient::disabled(),
            SessionBrokerClient::disabled(),
        );
        let create_response = crate::handler::build_router(create_state)
            .oneshot(tenant_request(
                "POST",
                "/api/tenant/phlax/workspaces",
                "phlax",
                serde_json::json!({ "name": "mcp" }),
            ))
            .await
            .expect("response");
        assert_eq!(create_response.status(), StatusCode::CREATED);
        let create_location = create_response
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        assert!(create_location
            .as_deref()
            .expect("location")
            .starts_with("/api/tenant/phlax/workspaces/"));
        let _ = json_body(create_response).await;

        let create_taken_state = app_state_with_mock_store_db_and_clients(
            tenant_store.clone(),
            MockDatabase::new(DatabaseBackend::Postgres).append_query_results([vec![count_row(1)]]),
            ControlPlaneClient::disabled(),
            SecretStoreClient::disabled(),
            SessionBrokerClient::disabled(),
        );
        let create_taken = crate::handler::build_router(create_taken_state)
            .oneshot(tenant_request(
                "POST",
                "/api/tenant/phlax/workspaces",
                "phlax",
                serde_json::json!({ "name": "mcp" }),
            ))
            .await
            .expect("response");
        assert_eq!(create_taken.status(), StatusCode::CONFLICT);
        assert_eq!(
            json_body(create_taken).await["error"]["code"],
            "already_exists"
        );

        let create_missing_tenant =
            crate::handler::build_router(app_state_with_mock_store_db_and_clients(
                MockApiStore::new(),
                MockDatabase::new(DatabaseBackend::Postgres),
                ControlPlaneClient::disabled(),
                SecretStoreClient::disabled(),
                SessionBrokerClient::disabled(),
            ))
            .oneshot(tenant_request(
                "POST",
                "/api/tenant/phlax/workspaces",
                "phlax",
                serde_json::json!({ "name": "mcp" }),
            ))
            .await
            .expect("response");
        assert_eq!(create_missing_tenant.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json_body(create_missing_tenant).await["error"]["code"],
            "not_found"
        );

        let live_workspace = workspace_row(workspace_id, tenant_id, "mcp");
        let updated_workspace = workspace_row(workspace_id, tenant_id, "renamed");
        let update_response =
            crate::handler::build_router(app_state_with_mock_store_db_and_clients(
                tenant_store.clone(),
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([vec![live_workspace.clone()]])
                    .append_query_results([vec![count_row(0)]])
                    .append_query_results([vec![updated_workspace]]),
                ControlPlaneClient::disabled(),
                SecretStoreClient::disabled(),
                SessionBrokerClient::disabled(),
            ))
            .oneshot(tenant_request(
                "PUT",
                &format!("/api/tenant/phlax/workspaces/{workspace_id}"),
                "phlax",
                serde_json::json!({
                    "name": "renamed",
                    "if_unmodified_since": fixed_time().to_rfc3339_opts(SecondsFormat::Micros, true)
                }),
            ))
            .await
            .expect("response");
        assert_eq!(update_response.status(), StatusCode::OK);
        assert_eq!(json_body(update_response).await["name"], "renamed");

        let update_missing =
            crate::handler::build_router(app_state_with_mock_store_db_and_clients(
                tenant_store.clone(),
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([Vec::<workspace::Model>::new()]),
                ControlPlaneClient::disabled(),
                SecretStoreClient::disabled(),
                SessionBrokerClient::disabled(),
            ))
            .oneshot(tenant_request(
                "PUT",
                &format!("/api/tenant/phlax/workspaces/{workspace_id}"),
                "phlax",
                serde_json::json!({
                    "name": "renamed",
                    "if_unmodified_since": fixed_time().to_rfc3339_opts(SecondsFormat::Micros, true)
                }),
            ))
            .await
            .expect("response");
        assert_eq!(update_missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json_body(update_missing).await["error"]["code"],
            "not_found"
        );

        let update_mismatch =
            crate::handler::build_router(app_state_with_mock_store_db_and_clients(
                tenant_store.clone(),
                MockDatabase::new(DatabaseBackend::Postgres).append_query_results([vec![
                    workspace_row(workspace_id, Uuid::new_v4(), "mcp"),
                ]]),
                ControlPlaneClient::disabled(),
                SecretStoreClient::disabled(),
                SessionBrokerClient::disabled(),
            ))
            .oneshot(tenant_request(
                "PUT",
                &format!("/api/tenant/phlax/workspaces/{workspace_id}"),
                "phlax",
                serde_json::json!({
                    "name": "renamed",
                    "if_unmodified_since": fixed_time().to_rfc3339_opts(SecondsFormat::Micros, true)
                }),
            ))
            .await
            .expect("response");
        assert_eq!(update_mismatch.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json_body(update_mismatch).await["error"]["code"],
            "not_found"
        );

        let stale_response =
            crate::handler::build_router(app_state_with_mock_store_db_and_clients(
                tenant_store.clone(),
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([vec![live_workspace.clone()]]),
                ControlPlaneClient::disabled(),
                SecretStoreClient::disabled(),
                SessionBrokerClient::disabled(),
            ))
            .oneshot(tenant_request(
                "PUT",
                &format!("/api/tenant/phlax/workspaces/{workspace_id}"),
                "phlax",
                serde_json::json!({
                    "name": "renamed",
                    "if_unmodified_since": (fixed_time() + chrono::Duration::seconds(1))
                        .to_rfc3339_opts(SecondsFormat::Micros, true)
                }),
            ))
            .await
            .expect("response");
        assert_eq!(stale_response.status(), StatusCode::CONFLICT);
        assert_eq!(
            json_body(stale_response).await["error"]["code"],
            "stale_write"
        );

        let name_taken = crate::handler::build_router(app_state_with_mock_store_db_and_clients(
            tenant_store,
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![live_workspace]])
                .append_query_results([vec![count_row(1)]]),
            ControlPlaneClient::disabled(),
            SecretStoreClient::disabled(),
            SessionBrokerClient::disabled(),
        ))
        .oneshot(tenant_request(
            "PUT",
            &format!("/api/tenant/phlax/workspaces/{workspace_id}"),
            "phlax",
            serde_json::json!({
                "name": "renamed",
                "if_unmodified_since": fixed_time().to_rfc3339_opts(SecondsFormat::Micros, true)
            }),
        ))
        .await
        .expect("response");
        assert_eq!(name_taken.status(), StatusCode::CONFLICT);
        assert_eq!(
            json_body(name_taken).await["error"]["code"],
            "already_exists"
        );
    }

    #[tokio::test]
    async fn delete_workspace_covers_success_disabled_not_found_and_mismatch() {
        let tenant_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let plugin_id = Uuid::new_v4();
        let store = MockApiStore::new().with_tenant(tenant_row(tenant_id, "phlax", fixed_time()));
        let control_plane = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sessions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sessions": []
            })))
            .mount(&control_plane)
            .await;

        let delete_ok = crate::handler::build_router(app_state_with_mock_store_db_and_clients(
            store.clone(),
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![workspace_row(workspace_id, tenant_id, "mcp")]])
                .append_query_results([vec![workspace_plugin_row(workspace_id, plugin_id, None)]])
                .append_query_results([vec![tenant_row(tenant_id, "phlax", fixed_time())]])
                .append_query_results([vec![plugin_row(plugin_id, "mcp-fetch")]])
                .append_exec_results([MockExecResult {
                    last_insert_id: 0,
                    rows_affected: 1,
                }]),
            ControlPlaneClient::with_endpoint(control_plane.uri()),
            SecretStoreClient::disabled(),
            SessionBrokerClient::disabled(),
        ))
        .oneshot(tenant_delete_request(
            &format!("/api/tenant/phlax/workspaces/{workspace_id}"),
            "phlax",
        ))
        .await
        .expect("response");
        assert_eq!(delete_ok.status(), StatusCode::NO_CONTENT);

        let delete_disabled =
            crate::handler::build_router(app_state_with_mock_store_db_and_clients(
                store.clone(),
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([vec![workspace_row(workspace_id, tenant_id, "mcp")]])
                    .append_query_results([vec![workspace_plugin_row(
                        workspace_id,
                        plugin_id,
                        None,
                    )]])
                    .append_query_results([vec![tenant_row(tenant_id, "phlax", fixed_time())]])
                    .append_query_results([vec![plugin_row(plugin_id, "mcp-fetch")]])
                    .append_exec_results([MockExecResult {
                        last_insert_id: 0,
                        rows_affected: 1,
                    }]),
                ControlPlaneClient::disabled(),
                SecretStoreClient::disabled(),
                SessionBrokerClient::disabled(),
            ))
            .oneshot(tenant_delete_request(
                &format!("/api/tenant/phlax/workspaces/{workspace_id}"),
                "phlax",
            ))
            .await
            .expect("response");
        assert_eq!(delete_disabled.status(), StatusCode::NO_CONTENT);

        let delete_missing =
            crate::handler::build_router(app_state_with_mock_store_db_and_clients(
                store.clone(),
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([Vec::<workspace::Model>::new()]),
                ControlPlaneClient::disabled(),
                SecretStoreClient::disabled(),
                SessionBrokerClient::disabled(),
            ))
            .oneshot(tenant_delete_request(
                &format!("/api/tenant/phlax/workspaces/{workspace_id}"),
                "phlax",
            ))
            .await
            .expect("response");
        assert_eq!(delete_missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json_body(delete_missing).await["error"]["code"],
            "not_found"
        );

        let delete_mismatch =
            crate::handler::build_router(app_state_with_mock_store_db_and_clients(
                store,
                MockDatabase::new(DatabaseBackend::Postgres).append_query_results([vec![
                    workspace_row(workspace_id, Uuid::new_v4(), "mcp"),
                ]]),
                ControlPlaneClient::disabled(),
                SecretStoreClient::disabled(),
                SessionBrokerClient::disabled(),
            ))
            .oneshot(tenant_delete_request(
                &format!("/api/tenant/phlax/workspaces/{workspace_id}"),
                "phlax",
            ))
            .await
            .expect("response");
        assert_eq!(delete_mismatch.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json_body(delete_mismatch).await["error"]["code"],
            "not_found"
        );
    }

    #[tokio::test]
    async fn plugin_handlers_cover_create_update_and_delete_branches() {
        let plugin_id = Uuid::new_v4();

        let create_ok = crate::handler::build_router(crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![count_row(0)]])
                .append_query_results([vec![plugin_row(plugin_id, "mcp-fetch")]]),
        ))
        .oneshot(admin_request(
            "POST",
            "/api/plugins",
            serde_json::json!({
                "name": "mcp-fetch",
                "image": "ghcr.io/example/mcp-fetch:1.0",
                "egress": "none"
            }),
        ))
        .await
        .expect("response");
        assert_eq!(create_ok.status(), StatusCode::CREATED);
        let create_ok_location = create_ok
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        assert!(create_ok_location
            .as_deref()
            .expect("location")
            .starts_with("/api/plugins/"));
        let _ = json_body(create_ok).await;

        let create_taken =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([vec![count_row(1)]]),
            ))
            .oneshot(admin_request(
                "POST",
                "/api/plugins",
                serde_json::json!({
                    "name": "mcp-fetch",
                    "image": "ghcr.io/example/mcp-fetch:1.0",
                    "egress": "none"
                }),
            ))
            .await
            .expect("response");
        assert_eq!(create_taken.status(), StatusCode::CONFLICT);
        assert_eq!(
            json_body(create_taken).await["error"]["code"],
            "already_exists"
        );

        let live_plugin = plugin_row(plugin_id, "mcp-fetch");
        let updated_plugin = plugin_row(plugin_id, "renamed");
        let update_ok = crate::handler::build_router(crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![live_plugin.clone()]])
                .append_query_results([vec![count_row(0)]])
                .append_query_results([vec![updated_plugin]]),
        ))
        .oneshot(admin_request(
            "PUT",
            &format!("/api/plugins/{plugin_id}"),
            serde_json::json!({
                "name": "renamed",
                "image": "ghcr.io/example/mcp-fetch:1.0",
                "egress": "none",
                "if_unmodified_since": fixed_time().to_rfc3339_opts(SecondsFormat::Micros, true)
            }),
        ))
        .await
        .expect("response");
        assert_eq!(update_ok.status(), StatusCode::OK);
        assert_eq!(json_body(update_ok).await["name"], "renamed");

        let update_missing =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([Vec::<plugin::Model>::new()]),
            ))
            .oneshot(admin_request(
                "PUT",
                &format!("/api/plugins/{plugin_id}"),
                serde_json::json!({
                    "name": "renamed",
                    "image": "ghcr.io/example/mcp-fetch:1.0",
                    "egress": "none",
                    "if_unmodified_since": fixed_time().to_rfc3339_opts(SecondsFormat::Micros, true)
                }),
            ))
            .await
            .expect("response");
        assert_eq!(update_missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json_body(update_missing).await["error"]["code"],
            "not_found"
        );

        let update_stale =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([vec![live_plugin.clone()]]),
            ))
            .oneshot(admin_request(
                "PUT",
                &format!("/api/plugins/{plugin_id}"),
                serde_json::json!({
                    "name": "renamed",
                    "image": "ghcr.io/example/mcp-fetch:1.0",
                    "egress": "none",
                    "if_unmodified_since": (fixed_time() + chrono::Duration::seconds(1))
                        .to_rfc3339_opts(SecondsFormat::Micros, true)
                }),
            ))
            .await
            .expect("response");
        assert_eq!(update_stale.status(), StatusCode::CONFLICT);
        assert_eq!(
            json_body(update_stale).await["error"]["code"],
            "stale_write"
        );

        let update_taken =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([vec![live_plugin.clone()]])
                    .append_query_results([vec![count_row(1)]]),
            ))
            .oneshot(admin_request(
                "PUT",
                &format!("/api/plugins/{plugin_id}"),
                serde_json::json!({
                    "name": "renamed",
                    "image": "ghcr.io/example/mcp-fetch:1.0",
                    "egress": "none",
                    "if_unmodified_since": fixed_time().to_rfc3339_opts(SecondsFormat::Micros, true)
                }),
            ))
            .await
            .expect("response");
        assert_eq!(update_taken.status(), StatusCode::CONFLICT);
        assert_eq!(
            json_body(update_taken).await["error"]["code"],
            "already_exists"
        );

        let delete_ok = crate::handler::build_router(crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![live_plugin.clone()]])
                .append_query_results([
                    Vec::<(workspace_plugin::Model, Option<workspace::Model>)>::new(),
                ])
                .append_exec_results([MockExecResult {
                    last_insert_id: 0,
                    rows_affected: 1,
                }]),
        ))
        .oneshot(delete_request(&format!("/api/plugins/{plugin_id}")))
        .await
        .expect("response");
        assert_eq!(delete_ok.status(), StatusCode::NO_CONTENT);

        let delete_dependents =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([vec![live_plugin.clone()]])
                    .append_query_results([vec![(
                        workspace_plugin_row(Uuid::new_v4(), plugin_id, None),
                        Some(workspace_row(Uuid::new_v4(), Uuid::new_v4(), "mcp")),
                    )]])
                    .append_query_results([vec![tenant_row(
                        Uuid::new_v4(),
                        "phlax",
                        fixed_time(),
                    )]]),
            ))
            .oneshot(delete_request(&format!("/api/plugins/{plugin_id}")))
            .await
            .expect("response");
        assert_eq!(delete_dependents.status(), StatusCode::CONFLICT);
        let delete_dependents_body = json_body(delete_dependents).await;
        assert_eq!(delete_dependents_body["error"]["code"], "has_dependents");
        assert!(delete_dependents_body["error"]["message"]
            .as_str()
            .expect("message")
            .contains("phlax/mcp"));

        let delete_missing =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([Vec::<plugin::Model>::new()]),
            ))
            .oneshot(delete_request(&format!("/api/plugins/{plugin_id}")))
            .await
            .expect("response");
        assert_eq!(delete_missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json_body(delete_missing).await["error"]["code"],
            "not_found"
        );
    }

    #[tokio::test]
    async fn workspace_plugin_handlers_cover_create_update_delete_and_validation() {
        let workspace_id = Uuid::new_v4();
        let plugin_id = Uuid::new_v4();

        let create_ok = crate::handler::build_router(crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![triple_row("phlax", "mcp", "mcp-fetch")]])
                .append_query_results([Vec::<workspace_plugin::Model>::new()])
                .append_query_results([vec![workspace_plugin_row(
                    workspace_id,
                    plugin_id,
                    Some(serde_json::json!({ "k": "v" })),
                )]]),
        ))
        .oneshot(tenant_request(
            "POST",
            "/api/tenant/phlax/workspace_plugins",
            "phlax",
            serde_json::json!({
                "workspace_id": workspace_id,
                "plugin_id": plugin_id,
                "config": { "k": "v" }
            }),
        ))
        .await
        .expect("response");
        assert_eq!(create_ok.status(), StatusCode::CREATED);
        assert_eq!(
            create_ok
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some(
                format!("/api/tenant/phlax/workspace_plugins/{workspace_id}/{plugin_id}").as_str()
            )
        );

        let create_existing =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([vec![triple_row("phlax", "mcp", "mcp-fetch")]])
                    .append_query_results([vec![workspace_plugin_row(
                        workspace_id,
                        plugin_id,
                        None,
                    )]]),
            ))
            .oneshot(tenant_request(
                "POST",
                "/api/tenant/phlax/workspace_plugins",
                "phlax",
                serde_json::json!({
                    "workspace_id": workspace_id,
                    "plugin_id": plugin_id
                }),
            ))
            .await
            .expect("response");
        assert_eq!(create_existing.status(), StatusCode::CONFLICT);
        assert_eq!(
            json_body(create_existing).await["error"]["code"],
            "already_exists"
        );

        let create_missing =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([Vec::<BTreeMap<String, sea_orm::Value>>::new()]),
            ))
            .oneshot(tenant_request(
                "POST",
                "/api/tenant/phlax/workspace_plugins",
                "phlax",
                serde_json::json!({
                    "workspace_id": workspace_id,
                    "plugin_id": plugin_id
                }),
            ))
            .await
            .expect("response");
        assert_eq!(create_missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json_body(create_missing).await["error"]["code"],
            "not_found"
        );

        let create_bad_config =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([vec![triple_row("phlax", "mcp", "mcp-fetch")]])
                    .append_query_results([Vec::<workspace_plugin::Model>::new()]),
            ))
            .oneshot(tenant_request(
                "POST",
                "/api/tenant/phlax/workspace_plugins",
                "phlax",
                serde_json::json!({
                    "workspace_id": workspace_id,
                    "plugin_id": plugin_id,
                    "config": 7
                }),
            ))
            .await
            .expect("response");
        assert_eq!(create_bad_config.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            json_body(create_bad_config).await["error"]["code"],
            "validation_failed"
        );

        let create_null_config =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([vec![triple_row("phlax", "mcp", "mcp-fetch")]])
                    .append_query_results([Vec::<workspace_plugin::Model>::new()])
                    .append_query_results([vec![workspace_plugin_row(
                        workspace_id,
                        plugin_id,
                        None,
                    )]]),
            ))
            .oneshot(tenant_request(
                "POST",
                "/api/tenant/phlax/workspace_plugins",
                "phlax",
                serde_json::json!({
                    "workspace_id": workspace_id,
                    "plugin_id": plugin_id,
                    "config": null
                }),
            ))
            .await
            .expect("response");
        assert_eq!(create_null_config.status(), StatusCode::CREATED);
        assert!(json_body(create_null_config).await["config"].is_null());

        let create_empty_config =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([vec![triple_row("phlax", "mcp", "mcp-fetch")]])
                    .append_query_results([Vec::<workspace_plugin::Model>::new()])
                    .append_query_results([vec![workspace_plugin_row(
                        workspace_id,
                        plugin_id,
                        None,
                    )]]),
            ))
            .oneshot(tenant_request(
                "POST",
                "/api/tenant/phlax/workspace_plugins",
                "phlax",
                serde_json::json!({
                    "workspace_id": workspace_id,
                    "plugin_id": plugin_id,
                    "config": {}
                }),
            ))
            .await
            .expect("response");
        assert_eq!(create_empty_config.status(), StatusCode::CREATED);
        assert!(json_body(create_empty_config).await["config"].is_null());

        let live_binding = workspace_plugin_row(
            workspace_id,
            plugin_id,
            Some(serde_json::json!({ "k": "v" })),
        );
        let update_ok = crate::handler::build_router(crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![live_binding.clone()]])
                .append_query_results([vec![triple_row("phlax", "mcp", "mcp-fetch")]])
                .append_query_results([vec![workspace_plugin_row(
                    workspace_id,
                    plugin_id,
                    Some(serde_json::json!({ "k": "next" })),
                )]]),
        ))
        .oneshot(tenant_request(
            "PUT",
            &format!("/api/tenant/phlax/workspace_plugins/{workspace_id}/{plugin_id}"),
            "phlax",
            serde_json::json!({
                "config": { "k": "next" },
                "if_unmodified_since": fixed_time().to_rfc3339_opts(SecondsFormat::Micros, true)
            }),
        ))
        .await
        .expect("response");
        assert_eq!(update_ok.status(), StatusCode::OK);
        assert_eq!(json_body(update_ok).await["config"]["k"], "next");

        let update_missing =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([Vec::<workspace_plugin::Model>::new()]),
            ))
            .oneshot(tenant_request(
                "PUT",
                &format!("/api/tenant/phlax/workspace_plugins/{workspace_id}/{plugin_id}"),
                "phlax",
                serde_json::json!({
                    "config": { "k": "next" },
                    "if_unmodified_since": fixed_time().to_rfc3339_opts(SecondsFormat::Micros, true)
                }),
            ))
            .await
            .expect("response");
        assert_eq!(update_missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json_body(update_missing).await["error"]["code"],
            "not_found"
        );

        let update_stale =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([vec![live_binding.clone()]]),
            ))
            .oneshot(tenant_request(
                "PUT",
                &format!("/api/tenant/phlax/workspace_plugins/{workspace_id}/{plugin_id}"),
                "phlax",
                serde_json::json!({
                    "config": { "k": "next" },
                    "if_unmodified_since": (fixed_time() + chrono::Duration::seconds(1))
                        .to_rfc3339_opts(SecondsFormat::Micros, true)
                }),
            ))
            .await
            .expect("response");
        assert_eq!(update_stale.status(), StatusCode::CONFLICT);
        assert_eq!(
            json_body(update_stale).await["error"]["code"],
            "stale_write"
        );

        let update_clear =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([vec![live_binding.clone()]])
                    .append_query_results([vec![triple_row("phlax", "mcp", "mcp-fetch")]])
                    .append_query_results([vec![workspace_plugin_row(
                        workspace_id,
                        plugin_id,
                        None,
                    )]]),
            ))
            .oneshot(tenant_request(
                "PUT",
                &format!("/api/tenant/phlax/workspace_plugins/{workspace_id}/{plugin_id}"),
                "phlax",
                serde_json::json!({
                    "config": null,
                    "if_unmodified_since": fixed_time().to_rfc3339_opts(SecondsFormat::Micros, true)
                }),
            ))
            .await
            .expect("response");
        assert_eq!(update_clear.status(), StatusCode::OK);
        assert!(json_body(update_clear).await["config"].is_null());

        let update_unavailable = crate::handler::build_router(app_state_with_mock_db_and_clients(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![live_binding.clone()]])
                .append_query_results([vec![triple_row("phlax", "mcp", "mcp-fetch")]]),
            ControlPlaneClient::with_endpoint("http://127.0.0.1:1"),
            SecretStoreClient::disabled(),
            SessionBrokerClient::disabled(),
        ))
        .oneshot(tenant_request(
            "PUT",
            &format!("/api/tenant/phlax/workspace_plugins/{workspace_id}/{plugin_id}"),
            "phlax",
            serde_json::json!({
                "config": { "k": "next" },
                "if_unmodified_since": fixed_time().to_rfc3339_opts(SecondsFormat::Micros, true)
            }),
        ))
        .await
        .expect("response");
        assert_eq!(update_unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            json_body(update_unavailable).await["error"]["code"],
            "unavailable"
        );

        let delete_ok = crate::handler::build_router(crate::test_support::app_state_with_mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![live_binding.clone()]])
                .append_query_results([vec![triple_row("phlax", "mcp", "mcp-fetch")]])
                .append_exec_results([MockExecResult {
                    last_insert_id: 0,
                    rows_affected: 1,
                }]),
        ))
        .oneshot(tenant_delete_request(
            &format!(
                "/api/tenant/phlax/workspace_plugins/{workspace_id}/{plugin_id}?if_unmodified_since={}",
                fixed_time().to_rfc3339_opts(SecondsFormat::Micros, true)
            ),
            "phlax",
        ))
        .await
        .expect("response");
        assert_eq!(delete_ok.status(), StatusCode::NO_CONTENT);

        let delete_missing =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([Vec::<workspace_plugin::Model>::new()]),
            ))
            .oneshot(tenant_delete_request(
                &format!("/api/tenant/phlax/workspace_plugins/{workspace_id}/{plugin_id}"),
                "phlax",
            ))
            .await
            .expect("response");
        assert_eq!(delete_missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json_body(delete_missing).await["error"]["code"],
            "not_found"
        );

        let delete_unavailable = crate::handler::build_router(app_state_with_mock_db_and_clients(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![live_binding]])
                .append_query_results([vec![triple_row("phlax", "mcp", "mcp-fetch")]]),
            ControlPlaneClient::with_endpoint("http://127.0.0.1:1"),
            SecretStoreClient::disabled(),
            SessionBrokerClient::disabled(),
        ))
        .oneshot(tenant_delete_request(
            &format!("/api/tenant/phlax/workspace_plugins/{workspace_id}/{plugin_id}"),
            "phlax",
        ))
        .await
        .expect("response");
        assert_eq!(delete_unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            json_body(delete_unavailable).await["error"]["code"],
            "unavailable"
        );
    }

    #[tokio::test]
    async fn db_tenant_functions_cover_mock_database_branches() {
        let tenant_id = Uuid::new_v4();
        let lock = fixed_time();
        let live = tenant_row(tenant_id, "phlax", lock);

        let create_taken = db_create_tenant(
            &MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![count_row(1)]])
                .into_connection(),
            "phlax".to_string(),
        )
        .await
        .expect_err("taken")
        .into_response();
        assert_eq!(create_taken.status(), StatusCode::CONFLICT);

        let create_ok = db_create_tenant(
            &MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![count_row(0)]])
                .append_query_results([vec![tenant_row(tenant_id, "phlax", lock)]])
                .into_connection(),
            "phlax".to_string(),
        )
        .await
        .expect("create");
        assert_eq!(create_ok.name, "phlax");

        let update_missing = db_update_tenant(
            &MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([Vec::<tenant::Model>::new()])
                .into_connection(),
            tenant_id,
            "renamed".to_string(),
            lock,
        )
        .await
        .expect_err("missing")
        .into_response();
        assert_eq!(update_missing.status(), StatusCode::NOT_FOUND);

        let update_stale = db_update_tenant(
            &MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![live.clone()]])
                .into_connection(),
            tenant_id,
            "renamed".to_string(),
            lock + chrono::Duration::seconds(1),
        )
        .await
        .expect_err("stale")
        .into_response();
        assert_eq!(update_stale.status(), StatusCode::CONFLICT);

        let update_taken = db_update_tenant(
            &MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![live.clone()]])
                .append_query_results([vec![count_row(1)]])
                .into_connection(),
            tenant_id,
            "renamed".to_string(),
            lock,
        )
        .await
        .expect_err("taken")
        .into_response();
        assert_eq!(update_taken.status(), StatusCode::CONFLICT);

        let update_ok = db_update_tenant(
            &MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![live.clone()]])
                .append_query_results([vec![count_row(0)]])
                .append_query_results([vec![tenant_row(tenant_id, "renamed", lock)]])
                .into_connection(),
            tenant_id,
            "renamed".to_string(),
            lock,
        )
        .await
        .expect("update");
        assert_eq!(update_ok.name, "renamed");

        let delete_missing = db_delete_tenant(
            &MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([Vec::<tenant::Model>::new()])
                .into_connection(),
            tenant_id,
            Some(lock),
        )
        .await
        .expect_err("missing")
        .into_response();
        assert_eq!(delete_missing.status(), StatusCode::NOT_FOUND);

        let delete_stale = db_delete_tenant(
            &MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![live.clone()]])
                .into_connection(),
            tenant_id,
            Some(lock + chrono::Duration::seconds(1)),
        )
        .await
        .expect_err("stale")
        .into_response();
        assert_eq!(delete_stale.status(), StatusCode::CONFLICT);

        let delete_dependents = db_delete_tenant(
            &MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![live.clone()]])
                .append_query_results([vec![workspace_row(Uuid::new_v4(), tenant_id, "mcp")]])
                .into_connection(),
            tenant_id,
            Some(lock),
        )
        .await
        .expect_err("dependents")
        .into_response();
        assert_eq!(delete_dependents.status(), StatusCode::CONFLICT);

        let delete_ok = db_delete_tenant(
            &MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![live]])
                .append_query_results([Vec::<workspace::Model>::new()])
                .append_exec_results([MockExecResult {
                    last_insert_id: 0,
                    rows_affected: 1,
                }])
                .into_connection(),
            tenant_id,
            Some(lock),
        )
        .await
        .expect("delete");
        assert_eq!(delete_ok.id, tenant_id);
    }

    #[tokio::test]
    async fn secret_handlers_cover_reachable_client_variants_and_success_paths() {
        let secret_store = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/secrets"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "stored": "github/pat",
                "created": true
            })))
            .mount(&secret_store)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/secrets/github/pat"))
            .and(query_param("tenant", "phlax"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&secret_store)
            .await;

        let session_broker = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/evict-tenant/phlax"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&session_broker)
            .await;

        let create_ok = crate::handler::build_router(app_state_with_mock_db_and_clients(
            MockDatabase::new(DatabaseBackend::Postgres),
            ControlPlaneClient::disabled(),
            SecretStoreClient::with_endpoint(secret_store.uri()),
            SessionBrokerClient::with_endpoint(session_broker.uri()),
        ))
        .oneshot(tenant_request(
            "POST",
            "/api/tenant/phlax/secrets",
            "phlax",
            serde_json::json!({
                "service": "github",
                "name": "pat",
                "kind": "opaque",
                "value_b64": "dGVzdA=="
            }),
        ))
        .await
        .expect("response");
        assert_eq!(create_ok.status(), StatusCode::CREATED);
        assert_eq!(
            create_ok
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("/api/tenant/phlax/secrets/github/pat")
        );

        let delete_ok = crate::handler::build_router(app_state_with_mock_db_and_clients(
            MockDatabase::new(DatabaseBackend::Postgres),
            ControlPlaneClient::disabled(),
            SecretStoreClient::with_endpoint(secret_store.uri()),
            SessionBrokerClient::with_endpoint(session_broker.uri()),
        ))
        .oneshot(tenant_delete_request(
            "/api/tenant/phlax/secrets/github/pat",
            "phlax",
        ))
        .await
        .expect("response");
        assert_eq!(delete_ok.status(), StatusCode::NO_CONTENT);

        let create_disabled = crate::handler::build_router(app_state_with_mock_db_and_clients(
            MockDatabase::new(DatabaseBackend::Postgres),
            ControlPlaneClient::disabled(),
            SecretStoreClient::disabled(),
            SessionBrokerClient::disabled(),
        ))
        .oneshot(tenant_request(
            "POST",
            "/api/tenant/phlax/secrets",
            "phlax",
            serde_json::json!({
                "service": "github",
                "name": "pat",
                "kind": "opaque",
                "value_b64": "dGVzdA=="
            }),
        ))
        .await
        .expect("response");
        assert_eq!(create_disabled.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            json_body(create_disabled).await["error"]["code"],
            "unavailable"
        );

        let create_conflict_store = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/secrets"))
            .respond_with(ResponseTemplate::new(409).set_body_string("exists"))
            .mount(&create_conflict_store)
            .await;
        let create_conflict = crate::handler::build_router(app_state_with_mock_db_and_clients(
            MockDatabase::new(DatabaseBackend::Postgres),
            ControlPlaneClient::disabled(),
            SecretStoreClient::with_endpoint(create_conflict_store.uri()),
            SessionBrokerClient::disabled(),
        ))
        .oneshot(tenant_request(
            "POST",
            "/api/tenant/phlax/secrets",
            "phlax",
            serde_json::json!({
                "service": "github",
                "name": "pat",
                "kind": "opaque",
                "value_b64": "dGVzdA=="
            }),
        ))
        .await
        .expect("response");
        assert_eq!(create_conflict.status(), StatusCode::CONFLICT);
        assert_eq!(
            json_body(create_conflict).await["error"]["code"],
            "already_exists"
        );

        let create_bad_request_store = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/secrets"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad"))
            .mount(&create_bad_request_store)
            .await;
        let create_bad_request = crate::handler::build_router(app_state_with_mock_db_and_clients(
            MockDatabase::new(DatabaseBackend::Postgres),
            ControlPlaneClient::disabled(),
            SecretStoreClient::with_endpoint(create_bad_request_store.uri()),
            SessionBrokerClient::disabled(),
        ))
        .oneshot(tenant_request(
            "POST",
            "/api/tenant/phlax/secrets",
            "phlax",
            serde_json::json!({
                "service": "github",
                "name": "pat",
                "kind": "opaque",
                "value_b64": "dGVzdA=="
            }),
        ))
        .await
        .expect("response");
        assert_eq!(create_bad_request.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(create_bad_request).await["error"]["code"],
            "bad_request"
        );

        let create_unavailable = crate::handler::build_router(app_state_with_mock_db_and_clients(
            MockDatabase::new(DatabaseBackend::Postgres),
            ControlPlaneClient::disabled(),
            SecretStoreClient::with_endpoint("http://127.0.0.1:1"),
            SessionBrokerClient::disabled(),
        ))
        .oneshot(tenant_request(
            "POST",
            "/api/tenant/phlax/secrets",
            "phlax",
            serde_json::json!({
                "service": "github",
                "name": "pat",
                "kind": "opaque",
                "value_b64": "dGVzdA=="
            }),
        ))
        .await
        .expect("response");
        assert_eq!(create_unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            json_body(create_unavailable).await["error"]["code"],
            "unavailable"
        );

        let delete_disabled = crate::handler::build_router(app_state_with_mock_db_and_clients(
            MockDatabase::new(DatabaseBackend::Postgres),
            ControlPlaneClient::disabled(),
            SecretStoreClient::disabled(),
            SessionBrokerClient::disabled(),
        ))
        .oneshot(tenant_delete_request(
            "/api/tenant/phlax/secrets/github/pat",
            "phlax",
        ))
        .await
        .expect("response");
        assert_eq!(delete_disabled.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            json_body(delete_disabled).await["error"]["code"],
            "unavailable"
        );

        let delete_missing_store = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/secrets/github/pat"))
            .and(query_param("tenant", "phlax"))
            .respond_with(ResponseTemplate::new(404).set_body_string("missing"))
            .mount(&delete_missing_store)
            .await;
        let delete_missing = crate::handler::build_router(app_state_with_mock_db_and_clients(
            MockDatabase::new(DatabaseBackend::Postgres),
            ControlPlaneClient::disabled(),
            SecretStoreClient::with_endpoint(delete_missing_store.uri()),
            SessionBrokerClient::disabled(),
        ))
        .oneshot(tenant_delete_request(
            "/api/tenant/phlax/secrets/github/pat",
            "phlax",
        ))
        .await
        .expect("response");
        assert_eq!(delete_missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json_body(delete_missing).await["error"]["code"],
            "not_found"
        );

        let delete_bad_request_store = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/secrets/github/pat"))
            .and(query_param("tenant", "phlax"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad"))
            .mount(&delete_bad_request_store)
            .await;
        let delete_bad_request = crate::handler::build_router(app_state_with_mock_db_and_clients(
            MockDatabase::new(DatabaseBackend::Postgres),
            ControlPlaneClient::disabled(),
            SecretStoreClient::with_endpoint(delete_bad_request_store.uri()),
            SessionBrokerClient::disabled(),
        ))
        .oneshot(tenant_delete_request(
            "/api/tenant/phlax/secrets/github/pat",
            "phlax",
        ))
        .await
        .expect("response");
        assert_eq!(delete_bad_request.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(delete_bad_request).await["error"]["code"],
            "bad_request"
        );

        let delete_unavailable = crate::handler::build_router(app_state_with_mock_db_and_clients(
            MockDatabase::new(DatabaseBackend::Postgres),
            ControlPlaneClient::disabled(),
            SecretStoreClient::with_endpoint("http://127.0.0.1:1"),
            SessionBrokerClient::disabled(),
        ))
        .oneshot(tenant_delete_request(
            "/api/tenant/phlax/secrets/github/pat",
            "phlax",
        ))
        .await
        .expect("response");
        assert_eq!(delete_unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            json_body(delete_unavailable).await["error"]["code"],
            "unavailable"
        );
    }

    #[tokio::test]
    async fn delete_workspace_returns_503_when_control_plane_is_unavailable() {
        let path_tenant = "phlax";
        let tenant_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let plugin_id = Uuid::new_v4();

        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![tenant_row(tenant_id, path_tenant, fixed_time())]])
            .append_query_results([vec![workspace_row(workspace_id, tenant_id, "mcp")]])
            .append_query_results([vec![workspace_plugin_row(workspace_id, plugin_id, None)]])
            .append_query_results([vec![tenant_row(tenant_id, path_tenant, fixed_time())]])
            .append_query_results([vec![plugin_row(plugin_id, "mcp-fetch")]]);
        let db = Arc::new(db.into_connection());
        let state = AppState {
            store: Arc::new(SeaOrmApiStore::new_shared(db.clone())),
            db,
            control_plane: ControlPlaneClient::with_endpoint("http://127.0.0.1:1"),
            secret_store: SecretStoreClient::disabled(),
            session_broker: SessionBrokerClient::disabled(),
        };
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(tenant_delete_request(
                &format!("/api/tenant/{path_tenant}/workspaces/{workspace_id}"),
                path_tenant,
            ))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json_body(response).await["error"]["code"], "unavailable");
    }

    // ── Tier 1.5: json_type Bool, String, Array arms ───────────────

    #[tokio::test]
    async fn create_workspace_plugin_rejects_bool_string_and_array_configs() {
        let workspace_id = Uuid::new_v4();
        let plugin_id = Uuid::new_v4();

        // Bool arm
        let bool_config_response =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([vec![triple_row("phlax", "mcp", "mcp-fetch")]])
                    .append_query_results([Vec::<workspace_plugin::Model>::new()]),
            ))
            .oneshot(tenant_request(
                "POST",
                "/api/tenant/phlax/workspace_plugins",
                "phlax",
                serde_json::json!({
                    "workspace_id": workspace_id,
                    "plugin_id": plugin_id,
                    "config": true
                }),
            ))
            .await
            .expect("response");
        assert_eq!(
            bool_config_response.status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let json = json_body(bool_config_response).await;
        assert_eq!(json["error"]["code"], "validation_failed");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("bool"),
            "expected 'bool' in message: {json}"
        );

        // String arm
        let string_config_response =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([vec![triple_row("phlax", "mcp", "mcp-fetch")]])
                    .append_query_results([Vec::<workspace_plugin::Model>::new()]),
            ))
            .oneshot(tenant_request(
                "POST",
                "/api/tenant/phlax/workspace_plugins",
                "phlax",
                serde_json::json!({
                    "workspace_id": workspace_id,
                    "plugin_id": plugin_id,
                    "config": "invalid"
                }),
            ))
            .await
            .expect("response");
        assert_eq!(
            string_config_response.status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let json = json_body(string_config_response).await;
        assert_eq!(json["error"]["code"], "validation_failed");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("string"),
            "expected 'string' in message: {json}"
        );

        // Array arm
        let array_config_response =
            crate::handler::build_router(crate::test_support::app_state_with_mock_db(
                MockDatabase::new(DatabaseBackend::Postgres)
                    .append_query_results([vec![triple_row("phlax", "mcp", "mcp-fetch")]])
                    .append_query_results([Vec::<workspace_plugin::Model>::new()]),
            ))
            .oneshot(tenant_request(
                "POST",
                "/api/tenant/phlax/workspace_plugins",
                "phlax",
                serde_json::json!({
                    "workspace_id": workspace_id,
                    "plugin_id": plugin_id,
                    "config": [1, 2, 3]
                }),
            ))
            .await
            .expect("response");
        assert_eq!(
            array_config_response.status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let json = json_body(array_config_response).await;
        assert_eq!(json["error"]["code"], "validation_failed");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("array"),
            "expected 'array' in message: {json}"
        );
    }

    #[tokio::test]
    async fn require_secret_component_rejects_blank_value() {
        // Empty and whitespace-only values should yield validation_failed.
        let state = crate::test_support::app_state_with_mock_store(MockApiStore::new());
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(tenant_request(
                "POST",
                "/api/tenant/phlax/secrets",
                "phlax",
                serde_json::json!({
                    "service": "   ",   // whitespace-only → trimmed to ""
                    "name": "pat",
                    "kind": "opaque",
                    "value_b64": "dGVzdA=="
                }),
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            json_body(response).await["error"]["code"],
            "validation_failed"
        );
    }

    #[tokio::test]
    async fn require_secret_component_rejects_too_long_component() {
        // A component longer than 128 chars hits the `len() > 128` branch
        // (line 214) — the only condition not covered by the existing
        // test that uses dot-prefix names.
        let long_name = "a".repeat(129); // 129 chars, no forbidden chars, not empty
        let state = crate::test_support::app_state_with_mock_store(MockApiStore::new());
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(tenant_request(
                "POST",
                "/api/tenant/phlax/secrets",
                "phlax",
                serde_json::json!({
                    "service": long_name,
                    "name": "pat",
                    "kind": "opaque",
                    "value_b64": "dGVzdA=="
                }),
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            json_body(response).await["error"]["code"],
            "validation_failed"
        );
    }

    #[tokio::test]
    async fn delete_secret_rejects_too_long_component() {
        // Same branch via the DELETE handler path.
        let long_name = "a".repeat(129);
        let state = crate::test_support::app_state_with_mock_store(MockApiStore::new());
        let app = crate::handler::build_router(state);

        let response = app
            .oneshot(tenant_delete_request(
                &format!("/api/tenant/phlax/secrets/github/{long_name}"),
                "phlax",
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            json_body(response).await["error"]["code"],
            "validation_failed"
        );
    }
}
