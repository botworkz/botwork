//! HTTP client for the live-state ack gate against `botwork-control-plane`.
//!
//! ## Why this exists
//!
//! `workspace_plugin` mutations (POST / DELETE binding rows)
//! potentially affect already-live plugin sessions. session-broker
//! already enforces the invariant that no unpoliced spawn ever
//! serves a request: it `POST`s to control-plane on spawn, waits
//! for the xDS ack (#92), and fails the spawn on a non-2xx. The
//! admin-side mirror is in this module: `DELETE` of a binding row
//! that affects live sessions has to wait for control-plane to
//! confirm the live policy change, OR roll back the DB write.
//!
//! ## Wire contract
//!
//! Today control-plane's `POST /sessions` and `DELETE /sessions/{id}`
//! are keyed by `session_id`. api isn't operating on sessions;
//! it's operating on bindings. The translation layer:
//!
//! * api looks up the live `mcp_session_<token>` sessions
//!   associated with the binding being mutated (via
//!   `GET /sessions` on control-plane, filtered by `tenant`,
//!   `workspace`, `plugin` triple).
//! * For each live session it `DELETE`s the session record, waiting
//!   for control-plane's existing ack gate.
//! * If ANY DELETE returns a non-2xx (other than 404, which is
//!   benign — the session ended between list and delete), the DB
//!   write is rolled back and api returns 503 `unavailable`.
//!
//! This is the "Option A" posture decided in the design conversation:
//! the binding write is a single transaction; the control-plane
//! coordination happens inside that transaction; transport / 5xx /
//! ack-timeout against control-plane rolls back.
//!
//! ## Configurability + escape hatch
//!
//! Endpoint comes from `BOTWORK_CONTROL_PLANE_ENDPOINT`
//! (default `http://control_plane:9300`, mirroring session-broker's
//! env name). Setting `BOTWORK_API_DISABLE_LIVE_GATE=1`
//! short-circuits the coupling: writes commit without consulting
//! control-plane. v0 break-glass posture only; production sets this
//! ONLY when control-plane is unrecoverable and the operator
//! explicitly accepts the desync. (Same shape as control-plane's
//! own `BOTWORK_CONTROL_PLANE_DISABLE_ACK_WAIT`.)

use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use tracing::{debug, warn};

const PREFIX: &str = "[api]";

/// Env var holding the control-plane HTTP endpoint api
/// targets. Same name session-broker uses.
pub const ENDPOINT_ENV: &str = "BOTWORK_CONTROL_PLANE_ENDPOINT";

/// Default endpoint: the in-network alias control-plane registers
/// on `botwork-internal` plus its HTTP port (9300).
pub const ENDPOINT_DEFAULT: &str = "http://control_plane:9300";

/// Env var that flips the gate off. v0 break-glass only.
pub const DISABLE_ENV: &str = "BOTWORK_API_DISABLE_LIVE_GATE";

/// Per-request timeout for the control-plane round-trip.
///
/// 8s gives us plenty of headroom: control-plane's own synchronous
/// ack-wait budget is 5s (DEFAULT_ACK_WAIT in src/handler.rs), and
/// the extra 3s covers TCP setup + JSON serialisation on slow CI
/// hosts. Returning ServiceUnavailable after 8s of waiting is the
/// right operator-facing behaviour — a control-plane that's >8s
/// behind on writes is already broken.
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);

/// Failure modes for the live-state gate. Each variant maps 1:1 onto
/// an [`ApiError`] in the handler layer.
///
/// [`ApiError`]: crate::handler::ApiError
#[derive(Debug)]
pub enum GateError {
    /// The gate is disabled (env override set). Not strictly an
    /// error — the handler treats this as "skip the gate". Carried
    /// in the same enum to keep the call-site uniform.
    Disabled,
    /// Control-plane unreachable or returned non-2xx. The DB write
    /// MUST roll back. The handler emits 503 `unavailable`.
    Unavailable(String),
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GateError::Disabled => write!(f, "live-state gate disabled (break-glass)"),
            GateError::Unavailable(msg) => write!(f, "control-plane unavailable: {msg}"),
        }
    }
}

impl std::error::Error for GateError {}

