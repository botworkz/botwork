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

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::sessions::{SessionRecord, SessionStore, StoreError};

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

fn validate_post(body: PostBody) -> Result<SessionRecord, Response> {
    let session_id = body.session_id.ok_or_else(|| {
        warn!("{PREFIX} post: invalid_request — missing 'session_id'");
        error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing required field 'session_id'",
        )
    })?;
    let container_ip_raw = body.container_ip.ok_or_else(|| {
        warn!("{PREFIX} post: invalid_request — missing 'container_ip'");
        error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing required field 'container_ip'",
        )
    })?;
    let tenant = body.tenant.ok_or_else(|| {
        warn!("{PREFIX} post: invalid_request — missing 'tenant'");
        error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing required field 'tenant'",
        )
    })?;
    let namespace = body.namespace.ok_or_else(|| {
        warn!("{PREFIX} post: invalid_request — missing 'namespace'");
        error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing required field 'namespace'",
        )
    })?;
    let plugin = body.plugin.ok_or_else(|| {
        warn!("{PREFIX} post: invalid_request — missing 'plugin'");
        error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing required field 'plugin'",
        )
    })?;

    if !session_id_re().is_match(&session_id) {
        warn!("{PREFIX} post: invalid_request — bad session_id '{session_id}'");
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("invalid session_id '{session_id}': must match {SESSION_ID_RE}"),
        ));
    }
    if !name_re().is_match(&tenant) {
        warn!("{PREFIX} post: invalid_request — bad tenant '{tenant}'");
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("invalid tenant '{tenant}': must match {NAME_RE}"),
        ));
    }
    if !name_re().is_match(&namespace) {
        warn!("{PREFIX} post: invalid_request — bad namespace '{namespace}'");
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("invalid namespace '{namespace}': must match {NAME_RE}"),
        ));
    }
    if !name_re().is_match(&plugin) {
        warn!("{PREFIX} post: invalid_request — bad plugin '{plugin}'");
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("invalid plugin '{plugin}': must match {NAME_RE}"),
        ));
    }

    let container_ip: Ipv4Addr = container_ip_raw.parse().map_err(|_| {
        warn!("{PREFIX} post: invalid_request — bad container_ip '{container_ip_raw}'");
        error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("invalid container_ip '{container_ip_raw}': must be IPv4 dotted-quad"),
        )
    })?;

    Ok(SessionRecord {
        session_id,
        container_ip,
        tenant,
        namespace,
        plugin,
        egress_policy: body.egress_policy.unwrap_or(serde_json::Value::Null),
    })
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

    let record = match validate_post(payload) {
        Ok(record) => record,
        Err(response) => return response,
    };

    let session_id = record.session_id.clone();
    let plugin = record.plugin.clone();
    let container_ip = record.container_ip;

    match state.sessions.insert(record).await {
        Ok(()) => {
            info!("{PREFIX} post: ok session_id={session_id} plugin={plugin} ip={container_ip}");
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
            info!(
                "{PREFIX} delete: ok session_id={id} plugin={} ip={}",
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

    fn empty_state() -> AppState {
        AppState {
            sessions: Arc::new(SessionStore::new()),
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
}
