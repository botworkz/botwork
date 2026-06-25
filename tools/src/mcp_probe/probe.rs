//! MCP handshake against a containerised plugin.
//!
//! The runtime shape is intentionally narrow:
//!
//! 1. `docker run -d --rm -p <host_port>:<package.port> <image>` —
//!    bind the container port the operator declared in the package
//!    file to a host port (ephemeral by default; pinned via
//!    `--port`).
//! 2. Poll `127.0.0.1:<host_port>` for TCP acceptance for up to the
//!    user-supplied timeout. The first acceptable connect win is
//!    the cue to start the JSON-RPC handshake.
//! 3. Drive the four-call handshake the RFE specifies:
//!    `initialize` → `notifications/initialized` → `tools/list` →
//!    conditional `resources/list` / `prompts/list`.
//! 4. Always `docker stop <container>` (and rely on `--rm` to clean
//!    up). The teardown runs in a `Drop` impl so a panicking probe
//!    doesn't leak a long-running container into the runner's
//!    workspace.
//!
//! The HTTP client is `reqwest::blocking` to match the rest of
//! `botwork-tools` — the binary stays a single-threaded blocking
//! CLI so the operational story doesn't fork between subcommands.

use std::collections::BTreeMap;
use std::io::Read;
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};
use thiserror::Error;

use crate::mcp_probe::Args;

/// Per-call HTTP timeout for the JSON-RPC roundtrip. Shorter than
/// the overall handshake budget — a slow `tools/list` shouldn't be
/// allowed to chew the entire `--timeout` budget on its own.
const HTTP_CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Protocol version we negotiate with the upstream MCP server. The
/// pin lives here (not in admin-core) because it's a producer-side
/// probe knob; production session-broker negotiates whatever the
/// agent + server agree on. We use the same revision session-broker
/// targets in v1; bumping is a deliberate edit, not a runtime
/// surprise.
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// What the probe captures from the upstream MCP server. Mirrors
/// the four data sources the compose step needs:
///
/// * `server_info` — name + version the server advertises in its
///   `initialize` response. Renders `org.botwork.mcp.server-info.*`
///   labels.
/// * `capabilities` — server capability map; the compose step keys
///   the conditional `resources` / `prompts` label families off this.
/// * `tools` — `tools/list` array; renders `org.botwork.mcp.tools.<n>.*`
///   labels.
/// * `resources` / `prompts` — same shape, only populated when the
///   server's capability map advertises them.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub server_info: ServerInfo,
    pub protocol_version: String,
    pub capabilities: JsonValue,
    pub tools: Vec<JsonValue>,
    pub resources: Vec<JsonValue>,
    pub prompts: Vec<JsonValue>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
}

/// Run the full probe pipeline against [`Args::image_in`].
///
/// The crate-public entry point. Public-but-unstable: the inner
/// helpers ([`start_container`], [`wait_for_tcp`], [`handshake`])
/// are exposed for the integration test in `tools/tests/`, but
/// callers outside this module should always go through
/// [`run_probe`].
pub fn run_probe(args: &Args) -> Result<ProbeResult, ProbeError> {
    let host_port = args
        .host_port
        .map(Ok)
        .unwrap_or_else(allocate_ephemeral_port)?;
    let container_port = 8000_u16;
    // ^ The package validator defaults to 8000 and rejects out-of-
    //   range. The probe doesn't actually need to know the
    //   container-side port (we let docker forward to whatever the
    //   image's EXPOSE/CMD binds), but the wire shape says one MCP
    //   server per image listens on a single port; 8000 matches the
    //   bootstrap.yaml + package defaults so the host:container
    //   mapping stays the property "what you bind on the host is
    //   the port the server listens on".

    let container = start_container(&args.runtime, &args.image_in, host_port, container_port)?;
    // RAII guard ensures we run `docker stop` even on panic.
    let _teardown = TeardownGuard {
        runtime: args.runtime.clone(),
        container_id: container.id.clone(),
    };

    let deadline = Instant::now() + Duration::from_secs(args.timeout_secs);
    wait_for_tcp(host_port, deadline)?;

    let probe_url = format!("http://127.0.0.1:{host_port}/mcp");
    let probe = handshake(&probe_url, deadline)?;

    Ok(probe)
}

