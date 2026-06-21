//! HTTP handler primitives: app state, error envelope, health.
//!
//! Entity-specific read handlers live in [`crate::read`]. Write
//! handlers (POST/PUT/DELETE) land in RFE #106 PR3 and will share
//! the same `AppState` + `ErrorBody` + `ApiError` shape.
//!
//! Error envelope (mirrors config-broker / control-plane):
//!
//! ```json
//! { "error": "<machine code>", "message": "<human detail>" }
//! ```
//!
//! The `ApiError` enum below maps DB errors and request-shape
//! failures into that envelope with the right `StatusCode`. The
//! whole router is `with_state(AppState)`-wired in [`build_router`].

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, Statement};
use serde::Serialize;
use tracing::{error, warn};

use crate::read;

pub(crate) const PREFIX: &str = "[admin-api]";

/// Shared state injected into every handler. `Arc<DatabaseConnection>`
/// matches what `config-broker` ships: SeaORM connections are
/// internally pooled, so cloning the `Arc` for each request is the
/// canonical pattern.
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<DatabaseConnection>,
}

/// Wire-shape for non-2xx responses. Keys are stable; values are the
/// caller-facing contract. Don't add fields without bumping the
/// route version.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    /// Machine-readable code. v0 emits: `not_found`, `bad_request`,
    /// `internal`. PR3 adds: `conflict` (delete-guard hit),
    /// `precondition_failed` (optimistic lock lost).
    pub error: &'static str,
    /// Human-readable detail. May vary across releases — clients
    /// MUST NOT pattern-match on it.
    pub message: String,
}

/// Errors produced by handlers. Each variant maps to a single
/// `(StatusCode, ErrorBody)` pair via [`ApiError::into_response`];
/// callers `?`-bubble through this so the handler bodies stay
/// branch-free on the error path.
#[derive(Debug)]
pub enum ApiError {
    /// Resource lookup returned zero rows. 404.
    NotFound { what: &'static str, detail: String },
    /// Caller-supplied data was structurally wrong (bad UUID in the
    /// path / query, etc.). 400. v0 does not yet emit `bad_request`
    /// from request *bodies* — that comes with writes in PR3.
    BadRequest { detail: String },
    /// Anything else: DB failures, serialization errors, etc. 500.
    /// The detail is logged but NOT echoed verbatim in the response
    /// body (we surface a short summary instead) to avoid leaking
    /// internal shape on the wire.
    Internal { detail: String },
}

impl ApiError {
    fn not_found(what: &'static str, detail: impl Into<String>) -> Self {
        Self::NotFound {
            what,
            detail: detail.into(),
        }
    }

    fn bad_request(detail: impl Into<String>) -> Self {
        Self::BadRequest {
            detail: detail.into(),
        }
    }
}

impl From<DbErr> for ApiError {
    fn from(err: DbErr) -> Self {
        // SeaORM's `find_by_id(..).one(..)` returns Ok(None) for
        // missing rows (handled by callers as NotFound), so the only
        // DbErrs that reach this `From` impl are genuine errors:
        // connection drops, schema mismatches, query construction
        // bugs. All -> 500.
        error!("{PREFIX} DB error: {err}");
        Self::Internal {
            detail: format!("database error: {err}"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = match self {
            ApiError::NotFound { what, detail } => (
                StatusCode::NOT_FOUND,
                ErrorBody {
                    error: "not_found",
                    message: format!("{what}: {detail}"),
                },
            ),
            ApiError::BadRequest { detail } => (
                StatusCode::BAD_REQUEST,
                ErrorBody {
                    error: "bad_request",
                    message: detail,
                },
            ),
            ApiError::Internal { detail } => {
                // Log the full detail so operators have it in
                // journalctl; respond with a short summary so the
                // wire body is bounded.
                warn!("{PREFIX} internal error response: {detail}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorBody {
                        error: "internal",
                        message: "internal server error".to_string(),
                    },
                )
            }
        };
        (status, Json(body)).into_response()
    }
}

// Helpers used by the entity read modules. Public-in-crate, not part
// of the wire surface.
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

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    db: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    // SELECT 1 is the canonical "is this pool actually live" probe;
    // it round-trips the wire without touching any tables, which means
    // health stays meaningful even before any migrations have run.
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
        .route("/admin/api/v1/health", get(health))
        .merge(read::router())
        .with_state(state)
}
