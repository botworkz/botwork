//! HTTP client for the remote secret-store backend.
//!
//! ## Why this exists
//!
//! `api` is the operator-facing write surface for secrets.
//! Secrets themselves are NOT stored in postgres — they live in a
//! separate secret-store service (`secret_store` on
//! `botwork-internal`) that is responsible for persistence, access
//! control at the storage layer, and eventual HSM/vault integration.
//! This module is the seam between api and that backend: a
//! small, stable HTTP contract that lets api forward put and
//! delete operations without knowing anything about how secrets are
//! actually stored.
//!
//! ## Wire contract
//!
//! Two endpoints, keyed by `(tenant, service, name)`:
//!
//! * `POST /secrets` — create or replace a secret. Tenant is in the
//!   JSON body (never in the URL), matching the convention used
//!   everywhere else in api.
//! * `DELETE /secrets/{service}/{name}?tenant={tenant}` — remove a
//!   secret. Tenant as a query param so the path is generic, still
//!   honouring the no-tenant-in-path convention at this layer.
//!
//! The backend is anonymous from api's perspective — it could
//! be a cocoon-vault adapter, an HSM proxy, or a test stub. The
//! contract is small and stable by design.
//!
//! ## Configurability + escape hatch
//!
//! Endpoint comes from `BOTWORK_SECRET_STORE_ENDPOINT`
//! (default `http://secret_store:9500`, following the workspace port
//! convention: config-broker=9200, control-plane=9300,
//! api=9400, secret-store=9500). Setting
//! `BOTWORK_API_DISABLE_SECRET_STORE=1` short-circuits all
//! calls: write handlers return 503 immediately with a clear
//! break-glass message. v0 break-glass posture only; production sets
//! this ONLY when the backend is unrecoverable and the operator
//! explicitly accepts that writes are refused. Same shape as
//! `BOTWORK_API_DISABLE_LIVE_GATE`.

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
pub const DISABLE_ENV: &str = "BOTWORK_API_DISABLE_SECRET_STORE";

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
    /// api propagates without interpreting. Handlers emit 400
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
    /// `BOTWORK_API_DISABLE_SECRET_STORE` (truthy to disable).
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
    /// no-tenant-in-path convention used throughout api.
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

/// Request body for `POST /secrets` on the backend.
#[derive(Debug, Serialize)]
pub struct PutSecretRequest {
    pub tenant: String,
    pub service: String,
    pub name: String,
    pub kind: String,
    /// Base64-encoded secret value. Never logged anywhere in
    /// api — it is opaque at this layer.
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

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    struct EnvGuard {
        endpoint: Option<String>,
        disabled: Option<String>,
    }

    impl EnvGuard {
        fn capture() -> Self {
            Self {
                endpoint: std::env::var(ENDPOINT_ENV).ok(),
                disabled: std::env::var(DISABLE_ENV).ok(),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(v) = &self.endpoint {
                std::env::set_var(ENDPOINT_ENV, v);
            } else {
                std::env::remove_var(ENDPOINT_ENV);
            }
            if let Some(v) = &self.disabled {
                std::env::set_var(DISABLE_ENV, v);
            } else {
                std::env::remove_var(DISABLE_ENV);
            }
        }
    }

    fn sample_put() -> PutSecretRequest {
        PutSecretRequest {
            tenant: "phlax".to_string(),
            service: "github.com".to_string(),
            name: "pat".to_string(),
            kind: "opaque".to_string(),
            value_b64: "dGVzdA==".to_string(),
            allowed_consumers: vec![],
            tags: vec![],
            overwrite: false,
        }
    }

    #[test]
    fn from_env_honors_default_and_disable_flag() {
        let _guard = EnvGuard::capture();
        std::env::remove_var(ENDPOINT_ENV);
        std::env::remove_var(DISABLE_ENV);

        let default_client = SecretStoreClient::from_env();
        assert_eq!(default_client.endpoint, ENDPOINT_DEFAULT);
        assert!(!default_client.is_disabled());

        std::env::set_var(DISABLE_ENV, "1");
        let disabled_client = SecretStoreClient::from_env();
        assert!(disabled_client.is_disabled());
    }

    #[tokio::test]
    async fn disabled_client_short_circuits_operations() {
        let client = SecretStoreClient::disabled();
        assert!(matches!(
            client.put_secret(sample_put()).await,
            Err(SecretStoreError::Disabled)
        ));
        assert!(matches!(
            client.delete_secret("phlax", "github.com", "pat").await,
            Err(SecretStoreError::Disabled)
        ));
    }

