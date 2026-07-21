use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http1;
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::{TcpStream, UnixStream};
use tokio::time::timeout;

use crate::config_broker::PluginResources;
use crate::log_info;
use crate::PROBE_SLEEP;

#[derive(Debug, thiserror::Error)]
pub enum LauncherError {
    #[error("failed to contact launcher: {0}")]
    Launch(String),
    #[error("bind-agent conflict: {0}")]
    BindConflict(String),
    #[error("bind-agent returned HTTP {status}: {detail}")]
    BindHttp { status: u16, detail: String },
    #[error("launcher returned HTTP {status}: {detail}")]
    LaunchHttp { status: u16, detail: String },
    #[error("launcher returned invalid JSON")]
    LaunchInvalidJson,
    #[error("timed out waiting for {host}:{port} to become ready")]
    ProbeTimeout { host: String, port: u16 },
    /// Hard-gate denial: the spawned container reached probe-ready, but
    /// control-plane refused (or failed) to record it. The container
    /// has been torn down already; this variant exists so the
    /// request-headers path can surface a distinct 503 to the client
    /// rather than a generic 502.
    #[error("control-plane denied session: {0}")]
    ControlPlane(crate::control_plane::ControlPlaneError),
}

impl LauncherError {
    pub fn status_code(&self) -> u32 {
        match self {
            LauncherError::ProbeTimeout { .. } => 504,
            LauncherError::ControlPlane(err) => err.status_code(),
            _ => 502,
        }
    }
}

pub async fn launcher_post(
    socket_path: &str,
    path: &str,
    payload: Value,
    request_timeout: Duration,
) -> Result<(u16, Vec<u8>), LauncherError> {
    let body = serde_json::to_vec(&payload)
        .map_err(|e| LauncherError::Launch(format!("failed to encode launcher payload: {e}")))?;

    let stream = timeout(request_timeout, UnixStream::connect(socket_path))
        .await
        .map_err(|e| LauncherError::Launch(e.to_string()))?
        .map_err(|e| LauncherError::Launch(e.to_string()))?;
    let io = TokioIo::new(stream);
    let (mut sender, conn) = http1::handshake(io)
        .await
        .map_err(|e| LauncherError::Launch(e.to_string()))?;
    tokio::spawn(async move {
        if let Err(err) = conn.await {
            log_info(&format!("launcher HTTP connection error: {err}"));
        }
    });

    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://localhost{path}"))
        .header("Content-Type", "application/json")
        .header("Content-Length", body.len().to_string())
        .body(Full::new(Bytes::from(body)))
        .map_err(|e| LauncherError::Launch(e.to_string()))?;

    let response = timeout(request_timeout, sender.send_request(req))
        .await
        .map_err(|e| LauncherError::Launch(e.to_string()))?
        .map_err(|e| LauncherError::Launch(e.to_string()))?;
    let status = response.status().as_u16();
    let response_body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| LauncherError::Launch(e.to_string()))?
        .to_bytes()
        .to_vec();

    Ok((status, response_body))
}

/// Outcome of a successful `POST /launch` to the launcher socket.
///
/// `container_ip` is required since 0.1.5 -- the launcher refuses to
/// return 200 without an IP, and the call site treats a missing IP as
/// a launch failure. `raw` is the verbatim launcher response, kept so
/// the spawn path's existing logging continues to surface unknown
/// fields.
#[cfg_attr(test, derive(Debug))]
pub struct LaunchOutcome {
    pub container_ip: String,
    pub raw: Value,
}

/// Per-session parameters for [`launch_session`].
pub struct LaunchRequest<'a> {
    pub name: &'a str,
    pub image: &'a str,
    pub staging_path: &'a str,
    pub env: &'a [(String, String)],
    pub secrets: &'a [(String, String)],
    pub resources: &'a PluginResources,
    pub labels: &'a [(String, String)],
}