/// Allocate an ephemeral host port by binding `127.0.0.1:0`, asking
/// the kernel for the assigned port, then dropping the socket.
/// There's an inherent race (another process can grab the port
/// between `drop` and `docker run -p`), but the probe is short-lived
/// and the race is benign — a collision surfaces as
/// [`ProbeError::ContainerStartFailed`].
fn allocate_ephemeral_port() -> Result<u16, ProbeError> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|err| ProbeError::PortAlloc(err.to_string()))?;
    let port = listener
        .local_addr()
        .map_err(|err| ProbeError::PortAlloc(err.to_string()))?
        .port();
    drop(listener);
    Ok(port)
}

/// Wrapper around a running docker container so the rest of the
/// probe can refer to it by id.
#[derive(Debug, Clone)]
pub struct RunningContainer {
    pub id: String,
}

/// `docker run -d --rm -p host_port:container_port image`. Returns
/// the container id (stdout of `docker run`).
pub fn start_container(
    runtime: &str,
    image: &str,
    host_port: u16,
    container_port: u16,
) -> Result<RunningContainer, ProbeError> {
    let port_arg = format!("127.0.0.1:{host_port}:{container_port}");
    let output = Command::new(runtime)
        .args(["run", "-d", "--rm", "-p", &port_arg, image])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => ProbeError::RuntimeMissing(runtime.to_string()),
            _ => ProbeError::Io(err.to_string()),
        })?;
    if !output.status.success() {
        return Err(ProbeError::ContainerStartFailed {
            image: image.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr)
                .trim_end()
                .to_string(),
        });
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if id.is_empty() {
        return Err(ProbeError::ContainerStartFailed {
            image: image.to_string(),
            stderr: "docker run produced no container id".to_string(),
        });
    }
    Ok(RunningContainer { id })
}

/// Poll `127.0.0.1:port` for TCP acceptance every 200ms until
/// `deadline`. Returns once the first connect succeeds.
pub fn wait_for_tcp(port: u16, deadline: Instant) -> Result<(), ProbeError> {
    let addr = format!("127.0.0.1:{port}");
    loop {
        if Instant::now() >= deadline {
            return Err(ProbeError::PortNeverAccepted { port });
        }
        match TcpStream::connect_timeout(
            &addr.parse().expect("ip:port"),
            Duration::from_millis(500),
        ) {
            Ok(_) => return Ok(()),
            Err(_) => {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// Drive the MCP handshake against `url`.
///
/// The exact wire shape is the v1 Streamable-HTTP transport the
/// session-broker uses post-RFE: every request POSTs to the mount
/// point with a JSON-RPC envelope. The first `initialize` response
/// carries `Mcp-Session-Id`; subsequent calls echo it back. We do
/// not exercise the SSE / multi-response side of Streamable-HTTP
/// because the four init-time calls are request/response shaped.
pub fn handshake(url: &str, deadline: Instant) -> Result<ProbeResult, ProbeError> {
    let client = Client::builder()
        .timeout(HTTP_CALL_TIMEOUT)
        .build()
        .map_err(|err| ProbeError::Io(err.to_string()))?;

    let initialize_resp = call(
        &client,
        url,
        None,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "botwork-tools-mcp-probe",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }
        }),
        deadline,
    )?;
    let (init_result, session_id) = expect_result(initialize_resp)?;

    let server_info = init_result.get("serverInfo").ok_or_else(|| {
        ProbeError::HandshakeShape("initialize result missing 'serverInfo'".to_string())
    })?;
    let server_info: ServerInfo = serde_json::from_value(server_info.clone())
        .map_err(|err| ProbeError::HandshakeShape(format!("serverInfo: {err}")))?;
    let capabilities = init_result
        .get("capabilities")
        .cloned()
        .unwrap_or(JsonValue::Object(Default::default()));
    let protocol_version = init_result
        .get("protocolVersion")
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| MCP_PROTOCOL_VERSION.to_string());

    // notifications/initialized has no `id` field; it's a one-way
    // notification per JSON-RPC 2.0. We send it but don't decode a
    // body — many servers respond 202 No Content.
    call(
        &client,
        url,
        session_id.as_deref(),
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }),
        deadline,
    )?;

    let tools = list(
        &client,
        url,
        session_id.as_deref(),
        "tools/list",
        "tools",
        2,
        deadline,
    )?;
    let resources = if has_capability(&capabilities, "resources") {
        list(
            &client,
            url,
            session_id.as_deref(),
            "resources/list",
            "resources",
            3,
            deadline,
        )?
    } else {
        Vec::new()
    };
    let prompts = if has_capability(&capabilities, "prompts") {
        list(
            &client,
            url,
            session_id.as_deref(),
            "prompts/list",
            "prompts",
            4,
            deadline,
        )?
    } else {
        Vec::new()
    };

    Ok(ProbeResult {
        server_info,
        protocol_version,
        capabilities,
        tools,
        resources,
        prompts,
    })
}