/// Lightweight HTTP client targeting control-plane.
///
/// Cloneable; uses `reqwest::Client` internally which shares its
/// connection pool across clones. AppState holds one and clones it
/// per request.
#[derive(Clone)]
pub struct ControlPlaneClient {
    endpoint: String,
    disabled: bool,
    http: reqwest::Client,
}

impl ControlPlaneClient {
    /// Build a client from environment.
    ///
    /// Reads `BOTWORK_CONTROL_PLANE_ENDPOINT` (default
    /// `http://control_plane:9300`) and
    /// `BOTWORK_API_DISABLE_LIVE_GATE` (truthy to disable).
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

    /// Construct a client pointed at the given endpoint. Tests use
    /// this to inject a `mockito` URL.
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

    /// Disable the gate (test ergonomics).
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

    /// `true` if the gate is disabled. Handlers branch on this so
    /// the disabled path is unambiguous in the journal.
    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// List live sessions matching the `(tenant, workspace, plugin)`
    /// triple. Returns an empty vec when control-plane is reachable
    /// but the triple has no live sessions. Returns
    /// `Err(GateError::Unavailable)` when transport / 5xx.
    ///
    /// The match is client-side: control-plane's `GET /sessions`
    /// returns the full set and api filters. The session set
    /// is bounded by the number of live MCP transports in the
    /// deployment (~tens), so client-side filtering is fine in v0.
    pub async fn list_sessions_for(
        &self,
        tenant: &str,
        workspace: &str,
        plugin: &str,
    ) -> Result<Vec<LiveSession>, GateError> {
        if self.disabled {
            return Err(GateError::Disabled);
        }
        let url = format!("{}/sessions", self.endpoint);
        debug!("{PREFIX} live-gate list: GET {url} for ({tenant}, {workspace}, {plugin})");
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|err| GateError::Unavailable(format!("GET /sessions: {err}")))?;
        if !resp.status().is_success() {
            return Err(GateError::Unavailable(format!(
                "GET /sessions returned {}",
                resp.status()
            )));
        }
        let body: ListSessionsBody = resp
            .json()
            .await
            .map_err(|err| GateError::Unavailable(format!("GET /sessions: parse: {err}")))?;
        Ok(body
            .sessions
            .into_iter()
            .filter(|s| s.tenant == tenant && s.workspace == workspace && s.plugin == plugin)
            .collect())
    }

    /// Delete a single session record on control-plane, waiting for
    /// the existing ack gate (#92).
    ///
    /// 404 is treated as success — the session ended between our
    /// list call and this delete, which means the live state already
    /// matches what we're trying to achieve.
    pub async fn delete_session(&self, session_id: &str) -> Result<(), GateError> {
        if self.disabled {
            return Err(GateError::Disabled);
        }
        let url = format!("{}/sessions/{session_id}", self.endpoint);
        debug!("{PREFIX} live-gate delete: DELETE {url}");
        let resp = self
            .http
            .delete(&url)
            .send()
            .await
            .map_err(|err| GateError::Unavailable(format!("DELETE {url}: {err}")))?;
        match resp.status() {
            s if s.is_success() => Ok(()),
            StatusCode::NOT_FOUND => {
                debug!(
                    "{PREFIX} live-gate delete: 404 for session_id={session_id} (already gone — benign)"
                );
                Ok(())
            }
            other => {
                let body = resp.text().await.unwrap_or_default();
                warn!(
                    "{PREFIX} live-gate delete: control-plane returned {other} for session_id={session_id}: {body}"
                );
                Err(GateError::Unavailable(format!(
                    "DELETE /sessions/{session_id} returned {other}"
                )))
            }
        }
    }
}

/// Mirror of `control_plane::sessions::SessionRecord`. Kept hand-
/// rolled here (rather than depending on control-plane as a library)
/// so api doesn't pick up control-plane's transitive deps
/// (tonic, envoy-proto, the xds server-side runtime). Only the
/// fields api needs are deserialised; control-plane is free to
/// add more without breaking us.
#[derive(Debug, Clone, Deserialize)]
pub struct LiveSession {
    pub session_id: String,
    pub tenant: String,
    pub workspace: String,
    pub plugin: String,
}

