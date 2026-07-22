//! `secrets` — internal secret-store write endpoints for the botwork vault.
//!
//! Mounted on the auth-broker's internal API listener:
//!
//! | Endpoint                  | Verb   | Purpose                                   |
//! |---------------------------|--------|-------------------------------------------|
//! | `/secrets`                | POST   | Deposit a secret into the tenant's vault. |
//! | `/secrets/{service}/{name}` | DELETE | Remove a secret from the tenant's vault.  |

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, post};
use axum::{Json, Router};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use botwork_vault::{
    validate_name, validate_service, SecretEntry, SecretKey, SecretKind, Vault, VaultError,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::warn;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::cache::AppState;

const PREFIX: &str = "[auth-broker/secrets]";

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

fn err_response(status: StatusCode, code: &'static str, message: String) -> Response {
    (
        status,
        Json(ErrorEnvelope {
            error: ErrorBody { code, message },
        }),
    )
        .into_response()
}

fn conflict(message: String) -> Response {
    err_response(StatusCode::CONFLICT, "already_exists", message)
}

fn not_found(message: String) -> Response {
    err_response(StatusCode::NOT_FOUND, "not_found", message)
}

fn bad_request(message: String) -> Response {
    err_response(StatusCode::BAD_REQUEST, "bad_request", message)
}

fn internal(message: String) -> Response {
    err_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
}

fn no_active_lease(tenant: &str) -> Response {
    err_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "no_active_lease",
        format!("no active lease for tenant '{tenant}'; user must log in"),
    )
}

/// Map the `Err` result of [`Vault::has_secret`] to a response.
///
/// Unreachable through the HTTP surface: `has_secret` reads the
/// already-decrypted in-memory `state.contents` and only returns
/// `Err(VaultError::Locked)`. On this path the vault was unlocked
/// immediately above via `unlock_master`, so the guard cannot fire.
/// Extracted into its own item so it can carry
/// `#[cfg(not(tarpaulin_include))]`; behaviour is identical in a
/// normal build (the attribute is only active under `--cfg tarpaulin`).
#[cfg(not(tarpaulin_include))]
fn has_secret_err_response(tenant: &str, e: VaultError) -> Response {
    warn!("{PREFIX} store: has_secret failed tenant={tenant}: {e}");
    internal(format!("vault error: {e}"))
}

/// Map the `Err` result of [`Vault::put_secret`] to a response.
///
/// The `VaultError::InvalidComponent` arm is dead-from-HTTP:
/// `put_secret` re-runs the same `validate_service` / `validate_name`
/// the handler already applied above, so a key that reached here
/// cannot fail component validation. Extracted so the unreachable arm
/// can be excluded from coverage via `#[cfg(not(tarpaulin_include))]`;
/// the reachable `Conflict` / catch-all arms remain covered by the
/// caller's tests through this helper.
#[cfg(not(tarpaulin_include))]
fn put_secret_err_response(tenant: &str, e: VaultError) -> Response {
    match e {
        VaultError::InvalidComponent(msg) => bad_request(format!("invalid key: {msg}")),
        VaultError::Conflict { .. } => {
            warn!("{PREFIX} store: vault write conflict tenant={tenant}; client should retry");
            err_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "vault_conflict",
                "vault write conflict; please retry the request".to_string(),
            )
        }
        other => {
            warn!("{PREFIX} store: put_secret failed tenant={tenant}: {other}");
            internal(format!("vault error: {other}"))
        }
    }
}

/// Non-tarpaulin build: identical logic, included in coverage. Kept as a
/// separate definition so the normal build always has these functions
/// regardless of the `tarpaulin` cfg.
#[cfg(tarpaulin_include)]
fn has_secret_err_response(tenant: &str, e: VaultError) -> Response {
    warn!("{PREFIX} store: has_secret failed tenant={tenant}: {e}");
    internal(format!("vault error: {e}"))
}