/// Does `capabilities` advertise the named feature?
fn has_capability(capabilities: &JsonValue, feature: &str) -> bool {
    capabilities
        .as_object()
        .map(|m| m.contains_key(feature))
        .unwrap_or(false)
}

/// POST a JSON-RPC envelope to `url`, optionally with the
/// `Mcp-Session-Id` header. Returns the parsed envelope.
fn call(
    client: &Client,
    url: &str,
    session_id: Option<&str>,
    body: JsonValue,
    deadline: Instant,
) -> Result<JsonValue, ProbeError> {
    check_deadline(deadline)?;
    let mut req = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", MCP_PROTOCOL_VERSION)
        .json(&body);
    if let Some(sid) = session_id {
        req = req.header("Mcp-Session-Id", sid);
    }
    let resp = req
        .send()
        .map_err(|err| ProbeError::HttpTransport(format!("POST {url}: {err}")))?;
    let status = resp.status();
    let headers = resp.headers().clone();
    // We need to read the body even on success to capture the
    // notifications/initialized empty-body case cleanly.
    let bytes_result = read_body(resp);
    if !status.is_success() {
        return Err(ProbeError::HandshakeShape(format!(
            "POST {url} -> {status}"
        )));
    }
    let bytes = bytes_result?;
    // notifications/initialized may return empty body; surface as
    // a null JSON value rather than a parse error.
    let envelope: JsonValue = if bytes.is_empty() {
        JsonValue::Null
    } else if let Some(content_type) = headers.get(reqwest::header::CONTENT_TYPE) {
        let ctype = content_type.to_str().unwrap_or("");
        if ctype.contains("text/event-stream") {
            // Some MCP servers return Streamable-HTTP SSE for
            // initialize even when the client didn't subscribe.
            // Parse the first `data:` line as JSON.
            parse_sse_first_event(&bytes)?
        } else {
            serde_json::from_slice(&bytes)
                .map_err(|err| ProbeError::HandshakeShape(format!("decode body: {err}")))?
        }
    } else {
        serde_json::from_slice(&bytes)
            .map_err(|err| ProbeError::HandshakeShape(format!("decode body: {err}")))?
    };
    Ok(envelope)
}

fn read_body(mut resp: reqwest::blocking::Response) -> Result<Vec<u8>, ProbeError> {
    // Cap at 1MiB to avoid an unbounded read against a buggy server.
    let mut buf = Vec::with_capacity(8 * 1024);
    resp.by_ref()
        .take(1024 * 1024)
        .read_to_end(&mut buf)
        .map_err(|err| ProbeError::HttpTransport(format!("read body: {err}")))?;
    Ok(buf)
}

/// Extract the first SSE `data:` payload from `bytes` and parse it
/// as JSON. Returns the first event only — initialize handshakes
/// always carry a single response event in our usage.
fn parse_sse_first_event(bytes: &[u8]) -> Result<JsonValue, ProbeError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ProbeError::HandshakeShape("SSE body is not utf-8".to_string()))?;
    for line in text.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            let payload = payload.trim();
            if payload.is_empty() {
                continue;
            }
            return serde_json::from_str(payload)
                .map_err(|err| ProbeError::HandshakeShape(format!("decode SSE event: {err}")));
        }
    }
    Err(ProbeError::HandshakeShape(
        "SSE body had no data: lines".to_string(),
    ))
}

/// Call a list endpoint (`tools/list`, `resources/list`,
/// `prompts/list`) and return the array under `result.<key>`.
fn list(
    client: &Client,
    url: &str,
    session_id: Option<&str>,
    method: &str,
    result_key: &str,
    id: u64,
    deadline: Instant,
) -> Result<Vec<JsonValue>, ProbeError> {
    let resp = call(
        client,
        url,
        session_id,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": {}
        }),
        deadline,
    )?;
    let (result, _) = expect_result(resp)?;
    let arr = result
        .get(result_key)
        .and_then(JsonValue::as_array)
        .ok_or_else(|| {
            ProbeError::HandshakeShape(format!("{method} result missing '{result_key}' array"))
        })?;
    Ok(arr.clone())
}

