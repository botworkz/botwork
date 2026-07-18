//! Integration smoke for `botctl mcp-probe`.
//!
//! Stands up a fake MCP server with [`hyper`] (no rmcp dependency —
//! the probe drives the wire directly, so an in-process JSON-RPC
//! responder is enough to exercise the full probe pipeline against
//! a known shape).
//!
//! The headline scenario is `compose_pipeline_against_in_process_server`:
//! exercises [`botwork_ctl::mcp_probe::probe::handshake`] against a
//! hyper server that answers `initialize`, `notifications/initialized`,
//! and `tools/list`, then runs the captured catalog through
//! [`botwork_ctl::mcp_probe::compose::compose`] and asserts the
//! full label set matches the v1 schema. No docker, no patch, no
//! verify — proves the probe→compose pipe end-to-end without a
//! container runtime in CI.
//!
//! The hyper server here is the same shape the
//! `session-broker/tests/ext_proc_test.rs` integration tests use,
//! so the probe is being exercised against a server that follows
//! the on-the-wire conventions session-broker also enforces. A
//! drift between this fake and a real MCP server would surface as
//! a probe error on the first plugin that adopted the action;
//! locking it here means the producer side's HTTP shape stays
//! frozen against the same testbed every other broker integration
//! test uses.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value as JsonValue};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use botwork_api_core::package::{Isolation, PackageFileEntry, SpillEntry, SpillMode};
use botwork_ctl::mcp_probe::compose::compose;
use botwork_ctl::mcp_probe::probe::handshake;
use botwork_ctl::VERSION;

/// Spin a fake MCP server on a random local port. Returns the URL
/// the probe should hit + the JoinHandle (kept alive for the test
/// scope so the listener doesn't drop mid-request).
///
/// The server implements just enough of the Streamable-HTTP wire
/// shape for the probe's handshake to walk to completion:
/// `initialize` returns a `serverInfo` block + a `capabilities` map
/// that advertises `tools`; `notifications/initialized` returns
/// `202 Accepted` with an empty body; `tools/list` returns one
/// canned tool.
async fn spawn_fake_mcp_server() -> (String, JoinHandle<()>) {
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
                        service_fn(move |req: Request<Incoming>| async move {
                            let resp = handle_request(req).await;
                            Ok::<_, Infallible>(resp)
                        }),
                    )
                    .await;
            });
        }
    });
    (format!("http://{addr}/mcp"), handle)
}

async fn handle_request(req: Request<Incoming>) -> Response<Full<Bytes>> {
    // Only POST is in scope for the probe — sanity-check this so a
    // bug in the probe (e.g. accidentally GETing) trips here rather
    // than silently passing.
    if req.method() != hyper::Method::POST {
        return Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Full::new(Bytes::from_static(b"")))
            .expect("response");
    }

    // Capture headers before consuming the body.
    let headers = req.headers().clone();

    let bytes = req
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    let env: JsonValue = serde_json::from_slice(&bytes).unwrap_or(JsonValue::Null);
    let method = env.get("method").and_then(JsonValue::as_str).unwrap_or("");
    let id = env.get("id").cloned().unwrap_or(JsonValue::Null);

    if method == "initialize" {
        assert_eq!(
            env.pointer("/params/clientInfo/version")
                .and_then(JsonValue::as_str),
            Some(VERSION)
        );
        // The Streamable HTTP spec requires that initialize does NOT
        // carry MCP-Protocol-Version — the version is not yet
        // negotiated at this point.
        assert!(
            !headers.contains_key("mcp-protocol-version"),
            "initialize must NOT carry MCP-Protocol-Version header per MCP spec"
        );
    } else if !method.is_empty() {
        // Every post-initialize call must carry the negotiated version.
        assert_eq!(
            headers
                .get("mcp-protocol-version")
                .and_then(|v| v.to_str().ok()),
            Some("2025-06-18"),
            "post-initialize call {method:?} must carry MCP-Protocol-Version: 2025-06-18"
        );
    }

    let body: JsonValue = match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake-mcp-echo", "version": "0.0.1"},
            }
        }),
        "notifications/initialized" => {
            // Notifications get a 202 No Content with empty body
            // — let probe::call gracefully handle the empty case.
            return Response::builder()
                .status(StatusCode::ACCEPTED)
                .body(Full::new(Bytes::from_static(b"")))
                .expect("response");
        }
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": [
                    {
                        "name": "echo",
                        "description": "echo back the input",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"input": {"type": "string"}}
                        }
                    }
                ]
            }
        }),
        other => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("method not found: {other}")}
        }),
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("response")
}

fn minimal_package() -> PackageFileEntry {
    PackageFileEntry {
        name: "echo".to_string(),
        port: None,
        path: None,
        upstream_auth: None,
        isolation: Isolation::Shared,
        egress: serde_yaml::Value::String("none".to_string()),
        resources: None,
        env: None,
        spill: SpillEntry {
            mode: SpillMode::Never,
            threshold_bytes: None,
            include_methods: None,
            include_tools: None,
        },
    }
}

