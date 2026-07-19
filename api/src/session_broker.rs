//! HTTP client for signalling session-broker to evict tenant sessions.
//!
//! ## Why this exists
//!
//! When a secret is mutated (created, overwritten, or deleted) via the api's
//! tenant-scoped write surface, any already-running plugin containers for
//! that tenant are holding stale credentials — they were injected once at
//! spawn time and never refreshed.  This client sends a one-shot POST to
//! session-broker's admin interface so it can tombstone those sessions
//! synchronously and schedule async container teardown.
//!
//! The MCP Streamable HTTP spec requires a client that receives a 404 for
//! its `Mcp-Session-Id` to stop using that id and re-initialize.  The
//! re-initialize is a session-less POST that re-enters the spawn path and
//! re-fetches secrets, picking up the updated credentials.
//!
//! ## Failure semantics
//!
//! Eviction failure is **non-fatal for the secret-write request**.  If
//! session-broker is unreachable or returns an error, api logs a warning
//! and returns success for the secret write.  The next time the container's
//! liveness is checked (or the next request fails a liveness probe) the
//! stale session will be reaped through the normal liveness path.  This
//! is a best-effort eviction; the secret itself is always stored before
//! the signal is sent.
//!
//! ## Configurability + escape hatch
//!
//! Endpoint comes from `BOTWORK_SESSION_BROKER_EVICT_ENDPOINT`
//! (default `http://session_broker:9002`, the admin port session-broker
//! binds by default). Setting `BOTWORK_API_DISABLE_SESSION_BROKER_EVICT=1`
//! suppresses all eviction calls (break-glass / test override).

use std::time::Duration;

use tracing::{debug, warn};

use crate::handler::PREFIX;

/// Env var holding the session-broker admin HTTP endpoint.
pub const ENDPOINT_ENV: &str = "BOTWORK_SESSION_BROKER_EVICT_ENDPOINT";

/// Default endpoint: the admin port session-broker binds in production.
pub const ENDPOINT_DEFAULT: &str = "http://session_broker:9002";

/// Env var that disables eviction calls. Break-glass / test override.
pub const DISABLE_ENV: &str = "BOTWORK_API_DISABLE_SESSION_BROKER_EVICT";

/// Per-request timeout for the session-broker eviction call.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Lightweight HTTP client that signals session-broker to evict all live
/// sessions for a given tenant.
///
/// Cloneable; uses `reqwest::Client` internally (shared connection pool).
/// `AppState` holds one and clones it per request.
#[derive(Clone)]
pub struct SessionBrokerClient {
    endpoint: String,
    disabled: bool,
    http: reqwest::Client,
}

impl SessionBrokerClient {
    /// Build a client from explicit values — the pure core used by
    /// [`Self::from_env`] and by tests that inject values directly.
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

    /// Build a client from environment variables.
    pub fn from_env() -> Self {
        Self::from_parts(
            std::env::var(ENDPOINT_ENV).ok(),
            std::env::var(DISABLE_ENV).ok(),
        )
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

    /// Disabled client — eviction calls are suppressed.
    /// Used in tests that don't exercise the eviction path.
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

    /// Signal session-broker to evict all live sessions for `tenant`.
    ///
    /// Sends `POST {endpoint}/evict-tenant/{tenant}`.
    ///
    /// Returns `Ok(())` on success (any 2xx from session-broker) and on
    /// `Disabled`.  Returns `Err(EvictError)` on transport failure or
    /// non-2xx from session-broker.  The caller decides whether to log
    /// and ignore or propagate.
    pub async fn evict_tenant_sessions(&self, tenant: &str) -> Result<(), EvictError> {
        if self.disabled {
            return Ok(());
        }
        let url = format!("{}/evict-tenant/{tenant}", self.endpoint);
        debug!("{PREFIX} session-broker evict: POST {url}");
        let resp = self
            .http
            .post(&url)
            .header("Content-Length", "0")
            .send()
            .await
            .map_err(|err| EvictError(format!("POST {url}: {err}")))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(EvictError(format!("POST {url} returned {status}: {body}")))
        }
    }
}

/// Error returned when the eviction signal fails.
#[derive(Debug)]
pub struct EvictError(pub String);

impl std::fmt::Display for EvictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session-broker evict failed: {}", self.0)
    }
}

impl std::error::Error for EvictError {}

/// Fire-and-forget helper used by the secret-write handlers.
///
/// Calls `client.evict_tenant_sessions(tenant)` and logs a warning on
/// failure.  The secret write has already succeeded at this point; eviction
/// failure is non-fatal.
pub async fn signal_evict(client: &SessionBrokerClient, tenant: &str) {
    match client.evict_tenant_sessions(tenant).await {
        Ok(()) => {
            debug!("{PREFIX} session-broker evict: tenant={tenant:?} signaled");
        }
        Err(err) => {
            warn!("{PREFIX} session-broker evict failed for tenant={tenant:?}: {err} (live sessions for this tenant may serve stale credentials until their next liveness check)");
        }
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn from_env_honors_default_and_disable_flag() {
        let default_client = SessionBrokerClient::from_parts(None, None);
        assert_eq!(default_client.endpoint, ENDPOINT_DEFAULT);
        assert!(!default_client.is_disabled());

        let disabled_client = SessionBrokerClient::from_parts(None, Some("yes".to_string()));
        assert!(disabled_client.is_disabled());
    }

    #[test]
    fn from_env_honors_endpoint_override_and_true_spellings() {
        let client = SessionBrokerClient::from_parts(
            Some("http://broker.example:9002".to_string()),
            Some("TRUE".to_string()),
        );
        assert_eq!(client.endpoint, "http://broker.example:9002");
        assert!(client.is_disabled());

        let client = SessionBrokerClient::from_parts(None, Some("true".to_string()));
        assert!(client.is_disabled());
    }

    #[tokio::test]
    async fn disabled_client_treats_evict_as_noop() {
        let client = SessionBrokerClient::disabled();
        client
            .evict_tenant_sessions("phlax")
            .await
            .expect("disabled should be noop");
    }

    #[tokio::test]
    async fn evict_maps_http_success_and_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/evict-tenant/phlax"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let client = SessionBrokerClient::with_endpoint(server.uri());
        client
            .evict_tenant_sessions("phlax")
            .await
            .expect("evict ok");

        let error_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/evict-tenant/phlax"))
            .respond_with(ResponseTemplate::new(503).set_body_string("down"))
            .mount(&error_server)
            .await;
        let client = SessionBrokerClient::with_endpoint(error_server.uri());
        let err = client
            .evict_tenant_sessions("phlax")
            .await
            .expect_err("non-success status");
        assert!(err.0.contains("503"));
    }

    #[tokio::test]
    async fn signal_evict_handles_failure_non_fatally() {
        let client = SessionBrokerClient::with_endpoint("http://127.0.0.1:1");
        signal_evict(&client, "phlax").await;
    }

    #[test]
    fn evict_error_display_is_prefixed_for_logs() {
        let err = EvictError("POST http://x returned 503: down".into());
        assert!(format!("{err}").starts_with("session-broker evict failed: "));
    }
}