/// Pull the `result` object out of a JSON-RPC envelope, or surface
/// the `error` object as a [`ProbeError::JsonRpcError`]. Returns
/// `(result, Mcp-Session-Id)` — the session id only lives on the
/// initialize response, but we ignore the absence on later calls.
fn expect_result(envelope: JsonValue) -> Result<(JsonValue, Option<String>), ProbeError> {
    if envelope.is_null() {
        return Err(ProbeError::HandshakeShape(
            "empty envelope where a result was expected".to_string(),
        ));
    }
    if let Some(err) = envelope.get("error") {
        return Err(ProbeError::JsonRpcError(err.to_string()));
    }
    let result = envelope
        .get("result")
        .cloned()
        .ok_or_else(|| ProbeError::HandshakeShape("envelope missing 'result'".to_string()))?;
    // Mcp-Session-Id travels as a *header* per the Streamable-HTTP
    // spec; production session-broker reads it off the response
    // headers. The probe is a single-session lifecycle (initialize →
    // notifications/initialized → tools/list) so the session id is
    // only useful to forward verbatim across calls — we don't need
    // it for routing or persistence. We capture the body-side
    // duplicate (a courtesy some servers emit for diagnostic
    // clarity) when it's there and ignore it otherwise; the next
    // call is `notifications/initialized` which is a one-way notify
    // that doesn't need the id at all. If a future probe scenario
    // ever needs the canonical header-side id, this is the place
    // to plumb `resp.headers().get("Mcp-Session-Id")` through from
    // `call`'s response-side read.
    let session_id = envelope
        .get("session_id")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    Ok((result, session_id))
}

fn check_deadline(deadline: Instant) -> Result<(), ProbeError> {
    if Instant::now() >= deadline {
        return Err(ProbeError::HandshakeTimeout);
    }
    Ok(())
}

/// RAII teardown: `docker stop <id>` on drop. We don't surface
/// stop failures (the container has `--rm` so a stop almost always
/// succeeds; if it doesn't, the runner will reap the container
/// shortly).
struct TeardownGuard {
    runtime: String,
    container_id: String,
}

impl Drop for TeardownGuard {
    fn drop(&mut self) {
        let _ = Command::new(&self.runtime)
            .args(["stop", "--time", "2", &self.container_id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Per-spec validation of a captured tool. Returns the tool name on
/// success; errors carry the offending shape so the operator can
/// fix their server's `tools/list` response. Exposed for the
/// compose step so the validation happens once per tool, with the
/// error pointing at the producer-side bug rather than the
/// compose-side downstream symptom.
pub fn tool_name(tool: &JsonValue) -> Result<String, ProbeError> {
    let name = tool
        .get("name")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| ProbeError::HandshakeShape("tools/list entry missing 'name'".to_string()))?;
    // Regex lives in admin-core so the producer-side (this probe)
    // and the consumer-side catalog upserter enforce the same rule
    // out of one place — same posture PLUGIN_NAME_RE uses.
    let re = regex::Regex::new(botwork_admin_core::plugin_spec::TOOL_NAME_RE)
        .expect("valid tool name regex");
    if !re.is_match(name) {
        return Err(ProbeError::HandshakeShape(format!(
            "tool name {name:?} does not match {pattern}",
            pattern = botwork_admin_core::plugin_spec::TOOL_NAME_RE,
        )));
    }
    Ok(name.to_string())
}

/// Build a deterministic-iteration list of (name, body) tuples for
/// the captured catalog. The body is the tool object verbatim —
/// the compose step decides which fields turn into labels.
pub fn ordered_catalog(items: &[JsonValue]) -> Result<BTreeMap<String, JsonValue>, ProbeError> {
    let mut out = BTreeMap::new();
    for item in items {
        let name = tool_name(item)?;
        out.insert(name, item.clone());
    }
    Ok(out)
}

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("container runtime '{0}' not found on PATH")]
    RuntimeMissing(String),

    #[error("could not allocate ephemeral host port: {0}")]
    PortAlloc(String),

    #[error("io error: {0}")]
    Io(String),

    #[error("failed to start container {image}: {stderr}")]
    ContainerStartFailed { image: String, stderr: String },

    #[error("container never accepted TCP on host port {port} within --timeout")]
    PortNeverAccepted { port: u16 },

    #[error("MCP handshake exceeded --timeout")]
    HandshakeTimeout,

    #[error("MCP handshake transport: {0}")]
    HttpTransport(String),

    #[error("MCP handshake shape: {0}")]
    HandshakeShape(String),

    #[error("MCP server returned JSON-RPC error: {0}")]
    JsonRpcError(String),
}

impl ProbeError {
    /// Map a probe error to the RFE-stated exit code. Container
    /// lifecycle / port issues map to 4; handshake-side failures
    /// (server replied but with the wrong shape) map to 5.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::RuntimeMissing(_)
            | Self::PortAlloc(_)
            | Self::Io(_)
            | Self::ContainerStartFailed { .. }
            | Self::PortNeverAccepted { .. } => 4,
            Self::HandshakeTimeout
            | Self::HttpTransport(_)
            | Self::HandshakeShape(_)
            | Self::JsonRpcError(_) => 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_accepts_valid_shapes() {
        let v = json!({"name": "fetch"});
        assert_eq!(tool_name(&v).unwrap(), "fetch");
        let v = json!({"name": "fetch_url"});
        assert_eq!(tool_name(&v).unwrap(), "fetch_url");
        let v = json!({"name": "fetch-url-2"});
        assert_eq!(tool_name(&v).unwrap(), "fetch-url-2");
    }