#[tokio::test(flavor = "current_thread")]
async fn compose_pipeline_against_in_process_server() {
    let (url, _h) = spawn_fake_mcp_server().await;

    // The probe's handshake is blocking (reqwest::blocking) so run
    // it off the runtime — same shape ps_sessions.rs uses.
    let probe = tokio::task::spawn_blocking(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        handshake(&url, deadline)
    })
    .await
    .expect("join")
    .expect("handshake ok");

    // Validate the captured-side properties before feeding into
    // compose — these are the contracts the probe owns.
    assert_eq!(probe.server_info.name, "fake-mcp-echo");
    assert_eq!(probe.server_info.version.as_deref(), Some("0.0.1"));
    assert_eq!(probe.protocol_version, "2025-06-18");
    assert_eq!(probe.tools.len(), 1);
    assert_eq!(probe.tools[0]["name"], "echo");
    // We didn't advertise resources/prompts capabilities, so the
    // probe should have skipped both list calls.
    assert!(probe.resources.is_empty());
    assert!(probe.prompts.is_empty());

    let pkg =
        botwork_api_core::package::validate_package(&minimal_package()).expect("package validates");
    let labels = compose(&pkg, &probe).expect("compose");

    // Spot-check the full pipeline — schema version, tool count,
    // server info, and resources count (= 0, present even though
    // the server didn't advertise the capability).
    assert_eq!(
        labels.get("org.botwork.mcp.schema-version"),
        Some(&"1".to_string())
    );
    assert_eq!(
        labels.get("org.botwork.mcp.name"),
        Some(&"echo".to_string())
    );
    assert_eq!(
        labels.get("org.botwork.mcp.tools.count"),
        Some(&"1".to_string())
    );
    assert_eq!(
        labels.get("org.botwork.mcp.tools.0.name"),
        Some(&"echo".to_string())
    );
    assert_eq!(
        labels.get("org.botwork.mcp.server-info.name"),
        Some(&"fake-mcp-echo".to_string())
    );
    assert_eq!(
        labels.get("org.botwork.mcp.resources.count"),
        Some(&"0".to_string())
    );
    // The input schema came through compose's JSON-compact step:
    // no spaces around `:`.
    let schema = labels
        .get("org.botwork.mcp.tools.0.input-schema")
        .expect("input-schema present");
    assert!(!schema.contains(": "), "schema not compact: {schema}");
}

#[tokio::test(flavor = "current_thread")]
async fn probe_surfaces_jsonrpc_error_when_initialize_fails() {
    // Wire the server to refuse initialize. The probe should
    // surface this as a JsonRpcError so the CLI exit-code mapping
    // (5 = handshake error) catches it.
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let _h = tokio::spawn(async move {
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
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "error": {"code": -32000, "message": "no thanks"}
                            });
                            let resp: Response<Full<Bytes>> = Response::builder()
                                .status(StatusCode::OK)
                                .header("content-type", "application/json")
                                .body(Full::new(Bytes::from(body.to_string())))
                                .expect("response");
                            Ok::<_, Infallible>(resp)
                        }),
                    )
                    .await;
            });
        }
    });

    let url = format!("http://{addr}/mcp");
    let err = tokio::task::spawn_blocking(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        handshake(&url, deadline)
    })
    .await
    .expect("join")
    .expect_err("must surface JsonRpcError");
    let msg = format!("{err}");
    assert!(
        msg.contains("JSON-RPC error") || msg.contains("no thanks"),
        "{msg}"
    );
    // Mapping into the exit-code bucket the CLI uses (5).
    assert_eq!(err.exit_code(), 5);
}

#[tokio::test(flavor = "current_thread")]
async fn probe_surfaces_handshake_shape_error_when_server_omits_serverinfo() {
    // initialize that returns no serverInfo. The probe should
    // surface this as a HandshakeShape error → exit 5.
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let _h = tokio::spawn(async move {
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
                            let body = json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-06-18",
                                    "capabilities": {},
                                    // intentionally no serverInfo
                                }
                            });
                            let resp: Response<Full<Bytes>> = Response::builder()
                                .status(StatusCode::OK)
                                .header("content-type", "application/json")
                                .body(Full::new(Bytes::from(body.to_string())))
                                .expect("response");
                            Ok::<_, Infallible>(resp)
                        }),
                    )
                    .await;
            });
        }
    });

    let url = format!("http://{addr}/mcp");
    let err = tokio::task::spawn_blocking(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        handshake(&url, deadline)
    })
    .await
    .expect("join")
    .expect_err("must surface HandshakeShape");
    assert_eq!(err.exit_code(), 5);
    let msg = format!("{err}");
    assert!(msg.contains("serverInfo"), "{msg}");
}

// The docker-gated full-pipeline smoke that used to live here was a
// placeholder — it gated on `docker version` then returned without
// exercising the probe→generate→verify pipeline. Per reviewer
// feedback it has been dropped rather than carried as a permanent
// TODO; the real end-to-end coverage is the
// `actions/mcp-probe/action.yml` composite step exercised by
// consumer-repo CI against the published `botctl` binary. The
// three in-process tests above cover the probe→compose pipe directly
// and need no docker, so the unit-test-side acceptance criterion
// from the RFE is met without the placeholder.
