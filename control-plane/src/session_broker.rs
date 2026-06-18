//! HTTP client for `botwork-session-broker`'s recovery-sync endpoint.
//!
//! Mirrors session-broker's `config_broker.rs` in shape: thin wrapper
//! around `hyper::client::conn` that issues a typed GET and returns a
//! typed result. The endpoint queried is the admin server's
//! `GET /control-plane/sessions`, added in botwork #84 specifically as
//! the recovery-sync source of truth.
//!
//! The wire body, per session-broker `admin.rs`:
//!
//! ```json
//! {
//!   "sessions": [
//!     {
//!       "session_id":    "mcp_session_<token>",
//!       "container_ip":  "<ipv4>",
//!       "tenant":        "<name>",
//!       "namespace":     "<name>",
//!       "plugin":        "<name>",
//!       "egress_policy": <opaque JSON | null>
//!     },
//!     ...
//!   ]
//! }
//! ```
//!
//! v0 control-plane is a strict consumer of this shape: any failure --
//! transport, HTTP non-2xx, JSON parse, record validation -- is surfaced
//! to the caller (`recovery.rs`) so it can retry-and-exit rather than
//! silently start with a wrong view of the world.
//!
//! Trust posture: credless. Same as session-broker → config-broker.
//! Network membership is the boundary; this client does not authenticate.

use std::net::Ipv4Addr;
use std::time::Duration;

use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper::client::conn::http1;
use hyper::{Method, Request, Uri};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::warn;

use crate::sessions::SessionRecord;

const PREFIX: &str = "[control-plane:recovery]";

/// All ways a recovery-sync fetch can fail.
///
/// Distinguished so `recovery.rs` can log meaningfully and (in a future
/// iteration) tune its retry strategy per failure mode. v0 retries all
/// of them uniformly with the same backoff, then gives up; systemd's
/// `Restart=` resumes the loop from scratch.
#[derive(Debug, thiserror::Error)]
pub enum SessionBrokerError {
    /// Couldn't reach session-broker at all (DNS / TCP / handshake /
    /// timeout). Equally likely to be a fresh deployment where
    /// session-broker hasn't bound yet OR a permanent misconfiguration;
    /// only retries can tell them apart.
    #[error("transport error contacting session-broker: {0}")]
    Transport(String),
    /// Got a response but the status was not 2xx. session-broker should
    /// always 200 this endpoint -- a non-200 means the broker is
    /// unhealthy (parsing its own state failed, registry not yet
    /// loaded, etc). Surface as a distinct variant so logs make the
    /// difference visible.
    #[error("session-broker returned HTTP {status}: {body}")]
    BadStatus { status: u16, body: String },
    /// 2xx body did not parse as the expected `{"sessions": [...]}`
    /// envelope, or one of the records inside it failed validation.
    /// Either is a schema-drift bug between the two services and would
    /// be a real production problem; control-plane refuses to seed
    /// from a malformed snapshot.
    #[error("bad response from session-broker: {0}")]
    BadResponse(String),
}

/// Wire body for `GET /control-plane/sessions`.
#[derive(Debug, Deserialize)]
struct SessionsBody {
    sessions: Vec<SessionBrokerRecord>,
}

/// One record as session-broker emits it. Mirrors
/// `admin::ControlPlaneSessionView`.
///
/// `container_ip` is `String` on the wire (matches the surrounding
/// stack) and converted to `Ipv4Addr` when we project into a
/// `SessionRecord`; bad addresses become `BadResponse`, not silent
/// drops.
#[derive(Debug, Deserialize)]
struct SessionBrokerRecord {
    session_id: String,
    container_ip: String,
    tenant: String,
    namespace: String,
    plugin: String,
    /// `null` on the wire when the plugin has no `egress:` block;
    /// pass through verbatim (control-plane stores it as opaque JSON,
    /// schema lives in config-broker and the xDS materialiser).
    egress_policy: serde_json::Value,
}

impl SessionBrokerRecord {
    fn into_session_record(self) -> Result<SessionRecord, SessionBrokerError> {
        let container_ip: Ipv4Addr = self.container_ip.parse().map_err(|_| {
            SessionBrokerError::BadResponse(format!(
                "session-broker emitted non-IPv4 container_ip {:?} for session {:?}",
                self.container_ip, self.session_id
            ))
        })?;
        Ok(SessionRecord {
            session_id: self.session_id,
            container_ip,
            tenant: self.tenant,
            namespace: self.namespace,
            plugin: self.plugin,
            egress_policy: self.egress_policy,
        })
    }
}

