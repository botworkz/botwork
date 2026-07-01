//! HTTP handler primitives: app state, error envelope, JSON extractor, health.
//!
//! Entity-specific read handlers live in [`crate::read`]; write handlers
//! in [`crate::write`].
//!
//! # Error envelope
//!
//! Mirrors auth-broker:
//!
//! ```json
//! {
//!   "error": {
//!     "code": "<machine code>",
//!     "message": "<human detail>"
//!   }
//! }
//! ```
//!
//! [`ApiError`] maps each variant to a `(StatusCode, body)` pair via
//! [`ApiError::into_response`]; callers `?`-bubble through this so the
//! handler bodies stay branch-free on the error path.
//!
//! # JSON extractor
//!
//! Write handlers deserialise their bodies through [`AdminJson<T>`]
//! rather than axum's built-in `Json<T>`. The wrapping exists for one
//! reason: the default `JsonRejection` returns a 400 with a plaintext
//! body that doesn't follow the error envelope. `AdminJson` maps every
//! deserialisation failure into [`ApiError::BadRequest`] so the wire
//! response shape stays uniform.
//!
//! All write-side `Deserialize` bodies use
//! `#[serde(deny_unknown_fields)]` so a typo in a field name is a
//! 400 rather than a silently-dropped field.

use std::sync::Arc;

use axum::extract::State;
use axum::http::header::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use botwork_api_core::names::NameError;
use botwork_api_core::ValidationError;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, Statement};
use serde::Serialize;
use serde_json::Value as JsonValue;
use tracing::{error, warn};
use uuid::Uuid;

use crate::control_plane::ControlPlaneClient;
use crate::secret_store::SecretStoreClient;
use crate::{read, write};

pub(crate) const PREFIX: &str = "[api]";

/// Shared state injected into every handler.
///
/// * `db` — SeaORM connection. The crate clones the `Arc` per
///   request; the underlying pool is shared. Same posture as
///   config-broker.
/// * `control_plane` — HTTP client targeting control-plane on
///   `botwork-internal` for the live-state ack gate. See
///   [`crate::control_plane`] for the failure semantics.
/// * `secret_store` — HTTP client targeting the secret-store
///   backend on `botwork-internal`. See [`crate::secret_store`]
///   for the failure semantics.
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<DatabaseConnection>,
    pub control_plane: ControlPlaneClient,
    pub secret_store: SecretStoreClient,
}

/// Wire-shape for non-2xx responses.
///
/// Keys are stable; values are the caller-facing contract.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<ErrorRemediation>,
}

#[derive(Debug, Serialize)]
pub struct ErrorRemediation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs_url: Option<&'static str>,
}

impl ErrorDetail {
    fn new(code: &'static str, message: String) -> Self {
        Self {
            code,
            message,
            remediation: None,
        }
    }
}

/// Errors produced by handlers.
///
/// Each variant maps to a single `(StatusCode, ErrorBody)` pair via
/// [`ApiError::into_response`].
#[derive(Debug)]
pub enum ApiError {
    /// Resource lookup returned zero rows. 404 `not_found`.
    NotFound { what: &'static str, detail: String },
    /// Request structurally wrong: bad UUID in path/query, malformed
    /// JSON body, missing required field. 400 `bad_request`.
    BadRequest { detail: String },
    /// Per-entry validator rejected the body. 422 `validation_failed`.
    /// Uses 422 (not 400) so operators can distinguish "you typed the
    /// JSON wrong" from "you typed it right but it violates the
    /// schema."
    ValidationFailed { detail: String },
    /// Name failed the `^[A-Za-z0-9_-]{1,63}$` regex. 400
    /// `invalid_name`. Distinct from `bad_request` so callers can
    /// surface a name-specific error message.
    InvalidName { detail: String },
    /// Name matched the regex but appears in the reserved list. 400
    /// `reserved_name`.
    ReservedName { detail: String },
    /// Path-borne tenant (`/api/tenant/{tenant}/…`) does not match the
    /// `x-botwork-tenant` header injected by auth-broker, or the
    /// header is absent. 403 `cross_tenant_forbidden`.
    CrossTenantForbidden { path_tenant: String },
    /// Endpoint requires `x-botwork-admin` header (admin-gated surface)
    /// but the header was absent or empty. 403 `admin_required`.
    AdminRequired,
    /// Delete-guard preflight found dependents (RESTRICT would fire
    /// at the DB). 409 `has_dependents`.
    HasDependents { message: String },
    /// Optimistic-lock token didn't match the live `updated_at`. 409
    /// `stale_write`. Client should GET the entity and retry.
    StaleWrite { detail: String },
    /// Unique-constraint violation on insert (e.g. duplicate tenant
    /// `name`, duplicate binding `(workspace_id, plugin_id)`). 409
    /// `already_exists`. Distinguishable from `stale_write` so a UI
    /// can react differently.
    AlreadyExists { detail: String },
    /// Live-state coupling probe failed (control-plane unreachable
    /// or 5xx). The DB write has been rolled back. 503 `unavailable`.
    Unavailable { detail: String },
    /// DB failures, serialization errors, unexpected matches.
    /// Detail is logged but NOT echoed verbatim in the body. 500.
    Internal { detail: String },
}

impl ApiError {
    pub(crate) fn not_found(what: &'static str, detail: impl Into<String>) -> Self {
        Self::NotFound {
            what,
            detail: detail.into(),
        }
    }

