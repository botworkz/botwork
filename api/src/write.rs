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
use botwork_api_core::names::{normalise_name, validate_plugin_name, validate_tenant_name, validate_workspace_name};
use botwork_api_core::plugin_spec::{validate_one, RawPluginEntry};
use botwork_entity::{plugin, tenant, workspace, workspace_plugin};
use chrono::{DateTime, Utc};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseConnection,
    EntityTrait, JoinType, QueryFilter, TransactionTrait,
};
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};
use tracing::info;
use uuid::Uuid;

use crate::control_plane::{outcome_summary, terminate_live_sessions, GateError};
use crate::handler::{
    bad_request, check_tenant_consistency, operator, parse_body, require_admin,
    resolve_tenant_id, ApiError, ApiErrorExt, AppState, PREFIX,
};
use crate::secret_store::{PutSecretRequest, SecretStoreError};

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
    struct Row { cnt: i64 }

    let backend = db.get_database_backend();
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
        .map_err(|err| ApiError::Internal { detail: format!("db: {err}") })?;
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
    struct Row { cnt: i64 }

    let backend = db.get_database_backend();
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
        .map_err(|err| ApiError::Internal { detail: format!("db: {err}") })?;
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
    struct Row { cnt: i64 }

    let backend = db.get_database_backend();
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
        .map_err(|err| ApiError::Internal { detail: format!("db: {err}") })?;
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
fn check_lock(
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
        .route("/api/tenants/:id", put(update_tenant))
        .route("/api/tenants/:id", delete(delete_tenant))
        // Admin-gated plugin CRUD (plugins are globally shared resources).
        .route("/api/plugins", post(create_plugin))
        .route("/api/plugins/:id", put(update_plugin))
        .route("/api/plugins/:id", delete(delete_plugin))
        // Tenant-scoped workspace CRUD.
        .route("/api/tenant/:tenant/workspaces", post(create_workspace))
        .route(
            "/api/tenant/:tenant/workspaces/:id",
            put(update_workspace),
        )
        .route(
            "/api/tenant/:tenant/workspaces/:id",
            delete(delete_workspace),
        )
        // Tenant-scoped binding CRUD.
        .route(
            "/api/tenant/:tenant/workspace_plugins",
            post(create_workspace_plugin),
        )
        .route(
            "/api/tenant/:tenant/workspace_plugins/:workspace_id/:plugin_id",
            put(update_workspace_plugin),
        )
        .route(
            "/api/tenant/:tenant/workspace_plugins/:workspace_id/:plugin_id",
            delete(delete_workspace_plugin),
        )
        // Tenant-scoped secrets.
        .route("/api/tenant/:tenant/secrets", post(create_secret))
        .route(
            "/api/tenant/:tenant/secrets/:service/:name",
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

    // Case-insensitive uniqueness: normalise_name lowercases so
    // "Phlax" blocks "phlax" / "PHLAX" from being created.
    let normalised = normalise_name(&name);
    if tenant_name_taken(state.db.as_ref(), &normalised, None).await? {
        return Err(ApiError::already_exists(format!(
            "tenant with name {name:?} already exists (case-insensitive)"
        )));
    }

    let id = Uuid::new_v4();
    let now = Utc::now();
    let row = tenant::ActiveModel {
        id: Set(id),
        name: Set(name.clone()),
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(state.db.as_ref())
    .await?;

    audit_event(&op, "create", "tenant", row.id, &format!("name={name:?}"));

    let mut response = (StatusCode::CREATED, Json(row)).into_response();
    response.headers_mut().insert(
        LOCATION,
        HeaderValue::from_str(&format!("/api/tenants/{id}")).expect("uuid is ascii"),
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

    let tx = state.db.begin().await?;
    let live = tenant::Entity::find_by_id(id)
        .one(&tx)
        .await?
        .or_not_found("tenant", format!("no tenant with id {id}"))?;
    check_lock(&body.if_unmodified_since, &live.updated_at, "tenant")?;

    // Case-insensitive uniqueness: check no OTHER tenant has the same
    // normalised name. The exclude_id skips the current tenant so a
    // rename to a different capitalisation of the same name is allowed.
    let normalised = normalise_name(&name);
    if tenant_name_taken(&tx, &normalised, Some(live.id)).await? {
        return Err(ApiError::already_exists(format!(
            "tenant with name {name:?} already exists (case-insensitive)"
        )));
    }

    let now = Utc::now();
    let mut active: tenant::ActiveModel = live.into();
    active.name = Set(name.clone());
    active.updated_at = Set(now);
    let row = active.update(&tx).await?;
    tx.commit().await?;

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

    let tx = state.db.begin().await?;
    let live = tenant::Entity::find_by_id(id)
        .one(&tx)
        .await?
        .or_not_found("tenant", format!("no tenant with id {id}"))?;
    if let Some(lock) = q.if_unmodified_since {
        check_lock(&lock, &live.updated_at, "tenant")?;
    }

    // Delete-guard: workspace.tenant_id is RESTRICT. Build the
    // dependent identity list so the UI can render "remove these
    // workspaces first".
    let dependents = workspace::Entity::find()
        .filter(workspace::Column::TenantId.eq(id))
        .all(&tx)
        .await?;
    if !dependents.is_empty() {
        let names: Vec<String> = dependents.iter().map(|w| w.name.clone()).collect();
        let payload = json!(dependents
            .iter()
            .map(|w| json!({ "kind": "workspace", "id": w.id, "name": w.name }))
            .collect::<Vec<_>>());
        return Err(ApiError::has_dependents(
            format!(
                "tenant '{}' still has {} workspace(s): {:?}",
                live.name,
                dependents.len(),
                names
            ),
            payload,
        ));
    }

    tenant::Entity::delete_by_id(id).exec(&tx).await?;
    tx.commit().await?;

    audit_event(
        &op,
        "delete",
        "tenant",
        id,
        &format!("name={:?}", live.name),
    );
    Ok(StatusCode::NO_CONTENT)
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
    let tenant_id = resolve_tenant_id(state.db.as_ref(), &tenant_name).await?;

    // Case-insensitive uniqueness within the tenant: (tenant_id, LOWER(name)).
    let normalised = normalise_name(&name);
    if workspace_name_taken(state.db.as_ref(), tenant_id, &normalised, None).await? {
        return Err(ApiError::already_exists(format!(
            "workspace with name {name:?} already exists under tenant {tenant_name:?} (case-insensitive)"
        )));
    }

    let id = Uuid::new_v4();
    let now = Utc::now();
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

    let tenant_id = resolve_tenant_id(state.db.as_ref(), &tenant_name).await?;
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
    active.name = Set(name.clone());
    active.updated_at = Set(now);
    let row = active.update(&tx).await?;
    tx.commit().await?;

    audit_event(&op, "update", "workspace", id, &format!("tenant={tenant_name:?} name={name:?}"));
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

    let tenant_id = resolve_tenant_id(state.db.as_ref(), &tenant_name).await?;
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
            ApiError::bad_request(format!("could not re-serialise nested field: {err}"))
        })?;
        serde_yaml::from_str(&s).map_err(|err| {
            ApiError::bad_request(format!("nested field not yaml-roundtrip-able: {err}"))
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
        let mut payload = Vec::with_capacity(bindings.len());
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
            payload.push(json!({
                "kind": "workspace_plugin",
                "tenant": tenant_row.name,
                "workspace": ws.name,
                "workspace_id": ws.id,
                "plugin_id": binding.plugin_id,
            }));
            summary.push(format!("{}/{}", tenant_row.name, ws.name));
        }
        return Err(ApiError::has_dependents(
            format!(
                "plugin '{}' still bound in {} workspace(s): {:?}",
                live.name,
                bindings.len(),
                summary
            ),
            json!(payload),
        ));
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
            SecretStoreError::NotFound(msg) => ApiError::Internal {
                detail: format!("unexpected NotFound from secret-store on POST: {msg}"),
            },
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
            SecretStoreError::AlreadyExists(msg) => ApiError::Internal {
                detail: format!("unexpected AlreadyExists from secret-store on DELETE: {msg}"),
            },
        })?;

    audit_event(
        &op,
        "delete",
        "secret",
        format!("{service}/{name}"),
        &format!("tenant={tenant:?}"),
    );

    Ok(StatusCode::NO_CONTENT)
}

// ── shared helpers ─────────────────────────────────────────────────

fn json_type(v: &JsonValue) -> &'static str {
    match v {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "bool",
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
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
