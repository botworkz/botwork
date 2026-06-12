use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http1;
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::{TcpStream, UnixStream};
use tokio::time::timeout;

use crate::log_info;
use crate::plugin_registry::PluginResources;
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
}

impl LauncherError {
    pub fn status_code(&self) -> u32 {
        match self {
            LauncherError::ProbeTimeout { .. } => 504,
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

pub async fn launch_session(
    socket_path: &str,
    name: &str,
    image: &str,
    staging_path: &str,
    network: &str,
    env: &[(String, String)],
    resources: &PluginResources,
) -> Result<Value, LauncherError> {
    let mut payload = serde_json::Map::from_iter([
        ("name".to_string(), Value::String(name.to_string())),
        ("image".to_string(), Value::String(image.to_string())),
        ("network".to_string(), Value::String(network.to_string())),
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

    serde_json::from_slice(&body).map_err(|_| LauncherError::LaunchInvalidJson)
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
            ("BOTWORK_SECRET_A".to_string(), "1".to_string()),
            ("BOTWORK_SECRET_B".to_string(), "2".to_string()),
        ];
        let body = capture_launch_body(&env, &PluginResources::default()).await;
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("json body");
        let names: Vec<&str> = parsed["env"]
            .as_array()
            .expect("env array")
            .iter()
            .map(|entry| entry["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, vec!["BOTWORK_SECRET_A", "BOTWORK_SECRET_B"]);
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

    async fn capture_launch_body(env: &[(String, String)], resources: &PluginResources) -> String {
        let temp = tempdir().expect("tempdir");
        let socket_path = temp.path().join("launcher.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind unix socket");

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let request = read_http_request(&mut stream).await;
            let body = request
                .split("\r\n\r\n")
                .nth(1)
                .expect("request body")
                .to_string();
            let response = r#"HTTP/1.1 200 OK
Content-Type: application/json
Content-Length: 20
Connection: close

{"status":"started"}"#;
            let response = response.replace('\n', "\r\n");
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
            body
        });

        let _ = launch_session(
            path_to_string(&socket_path).as_str(),
            "mcp_session_abc",
            "botwork/mcp-a:local",
            "/tmp/staging",
            "botwork",
            env,
            resources,
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
}
