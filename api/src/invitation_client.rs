//! HTTP client for the auth-broker invitation minting endpoint.
//!
//! ## Why this exists
//!
//! When `POST /api/tenants` creates a new tenant the admin must receive a
//! single-use, time-limited OTP that the tenant later presents at register
//! time to prove they are the intended holder of the reserved name. The OTP
//! is minted by **auth-broker** (auth-broker owns the `invitations` table)
//! via its internal `POST /internal/invitations` endpoint.
//!
//! This module is the seam between api and auth-broker for that cold-path
//! mint call.
//!
//! ## Wire contract
//!
//! `POST {endpoint}/internal/invitations`
//! Body: `{ "tenant_id": "<uuid>" }`
//! Response (201): `{ "otp": "<plaintext>", "expires_at": "<RFC3339>" }`
//!
//! ## Disabled mode
//!
//! `BOTWORK_AUTH_BROKER_INVITATIONS_DISABLE=1` (or equivalently
//! `disabled()`) skips the auth-broker call and returns a fixed
//! `"DISABLED"` placeholder OTP. Used in tests and during break-glass
//! operation when auth-broker is unavailable and the operator explicitly
//! accepts the reduced security posture.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use uuid::Uuid;

use crate::handler::PREFIX;

/// Env var holding the auth-broker HTTP endpoint.
pub const ENDPOINT_ENV: &str = "BOTWORK_AUTH_BROKER_ENDPOINT";

/// Default endpoint: the in-network alias on `botwork-internal` plus
/// the auth-broker's HTTP port (9600).
pub const ENDPOINT_DEFAULT: &str = "http://auth_broker:9600";

/// Env var that flips the invitation client off. v0 break-glass only.
pub const DISABLE_ENV: &str = "BOTWORK_AUTH_BROKER_INVITATIONS_DISABLE";

/// OTP placeholder returned by [`InvitationClient::disabled`].
pub const DISABLED_OTP: &str = "DISABLED";

/// Per-request timeout for the auth-broker round-trip.
///
/// Tenant creation is a rare admin-only path; 8s matches the other
/// client timeouts in this crate.
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);
const READINESS_TIMEOUT: Duration = Duration::from_secs(2);

/// Failure modes for invitation mint calls.
#[derive(Debug)]
pub enum InvitationClientError {
    /// The client is disabled (env override set or [`disabled()`] used).
    ///
    /// [`disabled()`]: InvitationClient::disabled
    Disabled,
    /// Transport failure, 5xx from auth-broker, or JSON parse failure.
    Unavailable(String),
}

impl std::fmt::Display for InvitationClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InvitationClientError::Disabled => {
                write!(f, "invitation client disabled (break-glass)")
            }
            InvitationClientError::Unavailable(msg) => {
                write!(f, "auth-broker unavailable: {msg}")
            }
        }
    }
}

impl std::error::Error for InvitationClientError {}

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct MintInvitationRequest {
    tenant_id: Uuid,
}

