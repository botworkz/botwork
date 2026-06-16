//! Unix socket server that receives container-exit events from the launcher and
//! invalidates the corresponding `transport_sessions` entry.
//!
//! The launcher's `events` module subscribes to `docker events` and calls
//! `POST /container-exit` on this socket whenever an `mcp_session_*` container
//! exits.  The broker then drops the stale routing entry, tombstones the
//! `Mcp-Session-Id`, and calls the launcher teardown helper to clean staging
//! mounts.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::UnixListener;
use tokio::time::timeout;

use crate::launcher::call_teardown;
use crate::{log_info, AppState, TOMBSTONE_TTL};

pub async fn serve_exit_listener(state: AppState, socket_path: &str) -> Result<(), String> {
    match std::fs::remove_file(socket_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!(
                "failed to remove existing broker socket {socket_path}: {e}"
            ));
        }
    }

    // Create parent directory if needed
    if let Some(parent) = std::path::Path::new(socket_path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "failed to create broker socket directory {}: {e}",
                    parent.display()
                )
            })?;
        }
    }

    let listener = UnixListener::bind(socket_path)
        .map_err(|e| format!("failed to bind broker unix socket {socket_path}: {e}"))?;

    log_info(&format!("exit listener bound on unix://{socket_path}"));

    let state = Arc::new(state);

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                log_info(&format!("exit listener accept error: {e}"));
                continue;
            }
        };

        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| handle_exit_request(req, Arc::clone(&state)));
            if let Err(e) = http1::Builder::new()
                .keep_alive(false)
                .serve_connection(io, service)
                .await
            {
                log_info(&format!("exit listener connection error: {e}"));
            }
        });
    }
}

async fn handle_exit_request(
    request: Request<Incoming>,
    state: Arc<AppState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let response = match dispatch_exit_request(request, &state).await {
        Ok(response) => response,
        Err(e) => {
            log_info(&format!("exit listener request error: {e}"));
            json_response(StatusCode::INTERNAL_SERVER_ERROR, r#"{"status":"error"}"#)
        }
    };
    Ok(response)
}

async fn dispatch_exit_request(
    request: Request<Incoming>,
    state: &AppState,
) -> Result<Response<Full<Bytes>>, String> {
    if request.method() != Method::POST || request.uri().path() != "/container-exit" {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            r#"{"status":"not_found"}"#,
        ));
    }

    let body_bytes = timeout(Duration::from_secs(5), request.into_body().collect())
        .await
        .map_err(|_| "timeout reading request body".to_string())?
        .map_err(|e| format!("failed to read request body: {e}"))?
        .to_bytes();

    let payload: serde_json::Value =
        serde_json::from_slice(&body_bytes).map_err(|e| format!("invalid JSON body: {e}"))?;

    let name = payload
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing 'name' field".to_string())?
        .to_string();
    let event = payload
        .get("event")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let exit_code = payload.get("exit_code").and_then(|v| v.as_i64());

    handle_container_exit(state, &name, event, exit_code).await
}

/// Core handler for a container-exit event. Exported for direct unit testing.
///
/// Looks up `container_name` in `state.transport_sessions` (reverse scan),
/// tombstones the session, removes the transport entry, records teardown in the
/// session registry, and best-effort-calls the launcher teardown helper for
/// staging mount cleanup.  Returns `404` if `container_name` has no active
/// transport session.
pub async fn handle_container_exit(
    state: &AppState,
    container_name: &str,
    event: &str,
    exit_code: Option<i64>,
) -> Result<Response<Full<Bytes>>, String> {
    // Reverse-scan transport_sessions: container_name → mcp_session_id
    let teardown_info = {
        let sessions = state.transport_sessions.lock().await;
        sessions.iter().find_map(|(mcp_session_id, transport)| {
            if transport.container_name == container_name {
                Some((
                    mcp_session_id.clone(),
                    transport.staging_token.clone(),
                    transport.tenant_name.clone(),
                ))
            } else {
                None
            }
        })
    };

    let (mcp_session_id, staging_token, tenant_name) = match teardown_info {
        Some(info) => info,
        None => {
            log_info(&format!(
                "exit_listener: unknown container={container_name} event={event} (no active transport session)"
            ));
            return Ok(json_response(
                StatusCode::NOT_FOUND,
                r#"{"status":"unknown"}"#,
            ));
        }
    };

    let exit_code_display = exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "null".to_string());
    log_info(&format!(
        "exit_listener: dropped session={mcp_session_id} container={container_name} event={event} exit_code={exit_code_display}"
    ));

    let staging_path = format!("/var/lib/botwork/tenants/{tenant_name}/staging/{staging_token}");

    // Tombstone before removing transport entry so any concurrent request sees
    // the tombstone immediately after the lock is released.
    {
        let mut tombstones = state.tombstones.lock().await;
        tombstones.insert(
            mcp_session_id.clone(),
            std::time::Instant::now() + TOMBSTONE_TTL,
        );
    }
    {
        let mut sessions = state.transport_sessions.lock().await;
        sessions.remove(&mcp_session_id);
    }

    state.session_registry.record_teardown(container_name).await;

    // Best-effort: container is already gone, but the launcher still needs to
    // unmount the staging directory.  Fire and forget to not block the response.
    let launcher_path = state.launcher_socket_path.clone();
    let container_name_owned = container_name.to_string();
    tokio::spawn(async move {
        call_teardown(&launcher_path, &container_name_owned, &staging_path).await;
    });

    Ok(json_response(StatusCode::OK, r#"{"status":"ok"}"#))
}

fn json_response(status: StatusCode, body: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("response builder")
}
