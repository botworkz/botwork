//! HTTP handlers for control-plane's intake/read surface.
//!
//! Wire contract (full spec in `README.md` once PR A lands):
//!
//! * `POST   /sessions`        body=`SessionRecord` → 201 ack | 4xx error
//! * `DELETE /sessions/<id>`                        → 200 ack | 404
//! * `GET    /sessions/<id>`                        → 200 record | 404
//! * `GET    /sessions`                             → 200 `{ "sessions": […] }`
//!
//! Error envelope (matches config-broker's convention so callers can share
//! retry/logging code):
//!
//! ```json
//! { "error": "<machine code>", "message": "<human detail>" }
//! ```
//!
//! The single load-bearing design decision in this file: `POST /sessions`
//! is the hard gate the rest of the control-plane design is built on.
//! session-broker treats a non-2xx here as a hard fail for the session
//! it was about to hand off to envoy. So validation surface here matters
//! more than for typical CRUD: we want **clear, machine-distinguishable**
//! error codes for the things session-broker can correct, and we never
//! want to 200 a record we cannot actually serve.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::sessions::{AckWaitError, SessionRecord, SessionStore, StoreError};

/// How long `POST /sessions` / `DELETE /sessions/<id>` will block
/// waiting for envoy to ACK the resulting LDS push before returning
/// 503.
///
/// 5s is generous against the envoy hot path: ADS pushes typically
/// ACK in <100ms, and even a freshly-bootstrapping envoy completes
/// its first listener load in <1s in our setup. 5s gives us plenty
/// of headroom for the "envoy is briefly paused / mid-config-load /
/// has a slow protobuf decode" case without blocking spawns
/// indefinitely.
///
/// Configurable via `BOTWORK_CONTROL_PLANE_ACK_WAIT_MS` (e.g. shorter
/// in CI smoke tests for faster failure surfaces, longer if a future
/// deployment puts more between envoy and control-plane).
pub const DEFAULT_ACK_WAIT: Duration = Duration::from_secs(5);

/// Global escape hatch matching the
/// `BOTWORK_CONTROL_PLANE_DISABLE_RECOVERY` shape from #87. When set
/// truthy, mutation handlers do NOT wait for the xDS ACK before
/// returning 201/200; they revert to the pre-#92 behaviour of
/// "stored in our table, hopefully envoy catches up." Intended for
/// break-glass scenarios where envoy is unrecoverable and the
/// operator needs spawns to proceed regardless; explicitly not a
/// supported posture.
pub const ACK_DISABLED_ENV: &str = "BOTWORK_CONTROL_PLANE_DISABLE_ACK_WAIT";

const PREFIX: &str = "[control-plane]";

/// `session_id` shape: `mcp_session_<token>` where `<token>` is hex.
/// This matches what session-broker constructs in `ext_proc.rs`
/// (`mcp_session_<staging_token>`). We validate the shape rather than
/// the exact length/charset of the token: session-broker is the only
/// producer and may change the token width later, but the
/// `mcp_session_` prefix is the contract.
const SESSION_ID_RE: &str = r"^mcp_session_[a-z0-9]+$";

/// Plugin / tenant / namespace shape — same regex used in config-broker
/// and session-broker. Validated here so a malformed record never
/// pollutes the store.
const NAME_RE: &str = r"^[a-z][a-z0-9-]{0,30}$";

fn session_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(SESSION_ID_RE).expect("valid session id regex"))
}

fn name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(NAME_RE).expect("valid name regex"))
}

#[derive(Clone)]
pub struct AppState {
    pub sessions: Arc<SessionStore>,
    /// Per-request budget for waiting on the egress envoy to ACK a
    /// fresh Listener push after a successful mutation. Owned by
    /// AppState (rather than read from env on every request) so:
    ///   * tests can plug a short value without touching globals,
    ///   * the binary reads env exactly once at startup, in `main.rs`.
    pub ack_wait: Duration,
    /// `true` flips the synchronous ack gate off — mutations return
    /// success without waiting for envoy. Operator break-glass only.
    /// See `ACK_DISABLED_ENV`.
    pub ack_disabled: bool,
}

