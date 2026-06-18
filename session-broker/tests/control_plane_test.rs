//! End-to-end wire-contract tests for `session-broker::control_plane`.
//!
//! Spins up a fake control-plane on `127.0.0.1:0` and confirms the client
//! produces the expected wire shape for both `POST /sessions` and
//! `DELETE /sessions/<id>`, and maps the response status codes onto the
//! variants of `ControlPlaneError` correctly. The unit tests in
//! `src/control_plane.rs` cover serialisation and envelope decoding in
//! isolation; this file covers the transport.
//!
//! Keeps the same trust posture as production: plain HTTP, no auth.

use std::sync::Arc;
use std::time::Duration;

use botwork_session_broker::control_plane::{
    delete_session, post_session, ControlPlaneError, PostSessionRequest,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

/// Stand up a fake control-plane that accepts one request, captures the
/// raw HTTP request, responds with `status_code` + `body`, then closes.
/// Returns the base URL and a handle to the captured request body.
///
/// Splits status into `200`/`201` for ok / `400`/`409`/`500` for the
/// failure variants so we can drive a single helper through every error
/// mapping.
async fn spawn_control_plane(
    status_code: u16,
    body: &'static str,
    captured: Arc<Mutex<Option<String>>>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind control-plane");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let request = read_http_request(&mut stream).await;
        *captured.lock().await = Some(request);
        let reason = match status_code {
            200 => "OK",
            201 => "Created",
            204 => "No Content",
            400 => "Bad Request",
            404 => "Not Found",
            409 => "Conflict",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Unknown",
        };
        let response = format!(
            "HTTP/1.1 {status_code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.expect("write");
    });
    format!("http://{addr}")
}

async fn read_http_request(stream: &mut TcpStream) -> String {
    let mut raw = Vec::new();
    let mut buf = [0u8; 1024];
    let mut expected_total: Option<usize> = None;
    loop {
        let n = stream.read(&mut buf).await.expect("read");
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        if expected_total.is_none() {
            if let Some((header_end, content_len)) = parse_header_end_and_length(&raw) {
                expected_total = Some(header_end + 4 + content_len);
            }
        }
        if let Some(total) = expected_total {
            if raw.len() >= total {
                break;
            }
        }
    }
    String::from_utf8(raw).expect("utf8 request")
}

fn parse_header_end_and_length(raw: &[u8]) -> Option<(usize, usize)> {
    let header_end = raw.windows(4).position(|chunk| chunk == b"\r\n\r\n")?;
    let headers = String::from_utf8(raw[..header_end].to_vec()).ok()?;
    let content_length = headers
        .split("\r\n")
        .find_map(|line| {
            line.split_once(": ").and_then(|(name, value)| {
                if name.eq_ignore_ascii_case("content-length") {
                    value.parse::<usize>().ok()
                } else {
                    None
                }
            })
        })
        .unwrap_or(0);
    Some((header_end, content_length))
}

fn split_body(request: &str) -> &str {
    request.split("\r\n\r\n").nth(1).unwrap_or("")
}

#[tokio::test]
async fn post_session_201_succeeds() {
    let captured = Arc::new(Mutex::new(None));
    let url = spawn_control_plane(
        201,
        r#"{"status":"stored","session_id":"mcp_session_abc"}"#,
        Arc::clone(&captured),
    )
    .await;

    let body = PostSessionRequest {
        session_id: "mcp_session_abc",
        container_ip: "172.20.0.5",
        tenant: "phlax",
        namespace: "mcp",
        plugin: "fetch",
        egress_policy: &None,
    };
    let result = post_session(&url, &body, Duration::from_secs(2)).await;
    assert!(result.is_ok(), "expected ok, got {result:?}");

    let request = captured.lock().await.take().expect("captured request");
    let request_body = split_body(&request);
    let parsed: serde_json::Value = serde_json::from_str(request_body).expect("json body");
    // Wire shape regression: every field must be present and named
    // exactly; control-plane's PostBody is strict on names.
    assert_eq!(parsed["session_id"], "mcp_session_abc");
    assert_eq!(parsed["container_ip"], "172.20.0.5");
    assert_eq!(parsed["tenant"], "phlax");
    assert_eq!(parsed["namespace"], "mcp");
    assert_eq!(parsed["plugin"], "fetch");
    assert!(
        parsed.get("egress_policy").is_some(),
        "egress_policy must always be present in the wire body"
    );
    assert!(parsed["egress_policy"].is_null());

    // Sanity: confirm method and path. We don't pin the exact
    // request-target form (the client currently uses absolute form
    // `POST http://host:port/sessions HTTP/1.1`; either form is valid
    // for HTTP/1.1), but the method + path substring is enough.
    let request_line = request.lines().next().unwrap_or("");
    assert!(
        request_line.starts_with("POST ") && request_line.contains("/sessions"),
        "request line should be POST .../sessions: {request_line:?}"
    );
}

