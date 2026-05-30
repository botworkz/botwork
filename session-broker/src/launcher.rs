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
) -> Result<Value, LauncherError> {
    let (status, body) = launcher_post(
        socket_path,
        "/launch",
        serde_json::json!({
            "name": name,
            "image": image,
            "network": network,
            "staging_path": staging_path,
        }),
        Duration::from_secs(5),
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
