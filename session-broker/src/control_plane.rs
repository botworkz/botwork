//! HTTP client for `botwork-control-plane`.
//!
//! Mirrors `config_broker.rs` in shape: thin wrapper around
//! `hyper::client::conn` that issues a typed request and returns a typed
//! result. All status-mapping decisions are pushed into the consumer
//! (`ext_proc`) so the wire library stays uninterpreted.
//!
//! v0 wire contract (full spec in `control-plane/README.md` and botwork
//! issue #81):
//!
//! Request:  `POST /sessions` body=SessionRecord
//!     {
//!       "session_id":    "mcp_session_<token>",
//!       "container_ip":  "<ipv4>",
//!       "tenant":        "<name>",
//!       "workspace":     "<name>",
//!       "plugin":        "<name>",
//!       "egress_policy": <opaque JSON | null>
//!     }
//! Success:  `201` + ack
//! Error:    4xx/5xx with `{ "error", "message" }` envelope
//!
//! Request:  `DELETE /sessions/<id>`
//! Success:  `200` + ack
//! Error:    `404` (session unknown) | 4xx other
//!
//! Trust posture: credless. Same model as config-broker — network
//! membership of `botwork-internal` is the trust boundary, the client
//! does not authenticate.
//!
//! Failure semantics owned by the call site:
//!
//! * `post_session` failure during a spawn is a **hard fail**: the
//!   container is torn down and the client receives 503. This is the
//!   single load-bearing design property the control plane is built on
//!   (see #81): no plugin container ever serves traffic without first
//!   being announced to control-plane.
//! * `delete_session` failure on container exit is logged and ignored.
//!   The container is already gone (the launcher's exit listener is the
//!   trigger); a drifted record is reconciled by the future
//!   recovery-sync flow (control-plane polls session-broker `/sessions`
//!   on cold start). Blocking exit cleanup on control-plane reachability
//!   would punish the steady-state cleanup path for a transient outage
//!   the system will heal on its own.

use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http1;
use hyper::{Method, Request, Uri};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::log_info;

/// Wire body for `POST /sessions`.
///
/// All fields except `egress_policy` are required; the field is
/// `Option<serde_json::Value>` and serialised as JSON `null` when
/// absent (rather than omitted from the body) so the wire shape is
/// invariant across plugins with and without an `egress:` block.
/// Control-plane's `PostBody` accepts both shapes today; we settle
/// on "always present, possibly null" for predictability.
#[derive(Debug, Serialize)]
pub struct PostSessionRequest<'a> {
    pub session_id: &'a str,
    pub container_ip: &'a str,
    pub tenant: &'a str,
    pub workspace: &'a str,
    pub plugin: &'a str,
    /// Always serialised, even when `None` -- emitted as JSON `null`.
    /// See module docs for the rationale.
    pub egress_policy: &'a Option<serde_json::Value>,
}

/// All ways a control-plane call can fail. Each variant carries enough
/// detail to build the immediate-response that ext_proc returns to the
/// client when the call fired during spawn.
#[derive(Debug, thiserror::Error)]
pub enum ControlPlaneError {
    /// Body was missing / unparseable / required field absent / regex
    /// mismatch on the control-plane side. Almost always a session-broker
    /// bug: we constructed the body and we know what control-plane
    /// accepts. Pass through the 4xx detail so it shows up in logs.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Session id already present in control-plane store. Should never
    /// happen in practice -- session ids are token-derived and freshly
    /// generated per spawn -- but if it does we'd rather fail loud than
    /// silently merge.
    #[error("session_id already exists in control-plane: {0}")]
    AlreadyExists(String),
    /// DELETE for an unknown session id. On the cleanup path this is
    /// fine (the recovery-sync flow will reconcile); on a hypothetical
    /// "I just got a 201 and now you don't know about this id" hop it
    /// would be a real bug. Surfaced as a distinct variant so the
    /// caller can pick the right reaction.
    #[error("session_id not found in control-plane: {0}")]
    NotFound(String),
    /// 5xx from control-plane. Hard-fail at the spawn gate; on cleanup
    /// it is logged and ignored. Same posture session-broker already
    /// uses for config-broker 5xx (see `ConfigBrokerError::Internal`).
    #[error("control-plane internal error: {0}")]
    Internal(String),
    /// Couldn't reach control-plane (DNS, TCP, timeout). Same posture
    /// as `Internal` from the consumer's perspective: a credless intra-
    /// network call has no semantic distinction between "down" and
    /// "broken", and we treat both as hard-fail during spawn.
    #[error("transport error contacting control-plane: {0}")]
    Transport(String),
    /// Got a response but couldn't parse it as the expected envelope.
    #[error("bad response from control-plane: {0}")]
    BadResponse(String),
}