    #[tokio::test]
    async fn put_secret_maps_success_and_conflict() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/secrets"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "stored": "github.com/pat",
                "created": true
            })))
            .mount(&server)
            .await;
        let client = SecretStoreClient::with_endpoint(server.uri());
        let ok = client.put_secret(sample_put()).await.expect("put response");
        assert_eq!(ok.stored, "github.com/pat");
        assert!(ok.created);

        let conflict_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/secrets"))
            .respond_with(ResponseTemplate::new(409).set_body_string("exists"))
            .mount(&conflict_server)
            .await;
        let client = SecretStoreClient::with_endpoint(conflict_server.uri());
        let err = client.put_secret(sample_put()).await.expect_err("conflict");
        assert!(matches!(err, SecretStoreError::AlreadyExists(_)));
    }

    #[tokio::test]
    async fn delete_secret_maps_not_found_and_success() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/secrets/github.com/pat"))
            .and(query_param("tenant", "phlax"))
            .respond_with(ResponseTemplate::new(404).set_body_string("missing"))
            .mount(&server)
            .await;
        let client = SecretStoreClient::with_endpoint(server.uri());
        let err = client
            .delete_secret("phlax", "github.com", "pat")
            .await
            .expect_err("not found");
        assert!(matches!(err, SecretStoreError::NotFound(_)));

        let success_server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/secrets/github.com/pat"))
            .and(query_param("tenant", "phlax"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&success_server)
            .await;
        let client = SecretStoreClient::with_endpoint(success_server.uri());
        client
            .delete_secret("phlax", "github.com", "pat")
            .await
            .expect("delete ok");
    }

    #[tokio::test]
    async fn unreachable_endpoint_maps_to_unavailable() {
        let client = SecretStoreClient::with_endpoint("http://127.0.0.1:1");
        let err = client
            .put_secret(sample_put())
            .await
            .expect_err("unavailable");
        assert!(matches!(err, SecretStoreError::Unavailable(_)));
    }

<<<<<<< HEAD
    // ── Tier 1.5 fault-injection tests ─────────────────────────────

    #[tokio::test]
    async fn put_secret_maps_bad_request_to_error() {
        // 400 from backend → SecretStoreError::BadRequest
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/secrets"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad value"))
            .mount(&server)
            .await;
        let client = SecretStoreClient::with_endpoint(server.uri());
        let err = client
            .put_secret(sample_put())
            .await
            .expect_err("bad request");
        assert!(
            matches!(&err, SecretStoreError::BadRequest(msg) if msg == "bad value"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn put_secret_maps_other_status_to_unavailable() {
        // 500 (or any other non-200/201/400/409 status) → SecretStoreError::Unavailable
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/secrets"))
            .respond_with(ResponseTemplate::new(500).set_body_string("server error"))
            .mount(&server)
            .await;
        let client = SecretStoreClient::with_endpoint(server.uri());
        let err = client
            .put_secret(sample_put())
            .await
            .expect_err("unavailable");
        assert!(matches!(err, SecretStoreError::Unavailable(_)), "{err:?}");
    }

    #[tokio::test]
    async fn put_secret_maps_parse_failure_to_unavailable() {
        // 200 with non-JSON body → json parse failure → SecretStoreError::Unavailable
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/secrets"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json at all"))
            .mount(&server)
            .await;
        let client = SecretStoreClient::with_endpoint(server.uri());
        let err = client
            .put_secret(sample_put())
            .await
            .expect_err("parse error");
        assert!(matches!(err, SecretStoreError::Unavailable(_)), "{err:?}");
    }

    #[tokio::test]
    async fn delete_secret_transport_error_maps_to_unavailable() {
        // Connection refused → SecretStoreError::Unavailable
        let client = SecretStoreClient::with_endpoint("http://127.0.0.1:1");
        let err = client
            .delete_secret("phlax", "github.com", "pat")
            .await
            .expect_err("unavailable");
        assert!(matches!(err, SecretStoreError::Unavailable(_)), "{err:?}");
=======
    #[test]
    fn display_trait_covers_all_variants() {
        let disabled = SecretStoreError::Disabled;
        assert!(disabled.to_string().contains("disabled"));

        let unavail = SecretStoreError::Unavailable("conn refused".to_string());
        assert!(unavail.to_string().contains("unavailable"));
        assert!(unavail.to_string().contains("conn refused"));

        let exists = SecretStoreError::AlreadyExists("secret already there".to_string());
        assert!(exists.to_string().contains("already_exists"));
        assert!(exists.to_string().contains("already there"));

        let not_found = SecretStoreError::NotFound("no such secret".to_string());
        assert!(not_found.to_string().contains("not_found"));
        assert!(not_found.to_string().contains("no such secret"));

        let bad_req = SecretStoreError::BadRequest("bad base64".to_string());
        assert!(bad_req.to_string().contains("bad_request"));
        assert!(bad_req.to_string().contains("bad base64"));
    }

    #[tokio::test]
    async fn put_secret_maps_200_ok_to_success() {
        // Backend returning 200 (rather than 201) must also be treated as
        // success — both are in the `s == StatusCode::OK || s == StatusCode::CREATED`
        // arm at line 208.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/secrets"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "stored": "github.com/pat",
                "created": false
            })))
            .mount(&server)
            .await;
        let client = SecretStoreClient::with_endpoint(server.uri());
        let ok = client.put_secret(sample_put()).await.expect("put 200 ok");
        assert_eq!(ok.stored, "github.com/pat");
        assert!(!ok.created);
