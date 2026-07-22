//! Admin endpoints — incident-response lease revocation.
//!
//! Mounted by [`crate::handler::build_router`] under `/admin/`:
//!
//! | Endpoint                      | Verb   | Purpose                              |
//! |-------------------------------|--------|--------------------------------------|
//! | `/admin/api/v1/leases/{id}`   | DELETE | Revoke a lease by UUID; evict caps.  |
//!
//! ## Authentication
//!
//! All admin endpoints require `Authorization: Bearer <KEY>` where the
//! key matches `BOTWORK_ADMIN_API_KEY`. If that env var is unset (reflected as
//! [`AppState::admin_api_key`] being `None`), every admin call returns 401 —
//! the surface is disabled by default so a freshly deployed broker without the
//! env var cannot be exploited.
//!
//! ## Revocation semantics
//!
//! `DELETE /admin/api/v1/leases/{id}` does two things atomically from the
//! operator's perspective:
//!
//! 1. Sets `revoked_at = now()` on the postgres `lease` row (via
//!    [`LeaseStore::revoke_by_id`][crate::store::LeaseStore::revoke_by_id]).
//! 2. Evicts every in-memory cap whose `lease_id` matches via
//!    [`evict_caps_for_lease`][crate::cache::evict_caps_for_lease].
//!
//! Both steps are required to fully invalidate a compromised bearer:
//! postgres invalidation prevents re-login from the same lease row;
//! cap eviction prevents an in-flight cap from being reused.
//!
//! The operation is **idempotent** — calling it on an already-revoked
//! lease returns `{"revoked": 0, "caps_evicted": 0}` rather than an
//! error.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::delete;
use axum::{Json, Router};
use chrono::Utc;
use serde::Serialize;
use subtle::ConstantTimeEq;
use tracing::{info, warn};
use uuid::Uuid;

use crate::cache::{evict_caps_for_lease, AppState};
use crate::handler::extract_bearer;

const PREFIX: &str = "[auth-broker/admin]";

// ---------------------------------------------------------------------------
// Error envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct AdminErrorEnvelope {
    error: AdminErrorBody,
}

#[derive(Debug, Serialize)]
struct AdminErrorBody {
    code: &'static str,
    message: &'static str,
}

fn admin_unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(AdminErrorEnvelope {
            error: AdminErrorBody {
                code: "unauthorized",
                message: "valid admin bearer required",
            },
        }),
    )
        .into_response()
}

fn admin_internal(message: &'static str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(AdminErrorEnvelope {
            error: AdminErrorBody {
                code: "internal",
                message,
            },
        }),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Auth guard
// ---------------------------------------------------------------------------

/// Returns `Ok(())` if the request carries the correct admin bearer,
/// `Err(401 response)` otherwise.
///
/// When [`AppState::admin_api_key`] is `None` (env var unset) the admin
/// surface is disabled and every call returns 401.
fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    let Some(admin_key) = state.admin_api_key.as_deref() else {
        return Err(Box::new(admin_unauthorized()));
    };
    let auth_value = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    match extract_bearer(auth_value) {
        Some(provided) if bool::from(provided.as_bytes().ct_eq(admin_key.as_bytes())) => Ok(()),
        _ => Err(Box::new(admin_unauthorized())),
    }
}

// ---------------------------------------------------------------------------
// DELETE /admin/api/v1/leases/{id}
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct RevokeLeaseResponse {
    /// Number of postgres `lease` rows whose `revoked_at` was set.
    /// `0` means the lease was not found or was already revoked.
    revoked: u64,
    /// Number of in-memory cap entries that were evicted as part of
    /// the lease-cohort eviction.
    caps_evicted: usize,
}

async fn admin_revoke_lease(
    State(state): State<AppState>,
    Path(lease_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = require_admin(&state, &headers) {
        return *resp;
    }

    let now = Utc::now();
    let revoked = match state.auth.lease_store.revoke_by_id(lease_id, now).await {
        Ok(n) => n,
        Err(err) => {
            warn!("{PREFIX} revoke_by_id failed lease_id={lease_id} err={err}");
            return admin_internal("database error during lease revocation");
        }
    };

    let caps_evicted = evict_caps_for_lease(&state, lease_id).await;

    info!("{PREFIX} revoked lease_id={lease_id} revoked={revoked} caps_evicted={caps_evicted}");

    Json(RevokeLeaseResponse {
        revoked,
        caps_evicted,
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Build the `/admin/api/v1/leases/:id` router.
///
/// Composed into the top-level router by
/// [`crate::handler::build_router`].
pub fn build_admin_router(state: AppState) -> Router {
    Router::new()
        .route("/admin/api/v1/leases/{id}", delete(admin_revoke_lease))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use axum::http::header::AUTHORIZATION;
    use botwork_opaque_handshake::ServerSetup;

    use crate::store::mock::{MockLeaseStore, MockPasswordFileStore, MockTenantStore};

    const ADMIN_KEY: &str = "test-admin-key";

    fn build_state(admin_key: Option<&str>) -> AppState {
        let auth = crate::auth::AuthState::from_stores(
            Arc::new(MockLeaseStore::new()),
            Arc::new(MockTenantStore::new()),
            Arc::new(MockPasswordFileStore::new()),
            ServerSetup::generate(&mut rand::thread_rng()),
        );
        let state = AppState::with_auth(std::env::temp_dir(), auth);
        match admin_key {
            Some(key) => state.with_admin_api_key(key),
            None => state,
        }
    }

    fn auth_headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, value.parse().unwrap());
        headers
    }

    #[test]
    fn require_admin_accepts_correct_bearer() {
        let state = build_state(Some(ADMIN_KEY));
        let headers = auth_headers(&(String::from("Bearer ") + ADMIN_KEY));

        assert!(require_admin(&state, &headers).is_ok());
    }

    #[test]
    fn require_admin_rejects_wrong_bearer() {
        let state = build_state(Some(ADMIN_KEY));
        let headers = auth_headers(&(String::from("Bearer ") + "wrong-key"));

        let err = require_admin(&state, &headers).unwrap_err();
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn require_admin_rejects_missing_key_or_malformed_bearer() {
        let disabled_state = build_state(None);
        let valid_headers = auth_headers(&(String::from("Bearer ") + ADMIN_KEY));
        let disabled = require_admin(&disabled_state, &valid_headers).unwrap_err();
        assert_eq!(disabled.status(), StatusCode::UNAUTHORIZED);

        let enabled_state = build_state(Some(ADMIN_KEY));
        let malformed_headers = auth_headers("Basic not-a-bearer");
        let malformed = require_admin(&enabled_state, &malformed_headers).unwrap_err();
        assert_eq!(malformed.status(), StatusCode::UNAUTHORIZED);
    }
}