impl ControlPlaneError {
    /// HTTP status code session-broker should surface to the *client*
    /// when this error fires during spawn.
    ///
    /// The spawn-time hard gate is what makes the control-plane
    /// useful: anything that prevents us from announcing the session
    /// to control-plane is a 503. We deliberately do not pass through
    /// 400/409 (which would tell the client that *they* did something
    /// wrong, when really we did): an `InvalidRequest` or
    /// `AlreadyExists` from control-plane is a session-broker bug,
    /// and the client just sees a generic 503.
    pub fn status_code(&self) -> u32 {
        match self {
            // 503 across the board. The cleanup path (DELETE on exit)
            // does not consult this -- failure there is logged.
            Self::InvalidRequest(_)
            | Self::AlreadyExists(_)
            | Self::NotFound(_)
            | Self::Internal(_)
            | Self::Transport(_)
            | Self::BadResponse(_) => 503,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: Option<String>,
    message: Option<String>,
}

/// `POST /sessions` -- announce a freshly-spawned, probe-ready plugin
/// container to control-plane.
///
/// Returns `Ok(())` only on 2xx; everything else maps onto
/// `ControlPlaneError` and is the caller's job to handle. The single
/// load-bearing rule is documented at the call site: no unpoliced
/// container ever serves traffic, so the caller must teardown the
/// container on `Err`.
pub async fn post_session(
    endpoint: &str,
    body: &PostSessionRequest<'_>,
    request_timeout: Duration,
) -> Result<(), ControlPlaneError> {
    let url = format!("{}/sessions", endpoint.trim_end_matches('/'));
    log_info(&format!(
        "control-plane post: session_id={} plugin={} ip={} url={}",
        body.session_id, body.plugin, body.container_ip, url
    ));

    let payload = serde_json::to_vec(body)
        .map_err(|e| ControlPlaneError::Transport(format!("encode request: {e}")))?;

    let (status, response_body) = send_request(&url, Method::POST, Some(payload), request_timeout)
        .await
        .map_err(|e| ControlPlaneError::Transport(e.to_string()))?;

    match status {
        200 | 201 => {
            log_info(&format!(
                "control-plane post: ok session_id={} status={status}",
                body.session_id
            ));
            Ok(())
        }
        other => {
            let err = error_from_envelope(other, &response_body);
            log_info(&format!(
                "control-plane post: error session_id={} {err}",
                body.session_id
            ));
            Err(err)
        }
    }
}

/// `DELETE /sessions/<id>` -- tell control-plane the session is gone.
///
/// Called from the exit-listener path after a container-exit event from
/// the launcher. Returns `Ok(())` on 200 / 404 (the latter is "already
/// gone, recovery-sync will reconcile"); other failures bubble up so
/// the caller can log them. The caller treats any error as
/// non-fatal -- see module docs.
pub async fn delete_session(
    endpoint: &str,
    session_id: &str,
    request_timeout: Duration,
) -> Result<(), ControlPlaneError> {
    let url = format!("{}/sessions/{session_id}", endpoint.trim_end_matches('/'));
    log_info(&format!(
        "control-plane delete: session_id={session_id} url={url}"
    ));

    let (status, response_body) = send_request(&url, Method::DELETE, None, request_timeout)
        .await
        .map_err(|e| ControlPlaneError::Transport(e.to_string()))?;

    match status {
        200 => {
            log_info(&format!("control-plane delete: ok session_id={session_id}"));
            Ok(())
        }
        404 => {
            log_info(&format!(
                "control-plane delete: not_found session_id={session_id} (already removed; recovery-sync will reconcile)"
            ));
            // Treat as success: the cleanup goal is "control-plane no
            // longer has this id", and it doesn't. The consumer never
            // needs to distinguish 200 from 404.
            Ok(())
        }
        other => {
            let err = error_from_envelope(other, &response_body);
            log_info(&format!(
                "control-plane delete: error session_id={session_id} {err}"
            ));
            Err(err)
        }
    }
}

async fn send_request(
    url: &str,
    method: Method,
    body: Option<Vec<u8>>,
    request_timeout: Duration,
) -> Result<(u16, Vec<u8>), ControlPlaneError> {
    let uri: Uri = url
        .parse()
        .map_err(|e| ControlPlaneError::Transport(format!("invalid endpoint URL: {e}")))?;
    let host = uri
        .host()
        .ok_or_else(|| {
            ControlPlaneError::Transport("missing host in control-plane endpoint".to_string())
        })?
        .to_string();
    let port = uri.port_u16().unwrap_or(80);
    let authority = if let Some(explicit_port) = uri.port_u16() {
        format!("{host}:{explicit_port}")
    } else {
        host.clone()
    };
    let path = uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

    let mut sender = timeout(request_timeout, async {
        let stream = TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|e| ControlPlaneError::Transport(e.to_string()))?;
        let io = TokioIo::new(stream);
        let (sender, conn) = http1::handshake(io)
            .await
            .map_err(|e| ControlPlaneError::Transport(e.to_string()))?;
        tokio::spawn(async move {
            if let Err(err) = conn.await {
                log_info(&format!("control-plane HTTP connection error: {err}"));
            }
        });
        Ok::<_, ControlPlaneError>(sender)
    })
    .await
    .map_err(|e| ControlPlaneError::Transport(e.to_string()))??;

    let body_bytes = body.unwrap_or_default();
    let content_length = body_bytes.len().to_string();
    let request = Request::builder()
        .method(method)
        .uri(format!("http://{authority}{path}"))
        .header("Host", authority)
        .header("Content-Type", "application/json")
        .header("Content-Length", content_length)
        .body(Full::new(Bytes::from(body_bytes)))
        .map_err(|e| ControlPlaneError::Transport(e.to_string()))?;

    let response = timeout(request_timeout, sender.send_request(request))
        .await
        .map_err(|e| ControlPlaneError::Transport(e.to_string()))?
        .map_err(|e| ControlPlaneError::Transport(e.to_string()))?;
    let status = response.status().as_u16();
    let response_body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| ControlPlaneError::Transport(e.to_string()))?
        .to_bytes()
        .to_vec();