impl AppState {
    /// Construct an AppState with the default ack-wait budget and the
    /// gate enabled. Tests and `main.rs` use the struct-literal form
    /// directly when they need to override; this is the convenience.
    pub fn new(sessions: Arc<SessionStore>) -> Self {
        Self {
            sessions,
            ack_wait: DEFAULT_ACK_WAIT,
            ack_disabled: false,
        }
    }
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
}

fn error_response(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    let body = ErrorBody {
        error: code,
        message: message.into(),
    };
    (status, Json(body)).into_response()
}

/// Returned on a successful `POST /sessions`.
///
/// `session_id` is echoed back so session-broker can sanity-check the
/// ack is for the record it just sent (paranoid, cheap, and ergonomic
/// for the integration test). `status: "stored"` is the human-readable
/// confirmation; machines should branch on the HTTP status only.
#[derive(Debug, Serialize)]
struct AckBody<'a> {
    status: &'static str,
    session_id: &'a str,
}

#[derive(Debug, Serialize)]
struct ListBody {
    sessions: Vec<SessionRecord>,
}

#[derive(Debug, Deserialize)]
struct PostBody {
    session_id: Option<String>,
    container_ip: Option<String>,
    tenant: Option<String>,
    namespace: Option<String>,
    plugin: Option<String>,
    /// `null` is allowed and treated as "no policy / default-open" so a
    /// new plugin entry can be onboarded without a parallel
    /// config-broker change. v0 control-plane stores the value
    /// verbatim; no schema is enforced here.
    egress_policy: Option<serde_json::Value>,
}

