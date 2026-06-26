//! HTTP client for the remote secret-store backend.
//!
//! ## Why this exists
//!
//! `admin-api` is the operator-facing write surface for secrets.
//! Secrets themselves are NOT stored in postgres — they live in a
//! separate secret-store service (`secret_store` on
//! `botwork-internal`) that is responsible for persistence, access
//! control at the storage layer, and eventual HSM/vault integration.
//! This module is the seam between admin-api and that backend: a
//! small, stable HTTP contract that lets admin-api forward put and
//! delete operations without knowing anything about how secrets are
//! actually stored.
//!
//! ## Wire contract
//!
//! Two endpoints, keyed by `(tenant, service, name)`:
//!
//! * `POST /secrets` — create or replace a secret. Tenant is in the
//!   JSON body (never in the URL), matching the convention used
//!   everywhere else in admin-api.
//! * `DELETE /secrets/{service}/{name}?tenant={tenant}` — remove a
//!   secret. Tenant as a query param so the path is generic, still
//!   honouring the no-tenant-in-path convention at this layer.
//!
//! The backend is anonymous from admin-api's perspective — it could
//! be a cocoon-vault adapter, an HSM proxy, or a test stub. The
//! contract is small and stable by design.
//!
//! ## Configurability + escape hatch
//!
//! Endpoint comes from `BOTWORK_SECRET_STORE_ENDPOINT`
//! (default `http://secret_store:9500`, following the workspace port
//! convention: config-broker=9200, control-plane=9300,
//! admin-api=9400, secret-store=9500). Setting
//! `BOTWORK_ADMIN_API_DISABLE_SECRET_STORE=1` short-circuits all
//! calls: write handlers return 503 immediately with a clear
//! break-glass message. v0 break-glass posture only; production sets
//! this ONLY when the backend is unrecoverable and the operator
//! explicitly accepts that writes are refused. Same shape as
//! `BOTWORK_ADMIN_API_DISABLE_LIVE_GATE`.

use std::time::Duration;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::handler::PREFIX;

/// Env var holding the secret-store HTTP endpoint.
pub const ENDPOINT_ENV: &str = "BOTWORK_SECRET_STORE_ENDPOINT";

/// Default endpoint: the in-network alias on `botwork-internal` plus
/// the secret-store's HTTP port (9500).
pub const ENDPOINT_DEFAULT: &str = "http://secret_store:9500";

/// Env var that flips the secret-store client off. v0 break-glass only.
pub const DISABLE_ENV: &str = "BOTWORK_ADMIN_API_DISABLE_SECRET_STORE";

/// Per-request timeout for the secret-store round-trip.
///
/// 8s matches the control-plane gate. A secret-store that takes >8s
/// to respond is broken; returning 503 immediately is the right
/// operator-facing behaviour.
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);

/// Failure modes for secret-store calls. Each variant maps 1:1 onto
/// an [`ApiError`] in the handler layer.
///
/// [`ApiError`]: crate::handler::ApiError
#[derive(Debug)]
pub enum SecretStoreError {
    /// The client is disabled (env override set). Handlers treat
    /// this as 503 with a clear break-glass message.
    Disabled,
    /// Transport failure, 5xx from backend, or JSON parse failure.
    /// Handlers emit 503 `unavailable`.
    Unavailable(String),
    /// Backend returned 409 — the secret already exists and
    /// `overwrite: false` was specified. Handlers emit 409
    /// `already_exists`.
    AlreadyExists(String),
    /// Backend returned 404 on delete — no secret with that key.
    /// Handlers emit 404 `not_found`.
    NotFound(String),
    /// Backend returned 400 — bad base64 in value, unknown kind,
    /// etc. The backend is the authority on what is well-formed;
    /// admin-api propagates without interpreting. Handlers emit 400
    /// `bad_request`.
    BadRequest(String),
}

impl std::fmt::Display for SecretStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretStoreError::Disabled => {
                write!(f, "secret-store disabled (break-glass)")
            }
            SecretStoreError::Unavailable(msg) => {
                write!(f, "secret-store unavailable: {msg}")
            }
            SecretStoreError::AlreadyExists(msg) => write!(f, "already_exists: {msg}"),
            SecretStoreError::NotFound(msg) => write!(f, "not_found: {msg}"),
            SecretStoreError::BadRequest(msg) => write!(f, "bad_request: {msg}"),
        }
    }
}

impl std::error::Error for SecretStoreError {}

/// Lightweight HTTP client targeting the secret-store backend.
///
/// Cloneable; uses `reqwest::Client` internally which shares its
/// connection pool across clones. `AppState` holds one and clones it
/// per request.
#[derive(Clone)]
pub struct SecretStoreClient {
    endpoint: String,
    disabled: bool,
    http: reqwest::Client,
}

impl SecretStoreClient {
    /// Build a client from environment.
    ///
    /// Reads `BOTWORK_SECRET_STORE_ENDPOINT` (default
    /// `http://secret_store:9500`) and
    /// `BOTWORK_ADMIN_API_DISABLE_SECRET_STORE` (truthy to disable).
    pub fn from_env() -> Self {
        let endpoint = std::env::var(ENDPOINT_ENV).unwrap_or_else(|_| ENDPOINT_DEFAULT.to_string());
        let disabled = match std::env::var(DISABLE_ENV) {
            Ok(v) => matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"),
            Err(_) => false,
        };
        Self {
            endpoint,
            disabled,
            http: reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .expect("reqwest client build"),
        }
    }