pub async fn launch_session(
    socket_path: &str,
    req: LaunchRequest<'_>,
) -> Result<LaunchOutcome, LauncherError> {
    // network is intentionally not threaded from session-broker into the
    // launcher payload (post-0.1.4). The launcher resolves it from its own
    // BOTWORK_LAUNCHER_DEFAULT_NETWORK env, because "which docker network
    // do plugin containers belong to" is a deploy-topology concern owned
    // by the launcher's systemd unit, not a per-plugin-registry setting.
    let LaunchRequest {
        name,
        image,
        staging_path,
        env,
        secrets,
        resources,
        labels,
    } = req;
    let mut payload = serde_json::Map::from_iter([
        ("name".to_string(), Value::String(name.to_string())),
        ("image".to_string(), Value::String(image.to_string())),
        (
            "staging_path".to_string(),
            Value::String(staging_path.to_string()),
        ),
    ]);
    if !env.is_empty() {
        payload.insert(
            "env".to_string(),
            Value::Array(
                env.iter()
                    .map(|(name, value)| {
                        serde_json::json!({
                            "name": name,
                            "value": value,
                        })
                    })
                    .collect(),
            ),
        );
    }
    // Raw secret pairs are sent on a dedicated `secrets` field, separate from
    // `env`.  The launcher writes them to a container-local tmpfs as files and
    // injects only `*_FILE` path pointers into the container env.  Secret
    // values must never appear in the `env` array.
    if !secrets.is_empty() {
        payload.insert(
            "secrets".to_string(),
            Value::Array(
                secrets
                    .iter()
                    .map(|(name, value)| {
                        serde_json::json!({
                            "name": name,
                            "value": value,
                        })
                    })
                    .collect(),
            ),
        );
    }
    if resources.cpus.is_some() || resources.memory.is_some() || resources.pids.is_some() {
        let mut resources_payload = serde_json::Map::new();
        if let Some(cpus) = &resources.cpus {
            resources_payload.insert("cpus".to_string(), Value::String(cpus.to_string()));
        }
        if let Some(memory) = &resources.memory {
            resources_payload.insert("memory".to_string(), Value::String(memory.to_string()));
        }
        if let Some(pids) = resources.pids {
            resources_payload.insert("pids".to_string(), Value::Number(pids.into()));
        }
        payload.insert("resources".to_string(), Value::Object(resources_payload));
    }
    // RFE #105 round-3: pass container labels through to the
    // launcher. The launcher's wire validator (#115) enforces the
    // `io.botworkz.*` namespace + value shape, so the broker writes
    // the entries verbatim. Empty slice ⇒ omit the field entirely
    // for parity with the env slice (older launcher images stay
    // wire-compatible, though the new schema flow requires the
    // newer launcher).
    if !labels.is_empty() {
        payload.insert(
            "labels".to_string(),
            Value::Array(
                labels
                    .iter()
                    .map(|(name, value)| {
                        serde_json::json!({
                            "name": name,
                            "value": value,
                        })
                    })
                    .collect(),
            ),
        );
    }
    let (status, body) = launcher_post(
        socket_path,
        "/launch",
        Value::Object(payload),
        // Keep extra slack here: launcher work is still the critical path and can be slow on CI hosts.
        Duration::from_secs(15),
    )
    .await?;

    if status != 200 {
        let detail = String::from_utf8_lossy(&body).trim().to_string();
        return Err(LauncherError::LaunchHttp { status, detail });
    }

    let raw: Value = serde_json::from_slice(&body).map_err(|_| LauncherError::LaunchInvalidJson)?;
    let container_ip = raw
        .get("container_ip")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            // 0.1.5 contract: the launcher always populates this. A
            // missing field means we are pointing at a stale launcher
            // (and would otherwise silently drop a session-without-IP
            // on the floor at control-plane gate time).
            LauncherError::LaunchHttp {
                status: 200,
                detail: "launcher response missing required 'container_ip' field".to_string(),
            }
        })?
        .to_string();
    if container_ip.parse::<std::net::Ipv4Addr>().is_err() {
        return Err(LauncherError::LaunchHttp {
            status: 200,
            detail: format!("launcher response has non-IPv4 'container_ip': {container_ip:?}"),
        });
    }

    Ok(LaunchOutcome { container_ip, raw })
}