    pub(crate) fn bad_request(detail: impl Into<String>) -> Self {
        Self::BadRequest {
            detail: detail.into(),
        }
    }

    pub(crate) fn validation_failed(detail: impl Into<String>) -> Self {
        Self::ValidationFailed {
            detail: detail.into(),
        }
    }

    pub(crate) fn has_dependents(message: impl Into<String>) -> Self {
        Self::HasDependents {
            message: message.into(),
        }
    }

    pub(crate) fn stale_write(detail: impl Into<String>) -> Self {
        Self::StaleWrite {
            detail: detail.into(),
        }
    }

    pub(crate) fn already_exists(detail: impl Into<String>) -> Self {
        Self::AlreadyExists {
            detail: detail.into(),
        }
    }

    pub(crate) fn unavailable(detail: impl Into<String>) -> Self {
        Self::Unavailable {
            detail: detail.into(),
        }
    }
}

impl From<DbErr> for ApiError {
    fn from(err: DbErr) -> Self {
        // `find_by_id(..).one(..)` returns Ok(None) for missing
        // rows — handled by callers via `or_not_found`. The DbErrs
        // that reach this `From` impl are genuine failures
        // (connection drops, schema mismatches, query bugs). 500.
        //
        // Note: unique-constraint violations (duplicate name on
        // insert) come through DbErr too. We could try to
        // discriminate via `err` text matching but it's brittle
        // across postgres versions. Instead, write handlers preflight
        // duplicates explicitly and emit AlreadyExists themselves;
        // the unique constraint is the belt-and-braces backstop and
        // a regression in the preflight would surface here as an
        // Internal 500, which is loud and obvious.
        error!("{PREFIX} DB error: {err}");
        Self::Internal {
            detail: format!("database error: {err}"),
        }
    }
}

impl From<ValidationError> for ApiError {
    fn from(err: ValidationError) -> Self {
        // botwork-api-core's ValidationError is the same enum
        // bootstrap consumes (lifted into BootstrapError there).
        // On the HTTP side every variant -> 422.
        Self::ValidationFailed {
            detail: err.to_string(),
        }
    }
}

impl From<NameError> for ApiError {
    fn from(err: NameError) -> Self {
        match err {
            NameError::Invalid => Self::InvalidName {
                detail: err.to_string(),
            },
            NameError::Reserved => Self::ReservedName {
                detail: err.to_string(),
            },
        }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = match self {
            ApiError::NotFound { what, detail } => (
                StatusCode::NOT_FOUND,
                ErrorBody {
                    error: ErrorDetail::new("not_found", format!("{what}: {detail}")),
                },
            ),
            ApiError::BadRequest { detail } => (
                StatusCode::BAD_REQUEST,
                ErrorBody {
                    error: ErrorDetail::new("bad_request", detail),
                },
            ),
            ApiError::ValidationFailed { detail } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorBody {
                    error: ErrorDetail::new("validation_failed", detail),
                },
            ),
            ApiError::InvalidName { detail } => (
                StatusCode::BAD_REQUEST,
                ErrorBody {
                    error: ErrorDetail::new("invalid_name", detail),
                },
            ),
            ApiError::ReservedName { detail } => (
                StatusCode::BAD_REQUEST,
                ErrorBody {
                    error: ErrorDetail::new("reserved_name", detail),
                },
            ),
            ApiError::CrossTenantForbidden { path_tenant } => (
                StatusCode::FORBIDDEN,
                ErrorBody {
                    error: ErrorDetail::new(
                        "cross_tenant_forbidden",
                        format!("path tenant {path_tenant:?} does not match authenticated tenant"),
                    ),
                },
            ),
            ApiError::AdminRequired => (
                StatusCode::FORBIDDEN,
                ErrorBody {
                    error: ErrorDetail::new(
                        "admin_required",
                        "this endpoint requires the x-botwork-admin header".to_string(),
                    ),
                },
            ),
            ApiError::HasDependents { message } => (
                StatusCode::CONFLICT,
                ErrorBody {
                    error: ErrorDetail::new("has_dependents", message),
                },
            ),
            ApiError::StaleWrite { detail } => (
                StatusCode::CONFLICT,
                ErrorBody {
                    error: ErrorDetail::new("stale_write", detail),
                },
            ),
            ApiError::AlreadyExists { detail } => (
                StatusCode::CONFLICT,
                ErrorBody {
                    error: ErrorDetail::new("already_exists", detail),
                },
            ),
            ApiError::Unavailable { detail } => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorBody {
                    error: ErrorDetail::new("unavailable", detail),
                },
            ),
            ApiError::Internal { detail } => {
                warn!("{PREFIX} internal error response: {detail}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorBody {
                        error: ErrorDetail::new("internal", "internal server error".to_string()),
                    },
                )
            }
        };
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use super::*;

    #[tokio::test]
    async fn api_error_envelope_is_structured_for_all_variants() {
        let cases = vec![
            (
                ApiError::not_found("tenant", "missing"),
                StatusCode::NOT_FOUND,
                "not_found",
            ),
            (
                ApiError::bad_request("bad request"),
                StatusCode::BAD_REQUEST,
                "bad_request",
            ),
            (
                ApiError::validation_failed("validation failed"),
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation_failed",
            ),
            (
                ApiError::InvalidName {
                    detail: "invalid name".to_string(),
                },
                StatusCode::BAD_REQUEST,
                "invalid_name",
            ),
            (
                ApiError::ReservedName {
                    detail: "reserved name".to_string(),
                },
                StatusCode::BAD_REQUEST,
                "reserved_name",
            ),
            (
                ApiError::CrossTenantForbidden {
                    path_tenant: "wrong-tenant".to_string(),
                },
                StatusCode::FORBIDDEN,
                "cross_tenant_forbidden",
            ),
            (
                ApiError::AdminRequired,
                StatusCode::FORBIDDEN,
                "admin_required",
            ),
            (
                ApiError::has_dependents("blocked by dependents"),
                StatusCode::CONFLICT,
                "has_dependents",
            ),
            (
                ApiError::stale_write("stale write"),
                StatusCode::CONFLICT,
                "stale_write",
            ),
            (
                ApiError::already_exists("already exists"),
                StatusCode::CONFLICT,
                "already_exists",
            ),
            (
                ApiError::unavailable("temporarily unavailable"),
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
            ),
            (
                ApiError::Internal {
                    detail: "db down".to_string(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
            ),
        ];

        for (error, expected_status, expected_code) in cases {
            let response = error.into_response();
            assert_eq!(response.status(), expected_status);
            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("response bytes");
            let json: serde_json::Value = serde_json::from_slice(&body).expect("response json");
            assert_eq!(json["error"]["code"], expected_code);
            assert!(json["error"]["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()));
            assert!(json["error"]["remediation"].is_null());
        }
    }
}

// ── helpers used by read + write handlers ──────────────────────────

pub(crate) trait ApiErrorExt<T> {
    fn or_not_found(self, what: &'static str, detail: impl Into<String>) -> Result<T, ApiError>;
}

impl<T> ApiErrorExt<T> for Option<T> {
    fn or_not_found(self, what: &'static str, detail: impl Into<String>) -> Result<T, ApiError> {
        self.ok_or_else(|| ApiError::not_found(what, detail))
    }
}

pub(crate) fn bad_request<E: std::fmt::Display>(prefix: &str, err: E) -> ApiError {
    ApiError::bad_request(format!("{prefix}: {err}"))
}

// ── operator identity (audit logging) ──────────────────────────────

/// Operator identity header injected by the ingress envoy's
/// ext_authz overlay. api never validates the value — the
/// envoy filter is the gate; this crate only records it.
///
/// Absent in v0 (no overlay yet); audit events log `anonymous`.
pub(crate) const ADMIN_HEADER: &str = "x-botwork-admin";

pub(crate) fn operator(headers: &HeaderMap) -> String {
    headers
        .get(ADMIN_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or("anonymous")
        .to_string()
}

/// Tenant identity header injected by the ingress envoy's ext_authz
/// overlay. api trusts the value — the envoy filter is the
/// gate; this crate reads it and uses it as the secret's scope.
///
/// Returns `ApiError::bad_request` if the header is absent or
/// empty, which means the request arrived without going through
/// ext_authz (a misconfiguration in the deployment).
pub(crate) const TENANT_HEADER: &str = "x-botwork-tenant";

/// Verify that the path-borne tenant name matches the `x-botwork-tenant`
/// header injected by auth-broker.
///
/// Returns 403 `cross_tenant_forbidden` if:
/// * the header is absent or empty (request didn't come through auth-broker), or
/// * the header value differs from `path_tenant` (cross-tenant access attempt).
///
/// This is the primary auth enforcement for all tenant-scoped endpoints.
pub(crate) fn check_tenant_consistency(
    headers: &HeaderMap,
    path_tenant: &str,
) -> Result<(), ApiError> {
    // Validate the path segment before any header match so malformed or
    // reserved names fail at parser level (400) rather than falling through
    // to DB lookup paths.
    botwork_api_core::names::validate_tenant_name(path_tenant).map_err(ApiError::from)?;
    let header_tenant = headers
        .get(TENANT_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty());
    match header_tenant {
        Some(ht) if ht == path_tenant => Ok(()),
        _ => Err(ApiError::CrossTenantForbidden {
            path_tenant: path_tenant.to_string(),
        }),
    }
}

/// Require the `x-botwork-admin` header to be present and non-empty.
///
/// Returns 403 `admin_required` if the header is absent or empty.
/// Used on admin-gated endpoints (`/api/tenants`, `/api/plugins`).
pub(crate) fn require_admin(headers: &HeaderMap) -> Result<(), ApiError> {
    let has_admin = headers
        .get(ADMIN_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .is_some();
    if has_admin {
        Ok(())
    } else {
        Err(ApiError::AdminRequired)
    }
}

/// Resolve a tenant name to its UUID. Returns 404 if no such tenant exists.
///
/// Used by tenant-scoped handlers to translate the path segment `{tenant}`
/// (a human-readable name) into the UUID primary key used for DB joins.
pub(crate) async fn resolve_tenant_id(
    db: &DatabaseConnection,
    tenant_name: &str,
) -> Result<Uuid, ApiError> {
    use botwork_entity::tenant;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    tenant::Entity::find()
        .filter(tenant::Column::Name.eq(tenant_name))
        .one(db)
        .await?
        .map(|t| t.id)
        .ok_or_else(|| {
            ApiError::not_found("tenant", format!("no tenant with name {tenant_name:?}"))
        })
}

// ── JSON body parse helper with envelope-shaped errors ─────────────
//
// We can't implement axum's `FromRequest` directly for a custom
// wrapper type while keeping the elided lifetime on `state: &S` —
// axum-core's trait declaration uses an anonymous lifetime that
// can't be matched from outside the crate (E0195). Instead we let
// handlers take `axum::Json<serde_json::Value>` (no validation, no
// custom rejection) and call `parse_body` to convert into the
// concrete type with envelope-shaped 400s. The handler bodies are
// already structured this way (`AdminJson(body): AdminJson<T>`
// followed by validation) so the change is mechanical.

/// Deserialise a `serde_json::Value` into a concrete write-body
/// type, mapping every failure into [`ApiError::BadRequest`] with
/// the response envelope shape.
///
/// Use this from write handlers right after the `Json<Value>`
/// extractor:
///
/// ```ignore
/// async fn create_tenant(
///     State(state): State<AppState>,
///     headers: HeaderMap,
///     Json(body): Json<JsonValue>,
/// ) -> Result<impl IntoResponse, ApiError> {
///     let body: TenantCreate = parse_body(body)?;
///     ...
/// }
/// ```
///
/// All write-side bodies use `#[serde(deny_unknown_fields)]` so a
/// typo in a field name is a 400 rather than a silently-dropped
/// field.
pub(crate) fn parse_body<T: serde::de::DeserializeOwned>(body: JsonValue) -> Result<T, ApiError> {
    serde_json::from_value(body)
        .map_err(|err| ApiError::bad_request(format!("invalid JSON body: {err}")))
}

// ── health (unauthed liveness probe) ──────────────────────────────

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    db: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    // SELECT 1 is the canonical "is this pool actually live" probe;
    // it round-trips the wire without touching any tables, which
    // means health stays meaningful even before any migrations have
    // run.
    let backend = state.db.get_database_backend();
    let stmt = Statement::from_string(backend, "SELECT 1".to_owned());
    let body = match state.db.execute(stmt).await {
        Ok(_) => HealthResponse {
            status: "ok",
            db: "reachable",
            message: None,
        },
        Err(err) => {
            warn!("{PREFIX} health: DB probe failed: {err}");
            HealthResponse {
                status: "ok",
                db: "unreachable",
                message: Some(err.to_string()),
            }
        }
    };
    (StatusCode::OK, Json(body))
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        // Unauthed liveness probe.
        .route("/api/health", get(health))
        .merge(read::router())
        .merge(write::router())
        .with_state(state)
}