#[derive(Debug, Deserialize)]
struct MintInvitationResponse {
    otp: String,
    #[allow(dead_code)]
    expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
struct RenewInvitationRequest {
    tenant_id: Uuid,
}

#[derive(Debug, Deserialize)]
struct RenewInvitationResponse {
    otp: String,
    expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
struct RevokeInvitationsRequest {
    tenant_id: Uuid,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Lightweight HTTP client targeting the auth-broker invitation endpoint.
///
/// Cloneable; uses `reqwest::Client` internally which shares its connection
/// pool across clones. `AppState` holds one and clones it per request.
#[derive(Clone)]
pub struct InvitationClient {
    endpoint: String,
    disabled: bool,
    http: reqwest::Client,
}

impl InvitationClient {
    fn from_parts(endpoint: Option<String>, disabled: Option<String>) -> Self {
        let endpoint = endpoint.unwrap_or_else(|| ENDPOINT_DEFAULT.to_string());
        let disabled = match disabled {
            Some(v) => matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"),
            None => false,
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

    /// Build a client from environment.
    ///
    /// Reads `BOTWORK_AUTH_BROKER_ENDPOINT` (default
    /// `http://auth_broker:9600`) and
    /// `BOTWORK_AUTH_BROKER_INVITATIONS_DISABLE` (truthy to disable).
    pub fn from_env() -> Self {
        Self::from_parts(
            std::env::var(ENDPOINT_ENV).ok(),
            std::env::var(DISABLE_ENV).ok(),
        )
    }

    /// Construct a client pointed at the given endpoint.
    /// Tests use this to inject a wiremock/tower URL.
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
    ///
    /// Returns `Ok(DISABLED_OTP)` from [`Self::mint_invitation`] so
    /// handlers keep working in the absence of a live auth-broker.
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

    /// `true` if the client is disabled.
    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// Side-effect-free readiness probe.
    ///
    /// Uses a method-mismatch request against `/internal/invitations`
    /// so no invitation is ever minted.
    pub async fn ready(&self) -> Result<(), String> {
        if self.disabled {
            return Ok(());
        }
        let url = format!("{}/internal/invitations", self.endpoint);
        let resp = tokio::time::timeout(READINESS_TIMEOUT, self.http.get(&url).send())
            .await
            .map_err(|_| format!("timeout contacting {url}"))?
            .map_err(|err| format!("transport error contacting {url}: {err}"))?;
        let status = resp.status();
        if status.is_success() || status == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            Ok(())
        } else {
            Err(format!("GET {url} returned {status}"))
        }
    }

    /// Mint an invitation OTP for `tenant_id`.
    ///
    /// Calls `POST {endpoint}/internal/invitations`, returns the
    /// plaintext OTP on success.
    ///
    /// When disabled, returns the fixed placeholder [`DISABLED_OTP`]
    /// without making a network call.
    pub async fn mint_invitation(&self, tenant_id: Uuid) -> Result<String, InvitationClientError> {
        if self.disabled {
            info!("{PREFIX} invitation_client: disabled — returning placeholder OTP for tenant_id={tenant_id}");
            return Ok(DISABLED_OTP.to_string());
        }

        let url = format!("{}/internal/invitations", self.endpoint);
        let resp = self
            .http
            .post(&url)
            .json(&MintInvitationRequest { tenant_id })
            .send()
            .await
            .map_err(|e| {
                warn!("{PREFIX} invitation_client: transport error: {e}");
                InvitationClientError::Unavailable(e.to_string())
            })?;

        let status = resp.status();
        if status.is_success() {
            let body: MintInvitationResponse = resp.json().await.map_err(|e| {
                warn!("{PREFIX} invitation_client: failed to parse response: {e}");
                InvitationClientError::Unavailable(format!("parse error: {e}"))
            })?;
            Ok(body.otp)
        } else {
            let text = resp.text().await.unwrap_or_default();
            warn!("{PREFIX} invitation_client: auth-broker returned {status}: {text}");
            Err(InvitationClientError::Unavailable(format!(
                "auth-broker returned {status}: {text}"
            )))
        }
    }

    /// Renew an invitation OTP for `tenant_id`.
    ///
    /// Calls `POST {endpoint}/internal/invitations/renew`. Atomically revokes
    /// any outstanding invitation and mints a fresh one. Returns the new
    /// plaintext OTP and its expiry time on success.
    ///
    /// When disabled, returns the fixed placeholder [`DISABLED_OTP`]
    /// without making a network call.
    pub async fn renew_invitation(
        &self,
        tenant_id: Uuid,
    ) -> Result<(String, chrono::DateTime<chrono::Utc>), InvitationClientError> {
        if self.disabled {
            info!("{PREFIX} invitation_client: disabled — returning placeholder OTP for renew tenant_id={tenant_id}");
            let placeholder_expires = chrono::Utc::now() + chrono::Duration::days(7);
            return Ok((DISABLED_OTP.to_string(), placeholder_expires));
        }

        let url = format!("{}/internal/invitations/renew", self.endpoint);
        let resp = self
            .http
            .post(&url)
            .json(&RenewInvitationRequest { tenant_id })
            .send()
            .await
            .map_err(|e| {
                warn!("{PREFIX} invitation_client: transport error on renew: {e}");
                InvitationClientError::Unavailable(e.to_string())
            })?;

        let status = resp.status();
        if status.is_success() {
            let body: RenewInvitationResponse = resp.json().await.map_err(|e| {
                warn!("{PREFIX} invitation_client: failed to parse renew response: {e}");
                InvitationClientError::Unavailable(format!("parse error: {e}"))
            })?;
            Ok((body.otp, body.expires_at))
        } else {
            let text = resp.text().await.unwrap_or_default();
            warn!("{PREFIX} invitation_client: auth-broker returned {status} on renew: {text}");
            Err(InvitationClientError::Unavailable(format!(
                "auth-broker returned {status}: {text}"
            )))
        }
    }

    /// Revoke all outstanding invitations for `tenant_id`.
    ///
    /// Calls `POST {endpoint}/internal/invitations/revoke`. Idempotent:
    /// succeeds even if there are no active invitations to revoke.
    ///
    /// When disabled, returns `Ok(())` without making a network call
    /// (no active invitations exist in the disabled/break-glass state).
    pub async fn revoke_invitations(&self, tenant_id: Uuid) -> Result<(), InvitationClientError> {
        if self.disabled {
            info!(
                "{PREFIX} invitation_client: disabled — skipping revoke for tenant_id={tenant_id}"
            );
            return Ok(());
        }

        let url = format!("{}/internal/invitations/revoke", self.endpoint);
        let resp = self
            .http
            .post(&url)
            .json(&RevokeInvitationsRequest { tenant_id })
            .send()
            .await
            .map_err(|e| {
                warn!("{PREFIX} invitation_client: transport error on revoke: {e}");
                InvitationClientError::Unavailable(e.to_string())
            })?;

        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let text = resp.text().await.unwrap_or_default();
            warn!("{PREFIX} invitation_client: auth-broker returned {status} on revoke: {text}");
            Err(InvitationClientError::Unavailable(format!(
                "auth-broker returned {status}: {text}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn ready_disabled_is_ready() {
        let client = InvitationClient::disabled();
        client.ready().await.expect("disabled is ready");
    }

    #[tokio::test]
    async fn ready_unreachable_is_unready() {
        let client = InvitationClient::with_endpoint("http://127.0.0.1:1");
        let err = client.ready().await.expect_err("unready");
        assert!(err.contains("transport error") || err.contains("timeout"));
    }

    #[tokio::test]
    async fn ready_reachable_method_mismatch_is_ready() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/internal/invitations"))
            .respond_with(ResponseTemplate::new(405))
            .mount(&server)
            .await;
        let client = InvitationClient::with_endpoint(server.uri());
        client.ready().await.expect("ready");
    }

    #[tokio::test]
    async fn ready_probe_is_side_effect_free_no_mint_call() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/internal/invitations"))
            .respond_with(ResponseTemplate::new(405))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/internal/invitations"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "otp": "otp",
                "expires_at": chrono::Utc::now(),
            })))
            .expect(0)
            .mount(&server)
            .await;

        let client = InvitationClient::with_endpoint(server.uri());
        client.ready().await.expect("ready");
    }
}