    /// Construct a client pointed at the given endpoint.
    /// Tests use this to inject a wiremock URL.
    pub fn with_endpoint(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            disabled: false,
            http: reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .expect("reqwest client build"),
        }
    }

    /// Disabled client (test ergonomics / break-glass helper).
    pub fn disabled() -> Self {
        Self {
            endpoint: ENDPOINT_DEFAULT.to_string(),
            disabled: true,
            http: reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .expect("reqwest client build"),
        }
    }

    /// `true` if the client is disabled. Handlers branch on this so
    /// the disabled path is unambiguous in the journal.
    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// Store a secret in the backend.
    ///
    /// `POST {endpoint}/secrets`
    ///
    /// Status mapping:
    /// * 200 / 201 — parse body and return `Ok(PutSecretResponse)`.
    /// * 400 — `BadRequest(body)`.
    /// * 409 — `AlreadyExists(body)`.
    /// * everything else / transport / parse — `Unavailable(...)`.
    pub async fn put_secret(
        &self,
        req: PutSecretRequest,
    ) -> Result<PutSecretResponse, SecretStoreError> {
        if self.disabled {
            return Err(SecretStoreError::Disabled);
        }
        let url = format!("{}/secrets", self.endpoint);
        // Never log value_b64 — it is the secret material.
        debug!(
            "{PREFIX} secret-store put: POST {url} service={} name={} kind={} overwrite={}",
            req.service, req.name, req.kind, req.overwrite
        );
        let resp = self
            .http
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|err| SecretStoreError::Unavailable(format!("POST /secrets: {err}")))?;
        let status = resp.status();
        match status {
            s if s == StatusCode::OK || s == StatusCode::CREATED => {
                resp.json::<PutSecretResponse>().await.map_err(|err| {
                    SecretStoreError::Unavailable(format!("POST /secrets: parse response: {err}"))
                })
            }
            StatusCode::BAD_REQUEST => {
                let body = resp.text().await.unwrap_or_default();
                warn!("{PREFIX} secret-store put: backend returned 400: {body}");
                Err(SecretStoreError::BadRequest(body))
            }
            StatusCode::CONFLICT => {
                let body = resp.text().await.unwrap_or_default();
                warn!("{PREFIX} secret-store put: backend returned 409: {body}");
                Err(SecretStoreError::AlreadyExists(body))
            }
            other => {
                let body = resp.text().await.unwrap_or_default();
                warn!("{PREFIX} secret-store put: backend returned {other}: {body}");
                Err(SecretStoreError::Unavailable(format!(
                    "POST /secrets returned {other}"
                )))
            }
        }
    }

    /// Remove a secret from the backend.
    ///
    /// `DELETE {endpoint}/secrets/{service}/{name}?tenant={tenant}`
    ///
    /// Tenant is a query param — not a path segment — matching the
    /// no-tenant-in-path convention used throughout admin-api.
    ///
    /// Status mapping:
    /// * 200 / 204 — `Ok(())`.
    /// * 400 — `BadRequest(body)`.
    /// * 404 — `NotFound(body)`.
    /// * everything else / transport — `Unavailable(...)`.
    pub async fn delete_secret(
        &self,
        tenant: &str,
        service: &str,
        name: &str,
    ) -> Result<(), SecretStoreError> {
        if self.disabled {
            return Err(SecretStoreError::Disabled);
        }
        let url = format!("{}/secrets/{service}/{name}", self.endpoint);
        debug!("{PREFIX} secret-store delete: DELETE {url}?tenant={tenant}");
        let resp = self
            .http
            .delete(&url)
            .query(&[("tenant", tenant)])
            .send()
            .await
            .map_err(|err| {
                SecretStoreError::Unavailable(format!("DELETE /secrets/{service}/{name}: {err}"))
            })?;
        let status = resp.status();
        match status {
            s if s == StatusCode::OK || s == StatusCode::NO_CONTENT => Ok(()),
            StatusCode::BAD_REQUEST => {
                let body = resp.text().await.unwrap_or_default();
                warn!("{PREFIX} secret-store delete: backend returned 400: {body}");
                Err(SecretStoreError::BadRequest(body))
            }
            StatusCode::NOT_FOUND => {
                let body = resp.text().await.unwrap_or_default();
                Err(SecretStoreError::NotFound(body))
            }
            other => {
                let body = resp.text().await.unwrap_or_default();
                warn!("{PREFIX} secret-store delete: backend returned {other}: {body}");
                Err(SecretStoreError::Unavailable(format!(
                    "DELETE /secrets/{service}/{name} returned {other}"
                )))
            }
        }
    }
}

// ── wire types ─────────────────────────────────────────────────────

/// Request body for `PUT /secrets` on the backend.
#[derive(Debug, Serialize)]
pub struct PutSecretRequest {
    pub tenant: String,
    pub service: String,
    pub name: String,
    pub kind: String,
    /// Base64-encoded secret value. Never logged anywhere in
    /// admin-api — it is opaque at this layer.
    pub value_b64: String,
    pub allowed_consumers: Vec<String>,
    pub tags: Vec<String>,
    pub overwrite: bool,
}

/// Response body from `POST /secrets` on the backend.
#[derive(Debug, Deserialize, Serialize)]
pub struct PutSecretResponse {
    /// Canonical identifier of the stored secret as returned by
    /// the backend (e.g. `"github.com/pat"`).
    pub stored: String,
    /// `true` if this was a net-new secret; `false` if it
    /// overwrote an existing one.
    pub created: bool,
}
