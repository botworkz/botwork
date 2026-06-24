//! Wire-shape test for the session-broker `/sessions` admin
//! endpoint deserialiser.
//!
//! The full end-to-end "spin a wiremock + assert the row maps to a
//! TableRow" lives in `tests/ps_run_test.rs`; this one pins the
//! decoder contract in isolation so a future field rename surfaces
//! on its own, separate from any HTTP-layer noise.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use botwork_tools::ps::sessions::{fetch, SessionsError};

/// Stand up a minimal HTTP server on a random local port that always
/// returns the supplied body + status. Returns the URL the test
/// should hit and a JoinHandle (kept alive for the test scope so
/// the listener doesn't get dropped mid-request).
async fn spawn_fake(status: StatusCode, body: &'static str) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");

    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let _ = http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |_req: Request<Incoming>| async move {
                            let resp: Response<Full<Bytes>> = Response::builder()
                                .status(status)
                                .header("content-type", "application/json")
                                .body(Full::new(Bytes::from(body)))
                                .expect("build response");
                            Ok::<_, Infallible>(resp)
                        }),
                    )
                    .await;
            });
        }
    });
    (format!("http://{addr}/sessions"), handle)
}

#[tokio::test(flavor = "current_thread")]
async fn decodes_populated_envelope() {
    // The full broker-emitted shape; tenant / workspace / plugin /
    // agent_id are the four fields the table renderer consumes.
    let body = r#"{
        "sessions": {
            "mcp_session_aabbcc": {
                "container":    "mcp_session_aabbcc",
                "container_ip": "172.20.0.5",
                "tenant":       "phlax",
                "workspace":    "mcp",
                "plugin":       "mcp-fetch",
                "agent_id":     "agent-bound-1"
            },
            "mcp_session_ddee": {
                "container":    "mcp_session_ddee",
                "container_ip": "172.20.0.6",
                "tenant":       "phlax",
                "workspace":    "demo",
                "plugin":       "mcp-bash",
                "agent_id":     null
            }
        }
    }"#;
    let (url, _h) = spawn_fake(StatusCode::OK, body).await;

    // Run the blocking client off the runtime so it doesn't deadlock
    // the current-thread executor; the production tool is a
    // single-threaded blocking binary so this matches its shape.
    let sessions = tokio::task::spawn_blocking(move || fetch(&url))
        .await
        .expect("join")
        .expect("decode ok");

    assert_eq!(sessions.len(), 2);

    let bound = sessions.get("mcp_session_aabbcc").expect("bound row");
    assert_eq!(bound.tenant, "phlax");
    assert_eq!(bound.workspace, "mcp");
    assert_eq!(bound.plugin, "mcp-fetch");
    assert_eq!(bound.agent_id.as_deref(), Some("agent-bound-1"));

    let unbound = sessions.get("mcp_session_ddee").expect("unbound row");
    assert_eq!(unbound.plugin, "mcp-bash");
    // The pre-bind / pre-/bind-agent window is the canonical
    // null-agent case; rendered as `(unbound)` in the table.
    assert!(unbound.agent_id.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn decodes_empty_envelope() {
    // The broker serves `{"sessions":{}}` (not 404) when no
    // sessions are live. That's expressly so this tool can branch
    // on "got a response" vs "got rows", not on a missing route.
    let (url, _h) = spawn_fake(StatusCode::OK, r#"{"sessions": {}}"#).await;
    let sessions = tokio::task::spawn_blocking(move || fetch(&url))
        .await
        .expect("join")
        .expect("decode ok");
    assert!(sessions.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn non_2xx_surfaces_bad_status() {
    let (url, _h) = spawn_fake(
        StatusCode::INTERNAL_SERVER_ERROR,
        r#"{"error":"broker_unhealthy"}"#,
    )
    .await;
    let err = tokio::task::spawn_blocking(move || fetch(&url))
        .await
        .expect("join")
        .expect_err("must surface non-2xx");
    match err {
        SessionsError::BadStatus { status, .. } => assert_eq!(status, 500),
        other => panic!("expected BadStatus, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn bad_envelope_surfaces_decode_error() {
    // Missing `sessions` key entirely — a schema-drift bug.
    let (url, _h) = spawn_fake(StatusCode::OK, r#"{"unexpected":{}}"#).await;
    let err = tokio::task::spawn_blocking(move || fetch(&url))
        .await
        .expect("join")
        .expect_err("must surface decode");
    assert!(matches!(err, SessionsError::Decode(_)), "{err:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn unreachable_endpoint_surfaces_transport_error() {
    // Port 1 = tcpmux; nobody runs it, so the connect attempt
    // refuses cleanly.
    let err = tokio::task::spawn_blocking(|| {
        // Local timeout shrink so this case completes quickly even
        // when the kernel is being slow about ECONNREFUSED. The
        // production timeout is 5s; we don't need that here.
        let _ = Duration::from_millis(500); // unused — fetch uses its own constant
        fetch("http://127.0.0.1:1/sessions")
    })
    .await
    .expect("join")
    .expect_err("must surface transport");
    assert!(matches!(err, SessionsError::Transport(_)), "{err:?}");
}