>>>>>>> origin/main
    }

    #[tokio::test]
    async fn delete_secret_maps_200_ok_to_success() {
<<<<<<< HEAD
        // 200 is in the OK family (alongside 204)
=======
        // Backend returning 200 (rather than 204) must also be treated as
        // success — both are in the `s == StatusCode::OK || s == StatusCode::NO_CONTENT`
        // arm at line 267.
>>>>>>> origin/main
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/secrets/github.com/pat"))
            .and(query_param("tenant", "phlax"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let client = SecretStoreClient::with_endpoint(server.uri());
        client
            .delete_secret("phlax", "github.com", "pat")
            .await
<<<<<<< HEAD
            .expect("200 should be success");
    }

    #[tokio::test]
    async fn delete_secret_maps_bad_request_to_error() {
        // 400 from backend → SecretStoreError::BadRequest
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/secrets/github.com/pat"))
            .and(query_param("tenant", "phlax"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad"))
            .mount(&server)
            .await;
        let client = SecretStoreClient::with_endpoint(server.uri());
        let err = client
            .delete_secret("phlax", "github.com", "pat")
            .await
            .expect_err("bad request");
        assert!(
            matches!(&err, SecretStoreError::BadRequest(msg) if msg == "bad"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn delete_secret_maps_other_status_to_unavailable() {
        // 500 (or any other non-200/204/400/404 status) → SecretStoreError::Unavailable
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/secrets/github.com/pat"))
            .and(query_param("tenant", "phlax"))
            .respond_with(ResponseTemplate::new(500).set_body_string("error"))
            .mount(&server)
            .await;
        let client = SecretStoreClient::with_endpoint(server.uri());
        let err = client
            .delete_secret("phlax", "github.com", "pat")
            .await
            .expect_err("unavailable");
        assert!(matches!(err, SecretStoreError::Unavailable(_)), "{err:?}");
    }

    #[test]
    fn error_display_covers_all_variants() {
        // Every Display arm in SecretStoreError
        assert_eq!(
            format!("{}", SecretStoreError::Disabled),
            "secret-store disabled (break-glass)"
        );
        assert_eq!(
            format!("{}", SecretStoreError::Unavailable("msg".to_string())),
            "secret-store unavailable: msg"
        );
        assert_eq!(
            format!("{}", SecretStoreError::AlreadyExists("dup".to_string())),
            "already_exists: dup"
        );
        assert_eq!(
            format!("{}", SecretStoreError::NotFound("key".to_string())),
            "not_found: key"
        );
        assert_eq!(
            format!("{}", SecretStoreError::BadRequest("bad".to_string())),
            "bad_request: bad"
=======
            .expect("delete 200 ok");
    }

    #[test]
    fn from_env_recognizes_all_truthy_disable_values() {
        let _guard = EnvGuard::capture();
        for truthy in &["true", "TRUE", "yes", "YES"] {
            std::env::set_var(DISABLE_ENV, truthy);
            let client = SecretStoreClient::from_env();
            assert!(
                client.is_disabled(),
                "expected disabled=true for DISABLE_ENV={truthy}"
            );
        }
        // "0" is NOT truthy
        std::env::set_var(DISABLE_ENV, "0");
        let not_disabled = SecretStoreClient::from_env();
        assert!(
            !not_disabled.is_disabled(),
            "expected disabled=false for DISABLE_ENV=0"
>>>>>>> origin/main
        );
    }
}