#[derive(Debug, Deserialize)]
struct ListSessionsBody {
    sessions: Vec<LiveSession>,
}

/// Helper that combines list + delete-each for a `(tenant, workspace,
/// plugin)` triple. Used by `workspace_plugin` DELETE handlers; pulled
/// out so the read path stays a one-liner.
///
/// Returns the count of sessions terminated. Bubbles `GateError::Disabled`
/// to the caller (which decides whether to log + proceed); bubbles
/// `GateError::Unavailable` and the caller rolls back.
pub async fn terminate_live_sessions(
    client: &ControlPlaneClient,
    tenant: &str,
    workspace: &str,
    plugin: &str,
) -> Result<usize, GateError> {
    let live = client.list_sessions_for(tenant, workspace, plugin).await?;
    let mut count = 0;
    for session in &live {
        client.delete_session(&session.session_id).await?;
        count += 1;
    }
    Ok(count)
}

/// Helper used by audit logging. Builds a one-line summary of the
/// live-state interaction for the journal.
pub fn outcome_summary(result: &Result<usize, GateError>) -> String {
    match result {
        Ok(n) => format!("live_sessions_terminated={n}"),
        Err(GateError::Disabled) => "live_gate=disabled".to_string(),
        Err(GateError::Unavailable(msg)) => format!("live_gate=unavailable detail={msg:?}"),
    }
}

// (control-plane's DELETE /sessions/<id> is path-only; no body needed.
// Earlier draft carried a typed body struct; dropped because the
// DELETE handler ignores the body and we don't want to ship a wire
// shape we can't enforce on the server side.)

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
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

    #[test]
    fn from_env_honors_default_and_disable_flag() {
        let _guard = EnvGuard::capture();
        std::env::remove_var(ENDPOINT_ENV);
        std::env::remove_var(DISABLE_ENV);

        let default_client = ControlPlaneClient::from_env();
        assert!(!default_client.is_disabled());
        assert_eq!(default_client.endpoint, ENDPOINT_DEFAULT);

        std::env::set_var(DISABLE_ENV, "true");
        let disabled_client = ControlPlaneClient::from_env();
        assert!(disabled_client.is_disabled());
        assert_eq!(disabled_client.endpoint, ENDPOINT_DEFAULT);
    }

    #[tokio::test]
    async fn disabled_client_short_circuits() {
        let client = ControlPlaneClient::disabled();
        let err = client
            .list_sessions_for("phlax", "mcp", "mcp-fetch")
            .await
            .expect_err("disabled");
        assert!(matches!(err, GateError::Disabled));
    }

    #[tokio::test]
    async fn list_and_delete_map_success_and_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sessions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sessions": [
                    { "session_id": "a", "tenant": "phlax", "workspace": "mcp", "plugin": "mcp-fetch" },
                    { "session_id": "b", "tenant": "other", "workspace": "mcp", "plugin": "mcp-fetch" }
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/sessions/a"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let client = ControlPlaneClient::with_endpoint(server.uri());
        let sessions = client
            .list_sessions_for("phlax", "mcp", "mcp-fetch")
            .await
            .expect("sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "a");
        client.delete_session("a").await.expect("delete ok");
    }

    #[tokio::test]
    async fn delete_404_is_treated_as_success() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/sessions/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = ControlPlaneClient::with_endpoint(server.uri());
        client
            .delete_session("missing")
            .await
            .expect("404 should be benign");
    }

    #[tokio::test]
    async fn unreachable_endpoint_maps_to_unavailable() {
        let client = ControlPlaneClient::with_endpoint("http://127.0.0.1:1");
        let err = client
            .list_sessions_for("phlax", "mcp", "mcp-fetch")
            .await
            .expect_err("unavailable");
        assert!(matches!(err, GateError::Unavailable(_)));
    }

    #[test]
    fn outcome_summary_formats_all_variants() {
        assert_eq!(outcome_summary(&Ok(2)), "live_sessions_terminated=2");
        assert_eq!(
            outcome_summary(&Err(GateError::Disabled)),
            "live_gate=disabled"
        );
        assert!(
            outcome_summary(&Err(GateError::Unavailable("boom".to_string())))
                .contains("live_gate=unavailable")
        );
    }
}