/// One round-trip of the recovery sync.
///
/// `endpoint` is session-broker's admin base URL (no trailing slash
/// required); `/control-plane/sessions` is appended internally.
/// `request_timeout` covers connect + send + receive.
///
/// Returns the parsed records; the orchestrator (`recovery.rs`) decides
/// whether to retry or proceed.
pub async fn fetch_sessions(
    endpoint: &str,
    request_timeout: Duration,
) -> Result<Vec<SessionRecord>, SessionBrokerError> {
    let url = format!("{}/control-plane/sessions", endpoint.trim_end_matches('/'));
    let uri: Uri = url
        .parse()
        .map_err(|e| SessionBrokerError::Transport(format!("invalid endpoint URL: {e}")))?;
    let host = uri
        .host()
        .ok_or_else(|| {
            SessionBrokerError::Transport("missing host in session-broker endpoint".to_string())
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
        .unwrap_or_else(|| "/control-plane/sessions".to_string());

    let mut sender = timeout(request_timeout, async {
        let stream = TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|e| SessionBrokerError::Transport(e.to_string()))?;
        let io = TokioIo::new(stream);
        let (sender, conn) = http1::handshake(io)
            .await
            .map_err(|e| SessionBrokerError::Transport(e.to_string()))?;
        tokio::spawn(async move {
            if let Err(err) = conn.await {
                // Connection errors after the response has been read
                // are noise (peer closing the keepalive); log and move
                // on, since the caller already has the response body.
                warn!("{PREFIX} HTTP connection terminated: {err}");
            }
        });
        Ok::<_, SessionBrokerError>(sender)
    })
    .await
    .map_err(|e| SessionBrokerError::Transport(e.to_string()))??;

    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("http://{authority}{path}"))
        .header("Host", authority)
        .header("Accept", "application/json")
        .body(Empty::<Bytes>::new())
        .map_err(|e| SessionBrokerError::Transport(e.to_string()))?;

    let response = timeout(request_timeout, sender.send_request(request))
        .await
        .map_err(|e| SessionBrokerError::Transport(e.to_string()))?
        .map_err(|e| SessionBrokerError::Transport(e.to_string()))?;
    let status = response.status().as_u16();
    let body_bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|e| SessionBrokerError::Transport(e.to_string()))?
        .to_bytes();

    if !(200..300).contains(&status) {
        // Truncate the body in the error message so a huge HTML 502 page
        // (e.g. from a misconfigured proxy in front of session-broker)
        // doesn't drown the log.
        let snippet = String::from_utf8_lossy(&body_bytes);
        let snippet = snippet.chars().take(200).collect::<String>();
        return Err(SessionBrokerError::BadStatus {
            status,
            body: snippet,
        });
    }

    let envelope: SessionsBody = serde_json::from_slice(&body_bytes).map_err(|e| {
        SessionBrokerError::BadResponse(format!(
            "could not parse {{\"sessions\":[...]}} envelope: {e}"
        ))
    })?;

    let mut records = Vec::with_capacity(envelope.sessions.len());
    for record in envelope.sessions {
        records.push(record.into_session_record()?);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::sync::Arc;

    use http_body_util::Full;
    use hyper::body::Incoming;
    use hyper::server::conn::http1 as server_http1;
    use hyper::service::service_fn;
    use hyper::{Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;
    use tokio::task::JoinHandle;

    /// Spawn a minimal HTTP server that serves the same canned body for
    /// every request, captures the request path so tests can assert on
    /// it, and exposes the bind address. The handle is returned so the
    /// task is dropped (and the server torn down) at end of test.
    async fn spawn_fake(
        status: StatusCode,
        body: &'static str,
    ) -> (String, Arc<Mutex<Vec<String>>>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let captured_clone = captured.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let captured = captured_clone.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let _ = server_http1::Builder::new()
                        .serve_connection(
                            io,
                            service_fn(move |req: Request<Incoming>| {
                                let captured = captured.clone();
                                async move {
                                    captured.lock().await.push(req.uri().path().to_string());
                                    let response: Response<Full<Bytes>> = Response::builder()
                                        .status(status)
                                        .header("content-type", "application/json")
                                        .body(Full::new(Bytes::from(body)))
                                        .expect("build response");
                                    Ok::<_, Infallible>(response)
                                }
                            }),
                        )
                        .await;
                });
            }
        });
        (format!("http://{addr}"), captured, handle)
    }

    #[tokio::test]
    async fn fetches_and_projects_records() {
        let (endpoint, captured, _h) = spawn_fake(
            StatusCode::OK,
            r#"{"sessions":[
                {"session_id":"mcp_session_a","container_ip":"172.20.0.5","tenant":"phlax","namespace":"mcp","plugin":"fetch","egress_policy":null},
                {"session_id":"mcp_session_b","container_ip":"172.20.0.6","tenant":"phlax","namespace":"mcp","plugin":"git","egress_policy":{"mode":"allow_all"}}
            ]}"#,
        )
        .await;

        let records = fetch_sessions(&endpoint, Duration::from_secs(2))
            .await
            .expect("fetch ok");
        assert_eq!(records.len(), 2);

        let ids: Vec<&str> = records.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(ids, vec!["mcp_session_a", "mcp_session_b"]);

        assert_eq!(
            records[0].container_ip,
            "172.20.0.5".parse::<Ipv4Addr>().unwrap()
        );
        assert!(records[0].egress_policy.is_null());

        assert_eq!(records[1].plugin, "git");
        assert_eq!(records[1].egress_policy["mode"], "allow_all");

        let paths = captured.lock().await;
        assert_eq!(*paths, vec!["/control-plane/sessions"]);
    }

    #[tokio::test]
    async fn empty_sessions_array_is_ok() {
        // The "fresh cold start, no live sessions" case. Recovery
        // sync MUST treat this as success; otherwise every fresh
        // deploy fails to start.
        let (endpoint, _captured, _h) = spawn_fake(StatusCode::OK, r#"{"sessions":[]}"#).await;
        let records = fetch_sessions(&endpoint, Duration::from_secs(2))
            .await
            .expect("empty ok");
        assert!(records.is_empty());
    }

    #[tokio::test]
    async fn non_200_response_is_bad_status() {
        let (endpoint, _captured, _h) = spawn_fake(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"registry_load_failed"}"#,
        )
        .await;
        let err = fetch_sessions(&endpoint, Duration::from_secs(2))
            .await
            .expect_err("must surface non-2xx");
        match err {
            SessionBrokerError::BadStatus { status, body } => {
                assert_eq!(status, 500);
                assert!(body.contains("registry_load_failed"), "{body}");
            }
            other => panic!("expected BadStatus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bad_envelope_shape_is_bad_response() {
        // Missing `sessions` key entirely -- a schema-drift bug.
        let (endpoint, _captured, _h) = spawn_fake(StatusCode::OK, r#"{"unexpected":[]}"#).await;
        let err = fetch_sessions(&endpoint, Duration::from_secs(2))
            .await
            .expect_err("must surface schema drift");
        assert!(matches!(err, SessionBrokerError::BadResponse(_)), "{err:?}");
    }

    #[tokio::test]
    async fn bad_container_ip_in_record_is_bad_response() {
        let (endpoint, _captured, _h) = spawn_fake(
            StatusCode::OK,
            r#"{"sessions":[
                {"session_id":"mcp_session_a","container_ip":"not-an-ip","tenant":"phlax","namespace":"mcp","plugin":"fetch","egress_policy":null}
            ]}"#,
        )
        .await;
        let err = fetch_sessions(&endpoint, Duration::from_secs(2))
            .await
            .expect_err("must surface bad ip");
        match err {
            SessionBrokerError::BadResponse(msg) => {
                assert!(msg.contains("not-an-ip"), "{msg}");
                assert!(msg.contains("mcp_session_a"), "{msg}");
            }
            other => panic!("expected BadResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unreachable_endpoint_is_transport_error() {
        // Port 1 is the canonical "nothing listening" port; reserved
        // for tcpmux which nobody runs.
        let err = fetch_sessions("http://127.0.0.1:1", Duration::from_millis(200))
            .await
            .expect_err("must surface transport");
        assert!(matches!(err, SessionBrokerError::Transport(_)), "{err:?}");
    }
}
