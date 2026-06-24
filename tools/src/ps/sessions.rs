//! HTTP client for session-broker's `GET /sessions` admin endpoint.
//!
//! The endpoint returns an operator-visible view of the live
//! in-memory `transport_sessions` map, container-name-keyed,
//! shaped like:
//!
//! ```json
//! {
//!   "sessions": {
//!     "mcp_session_<token>": {
//!       "container":    "mcp_session_<token>",
//!       "container_ip": "172.20.0.5",
//!       "tenant":       "phlax",
//!       "workspace":    "mcp",
//!       "plugin":       "fetch",
//!       "agent_id":     "<goose-agent-session-id>" | null
//!     },
//!     ...
//!   }
//! }
//! ```
//!
//! See `session-broker/src/admin.rs::get_sessions` for the source.
//! This deserialiser pins the field names the broker emits; a wire
//! drift would surface here as a `Decode` error rather than a
//! silent empty table.
//!
//! Trust posture: credless, same as every broker-to-broker call.
//! Network membership of `botwork-internal` is the boundary — the
//! broker bound on `0.0.0.0:9002` doesn't authenticate callers, and
//! the deployment never publishes that port to the host.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::blocking::Client;
use serde::Deserialize;
use thiserror::Error;

/// 5s upper bound on the round trip. The broker serves this from an
/// in-memory map under a `Mutex` it holds for microseconds; 5s is
/// generous and accommodates a slow loopback during boot (where
/// `botwork-tools ps` is sometimes invoked from a shell-script
/// readiness probe).
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Deserialize)]
pub struct SessionView {
    /// Tenant the session belongs to (e.g. "phlax").
    pub tenant: String,
    /// Workspace within the tenant (default "mcp" pre-RFE-#101 PR2).
    pub workspace: String,
    /// Plugin name the broker spawned for this session. The
    /// pre-cutover registry surfaced an `image` field here; with the
    /// broker reading from postgres post-cutover, the user-facing
    /// identifier for "which thing is this" is the plugin name —
    /// stable across version bumps and operator-meaningful.
    /// Rendered in the IMAGE column of `botwork-tools ps`.
    pub plugin: String,
    /// Goose agent-session-id; `None` until the agent's first
    /// non-init JSON-RPC call surfaces `_meta.agent-session-id`
    /// and session-broker fires `record_bind_agent`. Rendered as
    /// `(unbound)` in the AGENT column when null.
    pub agent_id: Option<String>,
}

/// Wire envelope; matches `SessionsView` on the broker side.
#[derive(Debug, Deserialize)]
struct SessionsEnvelope {
    sessions: BTreeMap<String, SessionView>,
}

/// Fetch and deserialise the broker's `/sessions` snapshot.
///
/// `url` is the full URL of the endpoint (the caller appends or
/// substitutes a path if needed; the default `http://session_broker:9002/sessions`
/// is provided in `ps/mod.rs`). The returned map is keyed by
/// container name, same as the wire shape.
pub fn fetch(url: &str) -> Result<BTreeMap<String, SessionView>, SessionsError> {
    // Construct the client lazily here rather than caching one in a
    // OnceLock: this tool runs as a single-shot process, so the
    // per-invocation HTTP-client cost is negligible and the
    // explicit construction keeps the failure surface tight.
    let client = Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|err| SessionsError::BuildClient(err.to_string()))?;

    let resp = client
        .get(url)
        .header("accept", "application/json")
        .send()
        .map_err(|err| {
            // reqwest folds connection refused + DNS + timeout into
            // the same enum variant; we surface "is the broker
            // running?" as a single Transport error and let the
            // caller decide how to phrase it.
            SessionsError::Transport(format!("GET {url}: {err}"))
        })?;

    if !resp.status().is_success() {
        return Err(SessionsError::BadStatus {
            url: url.to_string(),
            status: resp.status().as_u16(),
        });
    }

    let envelope: SessionsEnvelope = resp
        .json()
        .map_err(|err| SessionsError::Decode(format!("GET {url}: {err}")))?;

    Ok(envelope.sessions)
}

#[derive(Debug, Error)]
pub enum SessionsError {
    #[error("failed to build HTTP client: {0}")]
    BuildClient(String),

    /// Couldn't reach the broker at all (DNS, refused, timeout).
    /// In production the most common cause is "running on the host
    /// outside the docker network" — the default
    /// `http://session_broker:9002/sessions` is reachable only from
    /// inside `botwork-internal`. Operator should run from a
    /// sibling container OR set BOTWORK_TOOLS_SESSIONS_URL to a
    /// port-forwarded address.
    #[error("could not reach session-broker: {0}\n\nHint: this tool calls session-broker's admin endpoint over HTTP.\nDefault URL targets the docker alias `session_broker` on the\n`botwork-internal` network. Run from a sibling container, or set\nBOTWORK_TOOLS_SESSIONS_URL to a reachable URL (e.g. via port-forward).")]
    Transport(String),

    /// Got a response but the status was not 2xx. The broker's
    /// `GET /sessions` is unconditional success in the current
    /// implementation; non-2xx means the broker is unhealthy or
    /// something else is bound to `:9002`.
    #[error("session-broker returned HTTP {status} for {url}")]
    BadStatus { url: String, status: u16 },

    /// 2xx body did not match the expected `{"sessions": {...}}`
    /// envelope. Always a schema-drift bug between this tool and
    /// the broker.
    #[error("could not decode session-broker response: {0}")]
    Decode(String),
}