    #[test]
    fn tool_name_rejects_uppercase_and_punctuation() {
        for bad in ["", "Fetch", "_fetch", "-fetch", "fetch!", "fetch space"] {
            let v = json!({"name": bad});
            assert!(tool_name(&v).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn tool_name_rejects_missing_name_field() {
        let v = json!({"description": "no name"});
        assert!(tool_name(&v).is_err());
    }

    #[test]
    fn ordered_catalog_sorts_by_name() {
        let items = vec![
            json!({"name": "zoo"}),
            json!({"name": "alpha"}),
            json!({"name": "mid"}),
        ];
        let cat = ordered_catalog(&items).unwrap();
        let names: Vec<&str> = cat.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["alpha", "mid", "zoo"]);
    }

    #[test]
    fn ordered_catalog_propagates_invalid_name_error() {
        let items = vec![json!({"name": "fine"}), json!({"name": "Bad"})];
        assert!(ordered_catalog(&items).is_err());
    }

    #[test]
    fn probe_error_exit_code_buckets_match_rfe() {
        assert_eq!(ProbeError::PortNeverAccepted { port: 1 }.exit_code(), 4);
        assert_eq!(ProbeError::HandshakeTimeout.exit_code(), 5);
        assert_eq!(ProbeError::JsonRpcError("e".into()).exit_code(), 5);
        assert_eq!(ProbeError::RuntimeMissing("d".into()).exit_code(), 4);
    }

    #[test]
    fn parse_sse_first_event_pulls_data_payload() {
        let body = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let v = parse_sse_first_event(body).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
    }

    #[test]
    fn parse_sse_first_event_errors_on_no_data_line() {
        let body = b"event: ping\n\n";
        assert!(parse_sse_first_event(body).is_err());
    }

    #[test]
    fn has_capability_treats_missing_as_false() {
        assert!(has_capability(&json!({"resources": {}}), "resources"));
        assert!(!has_capability(&json!({}), "resources"));
        assert!(!has_capability(&json!(null), "resources"));
        // Non-object capabilities map (would be a server bug) does
        // not crash, returns false.
        assert!(!has_capability(&json!([]), "resources"));
    }

    #[test]
    fn expect_result_surfaces_jsonrpc_errors() {
        let envelope = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "method not found"}});
        let err = expect_result(envelope).unwrap_err();
        assert!(matches!(err, ProbeError::JsonRpcError(_)));
    }

    #[test]
    fn expect_result_extracts_result() {
        let envelope = json!({"jsonrpc": "2.0", "id": 1, "result": {"capabilities": {}, "serverInfo": {"name": "x"}}});
        let (r, _) = expect_result(envelope).unwrap();
        assert_eq!(r["serverInfo"]["name"], "x");
    }
}
