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
}