async fn post_session(State(state): State<AppState>, body: Option<Json<PostBody>>) -> Response {
    let Some(Json(payload)) = body else {
        warn!("{PREFIX} post: invalid_request — missing or unparseable JSON body");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "request body must be a JSON object",
        );
    };

    // Validate inline rather than via `Result<SessionRecord, Response>`:
    // axum's `Response` is large (>128 bytes), and routing the failures
    // back through a `Result` trips `clippy::result_large_err`. Same
    // pattern config-broker uses in `handler::resolve` for the same
    // reason.
    let Some(session_id) = payload.session_id else {
        warn!("{PREFIX} post: invalid_request — missing 'session_id'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing required field 'session_id'",
        );
    };
    let Some(container_ip_raw) = payload.container_ip else {
        warn!("{PREFIX} post: invalid_request — missing 'container_ip'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing required field 'container_ip'",
        );
    };
    let Some(tenant) = payload.tenant else {
        warn!("{PREFIX} post: invalid_request — missing 'tenant'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing required field 'tenant'",
        );
    };
    let Some(namespace) = payload.namespace else {
        warn!("{PREFIX} post: invalid_request — missing 'namespace'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing required field 'namespace'",
        );
    };
    let Some(plugin) = payload.plugin else {
        warn!("{PREFIX} post: invalid_request — missing 'plugin'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing required field 'plugin'",
        );
    };

    if !session_id_re().is_match(&session_id) {
        warn!("{PREFIX} post: invalid_request — bad session_id '{session_id}'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("invalid session_id '{session_id}': must match {SESSION_ID_RE}"),
        );
    }
    if !name_re().is_match(&tenant) {
        warn!("{PREFIX} post: invalid_request — bad tenant '{tenant}'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("invalid tenant '{tenant}': must match {NAME_RE}"),
        );
    }
    if !name_re().is_match(&namespace) {
        warn!("{PREFIX} post: invalid_request — bad namespace '{namespace}'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("invalid namespace '{namespace}': must match {NAME_RE}"),
        );
    }
    if !name_re().is_match(&plugin) {
        warn!("{PREFIX} post: invalid_request — bad plugin '{plugin}'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("invalid plugin '{plugin}': must match {NAME_RE}"),
        );
    }

    let container_ip: Ipv4Addr = match container_ip_raw.parse() {
        Ok(ip) => ip,
        Err(_) => {
            warn!("{PREFIX} post: invalid_request — bad container_ip '{container_ip_raw}'");
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("invalid container_ip '{container_ip_raw}': must be IPv4 dotted-quad"),
            );
        }
    };

    let record = SessionRecord {
        session_id,
        container_ip,
        tenant,
        namespace,
        plugin,
        egress_policy: payload.egress_policy.unwrap_or(serde_json::Value::Null),
    };

    let session_id = record.session_id.clone();
    let plugin = record.plugin.clone();
    let container_ip = record.container_ip;

    match state.sessions.insert(record).await {
        Ok(()) => {
            // Capture the generation the insert just produced. xDS
            // ships this as `version_info`, envoy ACKs it back, and
            // `wait_for_ack` blocks on that ACK. See sessions.rs
            // module docs.
            let target_version = state.sessions.current_generation();
            if let Err(err) = wait_for_xds_ack(&state, target_version, "post", &session_id).await {
                // The store mutation succeeded but envoy hasn't
                // confirmed. Roll back so the store reflects what
                // envoy actually has, and 503 so session-broker
                // tears the spawned container down.
                rollback_after_ack_failure(&state, &session_id, "post").await;
                return err;
            }
            info!(
                "{PREFIX} post: ok session_id={session_id} plugin={plugin} ip={container_ip} version={target_version}"
            );
            (
                StatusCode::CREATED,
                Json(AckBody {
                    status: "stored",
                    session_id: &session_id,
                }),
            )
                .into_response()
        }
        Err(StoreError::AlreadyExists(_)) => {
            // session-broker shouldn't normally hit this — every spawn
            // gets a fresh token-derived id. If it does, surface it as
            // 409 so session-broker can either fail the spawn outright
            // or DELETE-then-POST as a recovery. Either way it isn't
            // silently merged.
            warn!("{PREFIX} post: conflict — session_id={session_id} already known");
            error_response(
                StatusCode::CONFLICT,
                "already_exists",
                format!("session_id '{session_id}' already exists in control-plane store"),
            )
        }
        Err(StoreError::NotFound(_)) => unreachable!("insert never returns NotFound"),
    }
}

async fn delete_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if !session_id_re().is_match(&id) {
        warn!("{PREFIX} delete: invalid_request — bad session_id '{id}'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("invalid session_id '{id}': must match {SESSION_ID_RE}"),
        );
    }

    match state.sessions.remove(&id).await {
        Ok(record) => {
            // Capture the post-delete generation and wait for envoy
            // to ACK the smaller Listener. Symmetry with POST: a
            // session whose policy is `egress: none` may have an
            // active SSRF-style request mid-flight that the deletion
            // is meant to terminate; returning 200 before the deny
            // is live in envoy would leak that request through.
            let target_version = state.sessions.current_generation();
            if let Err(err) = wait_for_xds_ack(&state, target_version, "delete", &id).await {
                // Re-insert the record so the store reflects what
                // envoy still has. This is best-effort: a concurrent
                // POST with the same id will lose its race here, but
                // the alternative (silently letting the store and
                // envoy diverge) is strictly worse.
                if let Err(reinsert_err) = state.sessions.insert(record.clone()).await {
                    warn!(
                        "{PREFIX} delete: ack failed AND rollback re-insert failed for session_id={id}: {reinsert_err}; store now diverged from envoy"
                    );
                }
                return err;
            }
            info!(
                "{PREFIX} delete: ok session_id={id} plugin={} ip={} version={target_version}",
                record.plugin, record.container_ip
            );
            (
                StatusCode::OK,
                Json(AckBody {
                    status: "removed",
                    session_id: &id,
                }),
            )
                .into_response()
        }
        Err(StoreError::NotFound(_)) => {
            warn!("{PREFIX} delete: not_found session_id={id}");
            error_response(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("session_id '{id}' not present in control-plane store"),
            )
        }
        Err(StoreError::AlreadyExists(_)) => unreachable!("remove never returns AlreadyExists"),
    }
}