    Ok((status, response_body))
}

fn error_from_envelope(status: u16, body: &[u8]) -> ControlPlaneError {
    let detail = serde_json::from_slice::<ErrorEnvelope>(body)
        .ok()
        .and_then(|env| {
            let code = env.error.unwrap_or_default();
            let message = env.message.unwrap_or_default();
            if code.is_empty() && message.is_empty() {
                None
            } else {
                Some((code, message))
            }
        });

    match (status, detail) {
        (400, Some((_, message))) => ControlPlaneError::InvalidRequest(message),
        (404, Some((_, message))) => ControlPlaneError::NotFound(message),
        (409, Some((_, message))) => ControlPlaneError::AlreadyExists(message),
        (500, Some((_, message))) => ControlPlaneError::Internal(message),
        (503, Some((_, message))) => ControlPlaneError::Internal(message),
        (status, detail) => ControlPlaneError::BadResponse(format!(
            "HTTP {status}: {}",
            detail
                .map(|(code, msg)| format!("{code}: {msg}"))
                .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string())
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_request_serialises_with_explicit_null_egress() {
        // The wire shape MUST always include `egress_policy` -- even
        // for plugins without an `egress:` block. This regression
        // test guards the contract.
        let body = PostSessionRequest {
            session_id: "mcp_session_abc",
            container_ip: "172.20.0.5",
            tenant: "phlax",
            workspace: "mcp",
            plugin: "fetch",
            egress_policy: &None,
        };
        let json = serde_json::to_value(&body).expect("encode");
        assert_eq!(json["session_id"], "mcp_session_abc");
        assert_eq!(json["container_ip"], "172.20.0.5");
        assert_eq!(json["tenant"], "phlax");
        assert_eq!(json["workspace"], "mcp");
        assert_eq!(json["plugin"], "fetch");
        assert!(
            json.get("egress_policy").is_some(),
            "egress_policy must be present in the wire body even when None"
        );
        assert!(json["egress_policy"].is_null());
    }

    #[test]
    fn post_request_serialises_with_object_egress() {
        let policy = serde_json::json!({
            "allow": [{"host": "api.github.com", "ports": [443]}]
        });
        let body = PostSessionRequest {
            session_id: "mcp_session_abc",
            container_ip: "172.20.0.5",
            tenant: "phlax",
            workspace: "mcp",
            plugin: "github",
            egress_policy: &Some(policy.clone()),
        };
        let json = serde_json::to_value(&body).expect("encode");
        assert_eq!(json["egress_policy"], policy);
    }

    #[test]
    fn error_envelope_maps_400_invalid_request() {
        let body = br#"{"error":"invalid_request","message":"bad ip"}"#;
        let err = error_from_envelope(400, body);
        assert!(matches!(err, ControlPlaneError::InvalidRequest(_)));
        // Spawn-time treatment: surface as 503 regardless of the
        // upstream 4xx (it's a session-broker bug, not the client's).
        assert_eq!(err.status_code(), 503);
    }

    #[test]
    fn error_envelope_maps_409_already_exists() {
        let body = br#"{"error":"already_exists","message":"already known"}"#;
        let err = error_from_envelope(409, body);
        assert!(matches!(err, ControlPlaneError::AlreadyExists(_)));
        assert_eq!(err.status_code(), 503);
    }

    #[test]
    fn error_envelope_maps_404_not_found() {
        let body = br#"{"error":"not_found","message":"unknown id"}"#;
        let err = error_from_envelope(404, body);
        assert!(matches!(err, ControlPlaneError::NotFound(_)));
        assert_eq!(err.status_code(), 503);
    }

    #[test]
    fn error_envelope_maps_500_internal() {
        let body = br#"{"error":"internal","message":"db down"}"#;
        let err = error_from_envelope(500, body);
        assert!(matches!(err, ControlPlaneError::Internal(_)));
        assert_eq!(err.status_code(), 503);
    }

    #[test]
    fn error_envelope_unparseable_falls_through_to_bad_response() {
        let body = b"<html>boom</html>";
        let err = error_from_envelope(418, body);
        assert!(matches!(err, ControlPlaneError::BadResponse(_)));
        assert_eq!(err.status_code(), 503);
    }

    #[test]
    fn error_envelope_unknown_status_falls_through_to_bad_response() {
        // Any unmapped status -- including a status with a recognisable
        // envelope but a code we don't have a variant for -- should fall
        // through to BadResponse so the consumer can log the raw detail.
        let body = br#"{"error":"i_am_a_teapot","message":"short and stout"}"#;
        let err = error_from_envelope(418, body);
        assert!(matches!(err, ControlPlaneError::BadResponse(_)));
    }
}