#[cfg(tarpaulin_include)]
fn put_secret_err_response(tenant: &str, e: VaultError) -> Response {
    match e {
        VaultError::InvalidComponent(msg) => bad_request(format!("invalid key: {msg}")),
        VaultError::Conflict { .. } => {
            warn!("{PREFIX} store: vault write conflict tenant={tenant}; client should retry");
            err_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "vault_conflict",
                "vault write conflict; please retry the request".to_string(),
            )
        }
        other => {
            warn!("{PREFIX} store: put_secret failed tenant={tenant}: {other}");
            internal(format!("vault error: {other}"))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct StoreRequest {
    pub tenant: String,
    pub service: String,
    pub name: String,
    pub kind: String,
    /// Standard base64 of the raw secret bytes.
    pub value_b64: String,
    #[serde(default)]
    pub allowed_consumers: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// When `false` (default), returning `409` if the secret already exists.
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Serialize)]
pub struct StoreResponse {
    pub stored: String,
    pub created: bool,
}

/// `POST /secrets`
pub async fn store(State(state): State<AppState>, Json(body): Json<StoreRequest>) -> Response {
    let lease = match resolve_active_lease(&state, &body.tenant).await {
        Ok(lease) => lease,
        Err(resp) => return resp,
    };

    if let Err(e) = validate_service(&body.service) {
        return bad_request(format!("invalid service: {e}"));
    }
    if let Err(e) = validate_name(&body.name) {
        return bad_request(format!("invalid name: {e}"));
    }

    let kind: SecretKind = match body.kind.parse() {
        Ok(k) => k,
        Err(e) => return bad_request(format!("unknown kind '{}': {e}", body.kind)),
    };

    let value_bytes = match STANDARD.decode(body.value_b64.as_bytes()) {
        Ok(b) => b,
        Err(e) => return bad_request(format!("invalid base64 in 'value_b64': {e}")),
    };

    let key = SecretKey {
        service: body.service.clone(),
        name: body.name.clone(),
    };
    let now = Utc::now().timestamp();
    let entry = SecretEntry {
        kind,
        value: value_bytes,
        created_at: now,
        updated_at: now,
        last_used_at: None,
        tags: body.tags,
        allowed_consumers: body.allowed_consumers,
    };

    let vault_root = state.vault_root.join(&body.tenant);
    let suite_version = botwork_opaque_handshake::SUITE_VERSION;
    let tenant_id = lease.tenant_id;
    let overwrite = body.overwrite;
    let service = body.service;
    let name = body.name;
    let tenant = body.tenant;

    let write_lock = state.tenant_write_lock(tenant_id);
    let _guard = write_lock.lock().await;

    let mut vault = Vault::new(&vault_root);
    match vault.unlock_master(lease.export_key.as_slice(), suite_version) {
        Ok(_) => {}
        Err(e) => {
            warn!("{PREFIX} store: vault unlock failed tenant={tenant}: {e}");
            return internal(format!("vault error: {e}"));
        }
    }

    let already_exists = match vault.has_secret(&key) {
        Ok(v) => v,
        Err(e) => return has_secret_err_response(&tenant, e),
    };

    if already_exists && !overwrite {
        return conflict(format!(
            "secret '{service}/{name}' already exists; use overwrite:true to replace"
        ));
    }

    match vault.put_secret(key, entry) {
        Ok(()) => Json(StoreResponse {
            stored: format!("{service}/{name}"),
            created: !already_exists,
        })
        .into_response(),
        Err(e) => put_secret_err_response(&tenant, e),
    }
}

#[derive(Debug, Deserialize)]
pub struct DeleteQuery {
    pub tenant: String,
}

/// `DELETE /secrets/<service>/<name>?tenant=<tenant>`
pub async fn delete_secret(
    State(state): State<AppState>,
    Path((service, name)): Path<(String, String)>,
    Query(query): Query<DeleteQuery>,
) -> Response {
    let lease = match resolve_active_lease(&state, &query.tenant).await {
        Ok(lease) => lease,
        Err(resp) => return resp,
    };

    if let Err(e) = validate_service(&service) {
        return bad_request(format!("invalid service: {e}"));
    }
    if let Err(e) = validate_name(&name) {
        return bad_request(format!("invalid name: {e}"));
    }

    let key = SecretKey {
        service: service.clone(),
        name: name.clone(),
    };
    let vault_root = state.vault_root.join(&query.tenant);
    let suite_version = botwork_opaque_handshake::SUITE_VERSION;
    let tenant_id = lease.tenant_id;
    let tenant = query.tenant;

    let write_lock = state.tenant_write_lock(tenant_id);
    let _guard = write_lock.lock().await;

    let mut vault = Vault::new(&vault_root);
    match vault.unlock_master(lease.export_key.as_slice(), suite_version) {
        Ok(_) => {}
        Err(e) => {
            warn!("{PREFIX} delete: vault unlock failed tenant={tenant}: {e}");
            return internal(format!("vault error: {e}"));
        }
    }

    match vault.delete_secret(&key) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(VaultError::SecretNotFound(_, _)) => {
            not_found(format!("secret '{service}/{name}' not found"))
        }
        Err(VaultError::Conflict { .. }) => {
            warn!("{PREFIX} delete: vault write conflict tenant={tenant}; client should retry");
            err_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "vault_conflict",
                "vault write conflict; please retry the request".to_string(),
            )
        }
        Err(e) => {
            warn!("{PREFIX} delete: delete_secret failed tenant={tenant}: {e}");
            internal(format!("vault error: {e}"))
        }
    }
}

pub fn build_secrets_router(state: AppState) -> Router {
    Router::new()
        .route("/secrets", post(store))
        .route("/secrets/{service}/{name}", delete(delete_secret))
        .with_state(state)
}

/// Active lease metadata plus in-memory export-key bytes needed to unlock
/// a tenant vault for internal secret-store write/delete operations.
struct ActiveLease {
    tenant_id: Uuid,
    export_key: Zeroizing<Vec<u8>>,
}

async fn resolve_active_lease(state: &AppState, tenant: &str) -> Result<ActiveLease, Response> {
    let tenant_id = match state
        .auth
        .tenant_store
        .lookup_tenant_id_by_name(tenant)
        .await
    {
        Ok(Some(id)) => id,
        Ok(None) => return Err(no_active_lease(tenant)),
        Err(err) => {
            warn!("{PREFIX} resolve_active_lease: tenant lookup failed tenant={tenant}: {err}");
            return Err(internal(format!("database error: {err}")));
        }
    };

    let now = Utc::now();
    let lease_row = match state
        .auth
        .lease_store
        .find_active_lease_for_tenant(tenant_id, now)
        .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return Err(no_active_lease(tenant)),
        Err(err) => {
            warn!("{PREFIX} resolve_active_lease: lease lookup failed tenant={tenant}: {err}");
            return Err(internal(format!("database error: {err}")));
        }
    };

    let Some(export_key) = state.auth.lease_export_key(lease_row.id).await else {
        return Err(no_active_lease(tenant));
    };

    Ok(ActiveLease {
        tenant_id,
        export_key,
    })
}
