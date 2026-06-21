//! HTTP handlers.
//!
//! v0 surface: a single `GET /admin/api/v1/health` that returns
//! `{ "status": "ok", "db": "reachable" | "unreachable" }`. The DB
//! probe is a `SELECT 1`; if it fails the handler returns 200 with
//! `db: "unreachable"` and a `message` field. This is deliberate —
//! we want operators to be able to curl the service even when the DB
//! is sad, so the health endpoint reports DB liveness instead of
//! refusing to serve.
//!
//! All non-2xx responses share the same envelope as config-broker /
//! control-plane:
//!
//! ```json
//! { "error": "<machine code>", "message": "<human detail>" }
//! ```
//!
//! Future entity CRUD handlers (RFE #106 PR2) get bolted on under
//! `/admin/api/v1/{tenants,workspaces,plugins,workspace_plugins}`
//! and share this router + state.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde::Serialize;
use tracing::warn;

const PREFIX: &str = "[admin-api]";

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<DatabaseConnection>,
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
        .with_state(state)
}