async fn get_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if !session_id_re().is_match(&id) {
        warn!("{PREFIX} get: invalid_request — bad session_id '{id}'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("invalid session_id '{id}': must match {SESSION_ID_RE}"),
        );
    }

    match state.sessions.get(&id).await {
        Some(record) => (StatusCode::OK, Json(record)).into_response(),
        None => {
            warn!("{PREFIX} get: not_found session_id={id}");
            error_response(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("session_id '{id}' not present in control-plane store"),
            )
        }
    }
}

async fn list_sessions(State(state): State<AppState>) -> Response {
    let sessions = state.sessions.list().await;
    (StatusCode::OK, Json(ListBody { sessions })).into_response()
}

/// Block on the xDS ACK for the given target version. Returns either
/// `Ok(())` (envoy ACKed in time) or a fully-formed 503 `Response`
/// the handler can return verbatim. The body of the 503 names the
/// machine-readable reason (`no_xds_subscriber` vs `xds_ack_timeout`)
/// so session-broker / log consumers can distinguish a control-plane
/// not yet wired to envoy from one that is wired but slow.
async fn wait_for_xds_ack(
    state: &AppState,
    target_version: u64,
    op: &str,
    session_id: &str,
) -> Result<(), Response> {
    if state.ack_disabled {
        // Break-glass: gate is off, return success without waiting.
        // Logged at info so an operator who set this flag has a
        // greppable record of "we're running unsynced."
        info!(
            "{PREFIX} {op}: ack-gate disabled (BOTWORK_CONTROL_PLANE_DISABLE_ACK_WAIT=1) -- skipping wait for session_id={session_id}"
        );
        return Ok(());
    }
    match state
        .sessions
        .wait_for_ack(target_version, state.ack_wait)
        .await
    {
        Ok(()) => Ok(()),
        Err(AckWaitError::NoSubscriber) => {
            warn!(
                "{PREFIX} {op}: no xDS subscriber for session_id={session_id} (target_version={target_version})"
            );
            Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "no_xds_subscriber",
                format!(
                    "no egress envoy is currently subscribed to control-plane xDS; refusing to ack session_id '{session_id}'"
                ),
            ))
        }
        Err(AckWaitError::Timeout(version)) => {
            warn!(
                "{PREFIX} {op}: xDS ack timeout for session_id={session_id} version={version} after {:?}",
                state.ack_wait
            );
            Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "xds_ack_timeout",
                format!(
                    "egress envoy did not ACK xDS version {version} within {:?}; refusing to ack session_id '{session_id}'",
                    state.ack_wait
                ),
            ))
        }
    }
}