#[tokio::test]
async fn post_session_200_also_succeeds() {
    // The control-plane spec says 201 for create; we accept 200 too so
    // a future control-plane upgrade can switch without an orchestrated
    // session-broker bump. This is a defensive choice -- the active
    // contract is 201.
    let captured = Arc::new(Mutex::new(None));
    let url = spawn_control_plane(200, r#"{"status":"stored"}"#, Arc::clone(&captured)).await;
    let body = PostSessionRequest {
        session_id: "mcp_session_abc",
        container_ip: "172.20.0.5",
        tenant: "phlax",
        namespace: "mcp",
        plugin: "fetch",
        egress_policy: &None,
    };
    let result = post_session(&url, &body, Duration::from_secs(2)).await;
    assert!(result.is_ok(), "expected ok, got {result:?}");
}

#[tokio::test]
async fn post_session_409_returns_already_exists() {
    let url = spawn_control_plane(
        409,
        r#"{"error":"already_exists","message":"already known"}"#,
        Arc::new(Mutex::new(None)),
    )
    .await;
    let body = PostSessionRequest {
        session_id: "mcp_session_abc",
        container_ip: "172.20.0.5",
        tenant: "phlax",
        namespace: "mcp",
        plugin: "fetch",
        egress_policy: &None,
    };
    let err = post_session(&url, &body, Duration::from_secs(2))
        .await
        .expect_err("409 should error");
    assert!(matches!(err, ControlPlaneError::AlreadyExists(_)));
    // The spawn-time consumer surfaces this as 503 regardless of the
    // upstream 4xx -- it's a session-broker bug, not the client's.
    assert_eq!(err.status_code(), 503);
}

#[tokio::test]
async fn post_session_500_returns_internal() {
    let url = spawn_control_plane(
        500,
        r#"{"error":"internal","message":"db down"}"#,
        Arc::new(Mutex::new(None)),
    )
    .await;
    let body = PostSessionRequest {
        session_id: "mcp_session_abc",
        container_ip: "172.20.0.5",
        tenant: "phlax",
        namespace: "mcp",
        plugin: "fetch",
        egress_policy: &None,
    };
    let err = post_session(&url, &body, Duration::from_secs(2))
        .await
        .expect_err("500 should error");
    assert!(matches!(err, ControlPlaneError::Internal(_)));
    assert_eq!(err.status_code(), 503);
}

#[tokio::test]
async fn post_session_unreachable_returns_transport() {
    // Port 1 on 127.0.0.1 is reliably unbound. We don't spawn a server;
    // the connect should fail fast.
    let err = post_session(
        "http://127.0.0.1:1",
        &PostSessionRequest {
            session_id: "mcp_session_abc",
            container_ip: "172.20.0.5",
            tenant: "phlax",
            namespace: "mcp",
            plugin: "fetch",
            egress_policy: &None,
        },
        Duration::from_millis(500),
    )
    .await
    .expect_err("unreachable should error");
    assert!(matches!(err, ControlPlaneError::Transport(_)));
    assert_eq!(err.status_code(), 503);
}

#[tokio::test]
async fn post_session_with_egress_policy_serialises_object() {
    let captured = Arc::new(Mutex::new(None));
    let url = spawn_control_plane(
        201,
        r#"{"status":"stored","session_id":"mcp_session_abc"}"#,
        Arc::clone(&captured),
    )
    .await;
    let policy = serde_json::json!({
        "allow": [{"host": "api.github.com", "ports": [443]}]
    });
    let body = PostSessionRequest {
        session_id: "mcp_session_abc",
        container_ip: "172.20.0.5",
        tenant: "phlax",
        namespace: "mcp",
        plugin: "github",
        egress_policy: &Some(policy.clone()),
    };
    post_session(&url, &body, Duration::from_secs(2))
        .await
        .expect("ok");
    let request = captured.lock().await.take().expect("captured request");
    let request_body = split_body(&request);
    let parsed: serde_json::Value = serde_json::from_str(request_body).expect("json body");
    assert_eq!(parsed["egress_policy"], policy);
}

#[tokio::test]
async fn delete_session_200_succeeds() {
    let captured = Arc::new(Mutex::new(None));
    let url = spawn_control_plane(
        200,
        r#"{"status":"removed","session_id":"mcp_session_abc"}"#,
        Arc::clone(&captured),
    )
    .await;
    delete_session(&url, "mcp_session_abc", Duration::from_secs(2))
        .await
        .expect("ok");
    let request = captured.lock().await.take().expect("captured request");
    let request_line = request.lines().next().unwrap_or("");
    assert!(
        request_line.starts_with("DELETE ") && request_line.contains("/sessions/mcp_session_abc"),
        "request line should be DELETE .../sessions/mcp_session_abc: {request_line:?}"
    );
}

#[tokio::test]
async fn delete_session_404_is_treated_as_success() {
    // 404 on delete is "already gone, recovery-sync will reconcile" --
    // surfaced as Ok(()) so the cleanup path doesn't log spurious
    // errors. The unit tests in src/control_plane.rs assert the error
    // envelope mapping; this test asserts the consumer-visible behaviour.
    let url = spawn_control_plane(
        404,
        r#"{"error":"not_found","message":"unknown id"}"#,
        Arc::new(Mutex::new(None)),
    )
    .await;
    delete_session(&url, "mcp_session_xyz", Duration::from_secs(2))
        .await
        .expect("404 must collapse to Ok");
}

#[tokio::test]
async fn delete_session_500_bubbles_internal_error() {
    let url = spawn_control_plane(
        500,
        r#"{"error":"internal","message":"db down"}"#,
        Arc::new(Mutex::new(None)),
    )
    .await;
    let err = delete_session(&url, "mcp_session_abc", Duration::from_secs(2))
        .await
        .expect_err("500 should error");
    assert!(matches!(err, ControlPlaneError::Internal(_)));
    // Consumer (exit_listener) logs and ignores -- but the variant must
    // be surfaceable so it lands in the log line.
}

#[tokio::test]
async fn delete_session_unreachable_returns_transport() {
    let err = delete_session(
        "http://127.0.0.1:1",
        "mcp_session_abc",
        Duration::from_millis(500),
    )
    .await
    .expect_err("unreachable should error");
    assert!(matches!(err, ControlPlaneError::Transport(_)));
}