pub async fn call_bind_agent(
    socket_path: &str,
    staging_path: &str,
    agent_dir: &str,
) -> Result<(), LauncherError> {
    let (status, body) = launcher_post(
        socket_path,
        "/bind-agent",
        serde_json::json!({
            "staging_path": staging_path,
            "agent_dir": agent_dir,
        }),
        Duration::from_secs(5),
    )
    .await?;

    if status == 409 {
        return Err(LauncherError::BindConflict(
            String::from_utf8_lossy(&body).trim().to_string(),
        ));
    }
    if status != 200 {
        return Err(LauncherError::BindHttp {
            status,
            detail: String::from_utf8_lossy(&body).trim().to_string(),
        });
    }
    Ok(())
}

pub async fn call_teardown(socket_path: &str, name: &str, staging_path: &str) {
    let _ = launcher_post(
        socket_path,
        "/teardown",
        serde_json::json!({
            "name": name,
            "staging_path": staging_path,
        }),
        Duration::from_secs(5),
    )
    .await;
}

pub async fn probe_ready(
    host: &str,
    port: u16,
    timeout_per_attempt: Duration,
    total_timeout: Duration,
) -> bool {
    let deadline = Instant::now() + total_timeout;
    while Instant::now() < deadline {
        let connect = timeout(timeout_per_attempt, TcpStream::connect((host, port))).await;
        if let Ok(Ok(stream)) = connect {
            drop(stream);
            return true;
        }
        tokio::time::sleep(PROBE_SLEEP).await;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};

    #[tokio::test]
    async fn launcher_post_omits_env_when_slice_empty() {
        let body = capture_launch_body(&[], &PluginResources::default()).await;
        assert!(!body.contains("\"env\""));
    }

    #[tokio::test]
    async fn launcher_post_includes_env_when_slice_non_empty_in_order() {
        let env = vec![
            ("GITHUB_TOOLSETS".to_string(), "default,actions".to_string()),
            ("BOTWORK_MCP_CONFIG".to_string(), "{}".to_string()),
        ];
        let body = capture_launch_body(&env, &PluginResources::default()).await;
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("json body");
        let names: Vec<&str> = parsed["env"]
            .as_array()
            .expect("env array")
            .iter()
            .map(|entry| entry["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, vec!["GITHUB_TOOLSETS", "BOTWORK_MCP_CONFIG"]);
        // The `env` array must NOT contain any BOTWORK_SECRET_* entries;
        // secrets go on the dedicated `secrets` field.
        assert!(
            !body.contains("BOTWORK_SECRET_"),
            "env array must not contain BOTWORK_SECRET_ entries: {body}"
        );
    }

    #[tokio::test]
    async fn launcher_post_omits_secrets_when_slice_empty() {
        let body = capture_launch_body(&[], &PluginResources::default()).await;
        assert!(
            !body.contains("\"secrets\""),
            "secrets field must be absent when slice is empty: {body}"
        );
    }

    #[tokio::test]
    async fn launcher_post_includes_secrets_on_dedicated_field() {
        let secrets = vec![
            ("GITHUB_COM_PAT".to_string(), "ghp_abc".to_string()),
            ("SLACK_DEFAULT_TOKEN".to_string(), "xoxb-xyz".to_string()),
        ];
        let body =
            capture_launch_body_with_secrets(&[], &secrets, &PluginResources::default(), &[]).await;
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("json body");

        // Secrets must appear on the `secrets` field.
        let secret_names: Vec<&str> = parsed["secrets"]
            .as_array()
            .expect("secrets array")
            .iter()
            .map(|entry| entry["name"].as_str().expect("name"))
            .collect();
        assert_eq!(secret_names, vec!["GITHUB_COM_PAT", "SLACK_DEFAULT_TOKEN"]);

        // The `env` field must NOT contain any BOTWORK_SECRET_ entries.
        // (It may be absent entirely, or present with only non-secret entries.)
        if let Some(env_arr) = parsed["env"].as_array() {
            for entry in env_arr {
                let name = entry["name"].as_str().unwrap_or("");
                assert!(
                    !name.starts_with("BOTWORK_SECRET_"),
                    "env array must not contain BOTWORK_SECRET_* entries, found: {name}"
                );
            }
        }
    }

    #[tokio::test]
    async fn launcher_post_omits_resources_when_unset() {
        let body = capture_launch_body(&[], &PluginResources::default()).await;
        assert!(!body.contains("\"resources\""));
    }

    #[tokio::test]
    async fn launcher_post_includes_resources_when_set() {
        let resources = PluginResources {
            cpus: Some("4.0".to_string()),
            memory: Some("4g".to_string()),
            pids: Some(1024),
        };
        let body = capture_launch_body(&[], &resources).await;
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("json body");
        assert_eq!(parsed["resources"]["cpus"], "4.0");
        assert_eq!(parsed["resources"]["memory"], "4g");
        assert_eq!(parsed["resources"]["pids"], 1024);
    }

    #[tokio::test]
    async fn launcher_post_omits_network_field() {
        // Post-0.1.4: session-broker never sets `network` in the launch
        // payload — the launcher resolves it from its configured default.
        // This is a wire-contract test: a regression that re-introduced
        // `"network":` would silently re-couple session-broker to a
        // deploy-topology decision it shouldn't own.
        let body = capture_launch_body(&[], &PluginResources::default()).await;
        assert!(
            !body.contains("\"network\""),
            "launch payload must not include 'network': {body}"
        );
    }

    async fn capture_launch_body(env: &[(String, String)], resources: &PluginResources) -> String {
        let temp_dir = tempdir().expect("tempdir");
        let socket_path = temp_dir.path().join("launcher.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind unix socket");

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let request = read_http_request(&mut stream).await;
            let body = request
                .split("\r\n\r\n")
                .nth(1)
                .expect("request body")
                .to_string();
            // 0.1.5 wire shape: launcher always populates `container_ip`.
            // The fake response must include it; otherwise the new
            // launch_session decoder rejects it as a stale launcher.
            let body_json =
                r#"{"name":"mcp_session_abc","status":"started","container_ip":"172.20.0.5"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\nContent-Type: application/json\nContent-Length: {}\nConnection: close\n\n{body_json}",
                body_json.len()
            );
            let response = response.replace('\n', "\r\n");
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
            body
        });

        let _ = launch_session(
            path_to_string(&socket_path).as_str(),
            LaunchRequest {
                name: "mcp_session_abc",
                image: "botwork/mcp-a:local",
                staging_path: "/tmp/staging",
                env,
                secrets: &[],
                resources,
                // Test helper exercises the spawn path with the
                // pre-round-3-PR2 shape (no labels); the labels-emitted
                // case is covered by `launcher_post_includes_labels_when_slice_non_empty`
                // below.
                labels: &[],
            },
        )
        .await
        .expect("launch succeeds");
        server.await.expect("server result")
    }

    // ── RFE #105 round-3 PR2: labels on the launcher payload ───────────────

    #[tokio::test]
    async fn launcher_post_omits_labels_when_slice_empty() {
        let body = capture_launch_body(&[], &PluginResources::default()).await;
        assert!(
            !body.contains("\"labels\""),
            "labels field must be absent when slice is empty: {body}"
        );
    }

    #[tokio::test]
    async fn launcher_post_includes_labels_when_slice_non_empty() {
        let labels = vec![
            ("io.botworkz.tenant".to_string(), "acme".to_string()),
            ("io.botworkz.workspace".to_string(), "mcp".to_string()),
            ("io.botworkz.plugin".to_string(), "mcp-bash".to_string()),
        ];
        let body = capture_launch_body_with_labels(&[], &PluginResources::default(), &labels).await;
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("json body");
        let entries = parsed["labels"].as_array().expect("labels array");
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().expect("name"))
            .collect();
        let values: Vec<&str> = entries
            .iter()
            .map(|e| e["value"].as_str().expect("value"))
            .collect();
        // Order is preserved across the wire so the launcher's argv
        // (and the resulting `docker inspect`) stay deterministic.
        assert_eq!(
            names,
            vec![
                "io.botworkz.tenant",
                "io.botworkz.workspace",
                "io.botworkz.plugin",
            ]
        );
        assert_eq!(values, vec!["acme", "mcp", "mcp-bash"]);
    }

    async fn capture_launch_body_with_labels(
        env: &[(String, String)],
        resources: &PluginResources,
        labels: &[(String, String)],
    ) -> String {
        capture_launch_body_with_secrets(env, &[], resources, labels).await
    }

    async fn capture_launch_body_with_secrets(
        env: &[(String, String)],
        secrets: &[(String, String)],
        resources: &PluginResources,
        labels: &[(String, String)],
    ) -> String {
        let temp_dir = tempdir().expect("tempdir");
        let socket_path = temp_dir.path().join("launcher.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind unix socket");

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let request = read_http_request(&mut stream).await;
            let body = request
                .split("\r\n\r\n")
                .nth(1)
                .expect("request body")
                .to_string();
            let body_json =
                r#"{"name":"mcp_session_abc","status":"started","container_ip":"172.20.0.5"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\nContent-Type: application/json\nContent-Length: {}\nConnection: close\n\n{body_json}",
                body_json.len()
            );
            let response = response.replace('\n', "\r\n");
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
            body
        });

        let _ = launch_session(
            path_to_string(&socket_path).as_str(),
            LaunchRequest {
                name: "mcp_session_abc",
                image: "botwork/mcp-a:local",
                staging_path: "/tmp/staging",
                env,
                secrets,
                resources,
                labels,
            },
        )
        .await
        .expect("launch succeeds");
        server.await.expect("server result")
    }

    fn path_to_string(path: &Path) -> String {
        path.to_string_lossy().to_string()
    }

    async fn read_http_request(stream: &mut UnixStream) -> String {
        let mut raw = Vec::new();
        let mut buf = [0_u8; 1024];
        let mut expected_total = None;
        loop {
            let read = stream.read(&mut buf).await.expect("read request");
            if read == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..read]);
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

    /// Spin up a fake unix socket server that reads the client request (to
    /// unblock the sender) and writes back `http_response`, then run
    /// `launch_session` against it.  Returns the result of `launch_session`.
    async fn launch_session_with_fake_response(
        http_response: &str,
    ) -> Result<LaunchOutcome, LauncherError> {
        let temp_dir = tempdir().expect("tempdir");
        let socket_path = temp_dir.path().join("launcher.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        let resp = http_response.to_string();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            // drain the incoming request so the client's write doesn't stall
            let mut buf = [0_u8; 4096];
            let _ = stream.read(&mut buf).await;
            stream.write_all(resp.as_bytes()).await.expect("write");
        });
        let result = launch_session(
            path_to_string(&socket_path).as_str(),
            LaunchRequest {
                name: "mcp_session_abc",
                image: "botwork/mcp-a:local",
                staging_path: "/tmp/staging",
                env: &[],
                secrets: &[],
                resources: &PluginResources::default(),
                labels: &[],
            },
        )
        .await;
        let _ = server.await;
        result
    }

    /// Same helper for `call_bind_agent`.
    async fn bind_agent_with_fake_response(http_response: &str) -> Result<(), LauncherError> {
        let temp_dir = tempdir().expect("tempdir");
        let socket_path = temp_dir.path().join("bind.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        let resp = http_response.to_string();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = [0_u8; 4096];
            let _ = stream.read(&mut buf).await;
            stream.write_all(resp.as_bytes()).await.expect("write");
        });
        let result = call_bind_agent(
            path_to_string(&socket_path).as_str(),
            "/tmp/staging",
            "/tmp/agent",
        )
        .await;
        let _ = server.await;
        result
    }

    // ── LauncherError::status_code ────────────────────────────────────────────

    #[test]
    fn launcher_error_status_code_probe_timeout_is_504() {
        let err = LauncherError::ProbeTimeout {
            host: "host".to_string(),
            port: 8080,
        };
        assert_eq!(err.status_code(), 504);
    }

    #[test]
    fn launcher_error_status_code_other_variants_are_502() {
        assert_eq!(LauncherError::Launch("x".to_string()).status_code(), 502);
        assert_eq!(
            LauncherError::LaunchHttp {
                status: 503,
                detail: String::new(),
            }
            .status_code(),
            502
        );
        assert_eq!(LauncherError::LaunchInvalidJson.status_code(), 502);
        assert_eq!(
            LauncherError::BindConflict("x".to_string()).status_code(),
            502
        );
        assert_eq!(
            LauncherError::BindHttp {
                status: 500,
                detail: String::new(),
            }
            .status_code(),
            502
        );
    }

    // ── launch_session error paths ────────────────────────────────────────────

    #[tokio::test]
    async fn launch_session_non_200_returns_launch_http_error() {
        let resp =
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let err = launch_session_with_fake_response(resp)
            .await
            .expect_err("expected error");
        assert!(
            matches!(err, LauncherError::LaunchHttp { status: 503, .. }),
            "expected LaunchHttp{{503}}, got {err:?}"
        );
    }

    #[tokio::test]
    async fn launch_session_invalid_json_body_returns_launch_invalid_json() {
        let body = b"not-json";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let err = launch_session_with_fake_response(&resp)
            .await
            .expect_err("expected error");
        assert!(
            matches!(err, LauncherError::LaunchInvalidJson),
            "expected LaunchInvalidJson, got {err:?}"
        );
    }

    #[tokio::test]
    async fn launch_session_missing_container_ip_returns_launch_http_error() {
        let body = r#"{"name":"abc","status":"started"}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let err = launch_session_with_fake_response(&resp)
            .await
            .expect_err("expected error");
        assert!(
            matches!(
                err,
                LauncherError::LaunchHttp {
                    status: 200,
                    ref detail,
                } if detail.contains("container_ip")
            ),
            "expected LaunchHttp{{200}} mentioning container_ip, got {err:?}"
        );
    }

    #[tokio::test]
    async fn launch_session_non_ipv4_container_ip_returns_launch_http_error() {
        let body = r#"{"name":"abc","status":"started","container_ip":"not-an-ip"}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let err = launch_session_with_fake_response(&resp)
            .await
            .expect_err("expected error");
        assert!(
            matches!(
                err,
                LauncherError::LaunchHttp {
                    status: 200,
                    ref detail,
                } if detail.contains("non-IPv4")
            ),
            "expected LaunchHttp{{200}} mentioning non-IPv4, got {err:?}"
        );
    }

    // ── call_bind_agent error paths ───────────────────────────────────────────

    #[tokio::test]
    async fn call_bind_agent_409_returns_bind_conflict() {
        let resp =
            "HTTP/1.1 409 Conflict\r\nContent-Length: 7\r\nConnection: close\r\n\r\nconflict";
        let err = bind_agent_with_fake_response(resp)
            .await
            .expect_err("expected error");
        assert!(
            matches!(err, LauncherError::BindConflict(_)),
            "expected BindConflict, got {err:?}"
        );
    }

    #[tokio::test]
    async fn call_bind_agent_non_200_non_409_returns_bind_http() {
        let resp =
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\nConnection: close\r\n\r\nerror";
        let err = bind_agent_with_fake_response(resp)
            .await
            .expect_err("expected error");
        assert!(
            matches!(err, LauncherError::BindHttp { status: 500, .. }),
            "expected BindHttp{{500}}, got {err:?}"
        );
    }
}