/// After a successful insert that subsequently failed its xDS ack,
/// roll the store back so it reflects what envoy actually has. Logs
/// (but does not surface) a removal failure — the original 503 has
/// already been built; we just want to make best-effort sure the
/// store doesn't carry a record envoy never agreed to.
async fn rollback_after_ack_failure(state: &AppState, session_id: &str, op: &str) {
    match state.sessions.remove(session_id).await {
        Ok(_) => {
            info!("{PREFIX} {op}: rolled back session_id={session_id} after xDS ack failure");
        }
        Err(err) => {
            warn!(
                "{PREFIX} {op}: rollback of session_id={session_id} after xDS ack failure ALSO failed: {err}; store now diverged from envoy"
            );
        }
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/sessions", post(post_session).get(list_sessions))
        .route(
            "/sessions/:session_id",
            get(get_session).delete(delete_session),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// State for the existing handler tests. Disables the ack gate so
    /// tests don't have to spin up an xDS stub: the focus of these
    /// tests is the HTTP shape (validation, status codes, error
    /// envelope), not the ack-blocking behaviour. The ack-blocking
    /// behaviour has its own dedicated tests further down.
    fn empty_state() -> AppState {
        AppState {
            sessions: Arc::new(SessionStore::new()),
            ack_wait: DEFAULT_ACK_WAIT,
            ack_disabled: true,
        }
    }

    /// State for the ack-gate tests. Short ack_wait so the timeout
    /// path completes in <200ms.
    fn ack_state_with(
        sessions: Arc<SessionStore>,
        ack_wait: Duration,
        ack_disabled: bool,
    ) -> AppState {
        AppState {
            sessions,
            ack_wait,
            ack_disabled,
        }
    }

    fn good_post(session_id: &str) -> Json<PostBody> {
        Json(PostBody {
            session_id: Some(session_id.to_string()),
            container_ip: Some("172.20.0.5".to_string()),
            tenant: Some("phlax".to_string()),
            namespace: Some("mcp".to_string()),
            plugin: Some("fetch".to_string()),
            egress_policy: Some(serde_json::json!({})),
        })
    }

    async fn body_status(response: Response) -> (StatusCode, serde_json::Value) {
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, 64 * 1024)
            .await
            .expect("read body");
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (parts.status, value)
    }

    #[tokio::test]
    async fn post_creates_and_returns_201_with_ack() {
        let state = empty_state();
        let response = post_session(State(state.clone()), Some(good_post("mcp_session_abc"))).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["status"], "stored");
        assert_eq!(body["session_id"], "mcp_session_abc");
        // Sanity: also retrievable.
        assert!(state.sessions.get("mcp_session_abc").await.is_some());
    }

    #[tokio::test]
    async fn post_duplicate_returns_409_already_exists() {
        let state = empty_state();
        post_session(State(state.clone()), Some(good_post("mcp_session_abc"))).await;
        let response = post_session(State(state), Some(good_post("mcp_session_abc"))).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "already_exists");
    }

    #[tokio::test]
    async fn post_missing_field_returns_400_invalid_request() {
        let state = empty_state();
        let bad = Json(PostBody {
            session_id: Some("mcp_session_abc".to_string()),
            container_ip: Some("172.20.0.5".to_string()),
            tenant: None,
            namespace: Some("mcp".to_string()),
            plugin: Some("fetch".to_string()),
            egress_policy: None,
        });
        let response = post_session(State(state), Some(bad)).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_request");
        assert!(
            body["message"].as_str().unwrap_or("").contains("'tenant'"),
            "should name missing field: {body}"
        );
    }

    #[tokio::test]
    async fn post_bad_session_id_returns_400() {
        let state = empty_state();
        let bad = Json(PostBody {
            session_id: Some("not-a-session-id".to_string()),
            container_ip: Some("172.20.0.5".to_string()),
            tenant: Some("phlax".to_string()),
            namespace: Some("mcp".to_string()),
            plugin: Some("fetch".to_string()),
            egress_policy: None,
        });
        let response = post_session(State(state), Some(bad)).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_request");
    }

    #[tokio::test]
    async fn post_bad_ip_returns_400() {
        let state = empty_state();
        let bad = Json(PostBody {
            session_id: Some("mcp_session_abc".to_string()),
            container_ip: Some("not-an-ip".to_string()),
            tenant: Some("phlax".to_string()),
            namespace: Some("mcp".to_string()),
            plugin: Some("fetch".to_string()),
            egress_policy: None,
        });
        let response = post_session(State(state), Some(bad)).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_request");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or("")
                .contains("container_ip"),
            "should mention container_ip: {body}"
        );
    }

    #[tokio::test]
    async fn post_missing_body_returns_400() {
        let state = empty_state();
        let response = post_session(State(state), None).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_request");
    }

    #[tokio::test]
    async fn post_null_egress_policy_is_treated_as_default() {
        let state = empty_state();
        let body = Json(PostBody {
            session_id: Some("mcp_session_abc".to_string()),
            container_ip: Some("172.20.0.5".to_string()),
            tenant: Some("phlax".to_string()),
            namespace: Some("mcp".to_string()),
            plugin: Some("fetch".to_string()),
            egress_policy: None,
        });
        let response = post_session(State(state.clone()), Some(body)).await;
        let (status, _) = body_status(response).await;
        assert_eq!(status, StatusCode::CREATED);
        let stored = state
            .sessions
            .get("mcp_session_abc")
            .await
            .expect("present");
        assert_eq!(stored.egress_policy, serde_json::Value::Null);
    }

    #[tokio::test]
    async fn delete_known_session_returns_200_with_ack() {
        let state = empty_state();
        post_session(State(state.clone()), Some(good_post("mcp_session_abc"))).await;
        let response =
            delete_session(State(state.clone()), Path("mcp_session_abc".to_string())).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "removed");
        assert!(state.sessions.get("mcp_session_abc").await.is_none());
    }

    #[tokio::test]
    async fn delete_unknown_session_returns_404() {
        let state = empty_state();
        let response = delete_session(State(state), Path("mcp_session_xyz".to_string())).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "not_found");
    }

    #[tokio::test]
    async fn delete_bad_session_id_returns_400() {
        let state = empty_state();
        let response = delete_session(State(state), Path("bad-id".to_string())).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_request");
    }

    #[tokio::test]
    async fn get_known_session_returns_record() {
        let state = empty_state();
        post_session(State(state.clone()), Some(good_post("mcp_session_abc"))).await;
        let response = get_session(State(state), Path("mcp_session_abc".to_string())).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["session_id"], "mcp_session_abc");
        assert_eq!(body["plugin"], "fetch");
        assert_eq!(body["container_ip"], "172.20.0.5");
    }

    #[tokio::test]
    async fn get_unknown_session_returns_404() {
        let state = empty_state();
        let response = get_session(State(state), Path("mcp_session_xyz".to_string())).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "not_found");
    }

    #[tokio::test]
    async fn list_returns_sorted_records() {
        let state = empty_state();
        for id in ["mcp_session_b", "mcp_session_a", "mcp_session_c"] {
            let body = Json(PostBody {
                session_id: Some(id.to_string()),
                container_ip: Some("172.20.0.5".to_string()),
                tenant: Some("phlax".to_string()),
                namespace: Some("mcp".to_string()),
                plugin: Some("fetch".to_string()),
                egress_policy: None,
            });
            post_session(State(state.clone()), Some(body)).await;
        }
        let response = list_sessions(State(state)).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::OK);
        let ids: Vec<&str> = body["sessions"]
            .as_array()
            .expect("sessions array")
            .iter()
            .map(|s| s["session_id"].as_str().expect("session_id"))
            .collect();
        assert_eq!(ids, vec!["mcp_session_a", "mcp_session_b", "mcp_session_c"]);
    }

    // ── ack-gate tests ────────────────────────────────────────────────
    // All the above tests run with `ack_disabled: true` to isolate the
    // HTTP wire shape from the ack-blocking behaviour. These tests
    // explicitly turn the gate on (and arrange the SessionStore's xDS
    // subscriber / ack channel by hand) to exercise the three paths:
    //   1. no subscriber → 503 no_xds_subscriber + store rolled back
    //   2. subscriber but no ack in time → 503 xds_ack_timeout + rollback
    //   3. subscriber + ack → 201
    //
    // We don't spin up the full tonic AdsServer here -- those tests
    // live in tests/xds_test.rs. Here we just push the SessionStore's
    // ack channel directly to simulate "envoy ACKed."

    #[tokio::test]
    async fn post_with_ack_gate_returns_503_when_no_xds_subscriber() {
        let sessions = Arc::new(SessionStore::new());
        let state = ack_state_with(sessions.clone(), Duration::from_millis(100), false);
        let response = post_session(State(state), Some(good_post("mcp_session_abc"))).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "no_xds_subscriber");
        // Rollback: the store does not retain the record envoy never
        // saw, so a session-broker retry sees a clean slate.
        assert!(sessions.get("mcp_session_abc").await.is_none());
    }

    #[tokio::test]
    async fn post_with_ack_gate_returns_503_on_timeout() {
        let sessions = Arc::new(SessionStore::new());
        // Hold a subscriber guard so wait_for_ack actually waits
        // (rather than short-circuiting to NoSubscriber).
        let _guard = sessions.xds_subscriber_guard();
        let state = ack_state_with(sessions.clone(), Duration::from_millis(75), false);
        let response = post_session(State(state), Some(good_post("mcp_session_abc"))).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "xds_ack_timeout");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or("")
                .contains("xDS version"),
            "message names the version: {body}"
        );
        assert!(sessions.get("mcp_session_abc").await.is_none());
    }

    #[tokio::test]
    async fn post_with_ack_gate_returns_201_when_envoy_acks_in_time() {
        let sessions = Arc::new(SessionStore::new());
        let _guard = sessions.xds_subscriber_guard();
        let state = ack_state_with(sessions.clone(), Duration::from_secs(5), false);

        // Subscribe BEFORE spawning the acker, then move the rx into
        // the task. The acker would otherwise race the post's insert:
        // if the spawn hadn't run by the time insert bumps the
        // generation, subscribe_generation() inside the task would
        // start at the post-bump value and rx.changed() would wait
        // forever for the next change (which would never come).
        let mut rx = sessions.subscribe_generation();
        let acker_sessions = sessions.clone();
        let acker = tokio::spawn(async move {
            rx.changed().await.expect("generation bumped");
            let new_gen = *rx.borrow_and_update();
            acker_sessions.record_acked_version(new_gen);
        });

        let response = post_session(State(state), Some(good_post("mcp_session_abc"))).await;
        acker.await.expect("acker join");
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::CREATED, "body: {body}");
        assert_eq!(body["session_id"], "mcp_session_abc");
        assert!(sessions.get("mcp_session_abc").await.is_some());
    }

    #[tokio::test]
    async fn post_with_ack_gate_disabled_skips_wait_and_returns_201() {
        let sessions = Arc::new(SessionStore::new());
        // No subscriber, no acker. With ack_disabled=true the handler
        // does NOT call wait_for_ack at all.
        let state = ack_state_with(sessions.clone(), Duration::from_secs(5), true);
        let response = post_session(State(state), Some(good_post("mcp_session_abc"))).await;
        let (status, _body) = body_status(response).await;
        assert_eq!(status, StatusCode::CREATED);
        assert!(sessions.get("mcp_session_abc").await.is_some());
    }

    #[tokio::test]
    async fn delete_with_ack_gate_returns_503_and_reinserts_on_timeout() {
        let sessions = Arc::new(SessionStore::new());

        // Pre-insert the record without going through the handler
        // (so we don't have to satisfy the ack gate on the way in).
        sessions
            .insert(SessionRecord {
                session_id: "mcp_session_abc".to_string(),
                container_ip: "172.20.0.5".parse().unwrap(),
                tenant: "phlax".to_string(),
                namespace: "mcp".to_string(),
                plugin: "fetch".to_string(),
                egress_policy: serde_json::json!({}),
            })
            .await
            .expect("pre-insert");

        let _guard = sessions.xds_subscriber_guard();
        let state = ack_state_with(sessions.clone(), Duration::from_millis(75), false);
        let response = delete_session(State(state), Path("mcp_session_abc".to_string())).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "xds_ack_timeout");
        // Rollback: record is back in the store because envoy never
        // confirmed its removal.
        assert!(sessions.get("mcp_session_abc").await.is_some());
    }
}
