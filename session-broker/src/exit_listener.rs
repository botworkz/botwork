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
use tracing::warn;

use crate::control_plane;
use crate::ext_proc::liveness_remove;
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
    // Cancel any pending grace timer so the container-exit and stream-close
    // reapers don't race each other.
    liveness_remove(state, &mcp_session_id).await;

    // RFE #105 round-3 PR2: mark the session_worker row reaped. The
    // record_reap path is no-op-if-already-reaped (the teardown_session
    // path and the docker-exit path can both converge on the same
    // container), so concurrent invocation is safe.
    if let Some(writer) = state.session_worker_writer.as_ref() {
        if let Err(err) = writer.record_reap(container_name).await {
            warn!(
                "[session-broker] exit_listener: session_worker reap failed for \
                 container={container_name}: {err}"
            );
        }
    }

    // Best-effort: container is already gone, but the launcher still needs to
    // unmount the staging directory.  Fire and forget to not block the response.
    let launcher_path = state.launcher_socket_path.clone();
    let container_name_owned = container_name.to_string();
    tokio::spawn(async move {
        call_teardown(&launcher_path, &container_name_owned, &staging_path).await;
    });

    // Best-effort: tell control-plane the session is gone. A failure
    // here is logged but ignored -- the container has already exited,
    // and control-plane's cold-start recovery sync (control-plane polls
    // session-broker `/sessions` on restart) will reconcile any drift.
    // Blocking exit cleanup on control-plane reachability would punish
    // the steady-state cleanup path for a transient outage the system
    // heals on its own. See session-broker::control_plane module docs.
    let control_plane_endpoint = state.control_plane_endpoint.clone();
    let session_id_owned = mcp_session_id.clone();
    tokio::spawn(async move {
        if let Err(err) = control_plane::delete_session(
            &control_plane_endpoint,
            &session_id_owned,
            std::time::Duration::from_secs(5),
        )
        .await
        {
            log_info(&format!(
                "control-plane delete failed (non-fatal) session_id={session_id_owned}: {err}"
            ));
        }
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tokio::sync::Mutex;

    use super::*;
    use crate::{AppState, TransportState, UpstreamAuth};

    fn bare_state() -> AppState {
        AppState {
            transport_sessions: Arc::new(Mutex::new(HashMap::new())),
            pending_init: Arc::new(Mutex::new(HashMap::new())),
            launcher_socket_path: "/tmp/exit-listener-unit-launcher.sock".to_string(),
            auth_broker_url: "http://127.0.0.1:1".to_string(),
            config_broker_endpoint: "http://127.0.0.1:1".to_string(),
            control_plane_endpoint: "http://127.0.0.1:1".to_string(),
            tombstones: Arc::new(Mutex::new(HashMap::new())),
            liveness_cache: Arc::new(Mutex::new(HashMap::new())),
            stream_liveness: Arc::new(Mutex::new(HashMap::new())),
            disconnect_grace: Duration::from_secs(300),
            agent_session_writer: None,
            session_worker_writer: None,
            db: None,
        }
    }

    fn sample_transport(container_name: &str) -> TransportState {
        TransportState {
            container_name: container_name.to_string(),
            container_ip: "10.0.0.1".to_string(),
            tenant_name: "acme".to_string(),
            workspace: "mcp".to_string(),
            plugin_name: "mcp-bash".to_string(),
            staging_token: "tok-abc".to_string(),
            port: 8000,
            path: "/mcp".to_string(),
            upstream_auth: UpstreamAuth::None,
            upstream_authorization: None,
            agent_id: None,
            egress_policy: None,
        }
    }

    #[tokio::test]
    async fn handle_container_exit_unknown_container_returns_404() {
        let state = bare_state();
        let resp = handle_container_exit(&state, "nonexistent-container", "die", None)
            .await
            .expect("no error");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn handle_container_exit_known_container_returns_200() {
        let state = bare_state();
        let transport = sample_transport("mcp-session-abc");
        state
            .transport_sessions
            .lock()
            .await
            .insert("sess-123".to_string(), transport);

        let resp = handle_container_exit(&state, "mcp-session-abc", "die", Some(0))
            .await
            .expect("no error");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn handle_container_exit_removes_transport_and_tombstones_session() {
        let state = bare_state();
        let transport = sample_transport("mcp-session-xyz");
        state
            .transport_sessions
            .lock()
            .await
            .insert("sess-xyz".to_string(), transport);

        handle_container_exit(&state, "mcp-session-xyz", "die", None)
            .await
            .expect("no error");

        // Transport entry must be removed.
        assert!(
            !state
                .transport_sessions
                .lock()
                .await
                .contains_key("sess-xyz"),
            "transport should be removed"
        );
        // Tombstone must be set.
        assert!(
            state.tombstones.lock().await.contains_key("sess-xyz"),
            "tombstone should be set"
        );
    }

    #[tokio::test]
    async fn handle_container_exit_tombstone_expiry_is_in_the_future() {
        let state = bare_state();
        let transport = sample_transport("mcp-session-exp");
        state
            .transport_sessions
            .lock()
            .await
            .insert("sess-exp".to_string(), transport);

        handle_container_exit(&state, "mcp-session-exp", "die", None)
            .await
            .expect("no error");

        let expiry = state
            .tombstones
            .lock()
            .await
            .get("sess-exp")
            .copied()
            .expect("tombstone must exist");
        assert!(expiry > Instant::now(), "tombstone expiry should be future");
    }

    #[tokio::test]
    async fn handle_container_exit_with_db_writer_calls_record_reap() {
        let plugin_id = uuid::Uuid::new_v4();
        let mock = crate::store::mock::MockSessionWorkerStore::new()
            .with_plugin(plugin_id, "mcp-bash")
            .with_live_worker("mcp-session-reap", "10.0.0.2", "sess-reap-db", plugin_id);

        let mut state = bare_state();
        state.session_worker_writer = Some(Arc::new(mock.clone()));

        let transport = sample_transport("mcp-session-reap");
        state
            .transport_sessions
            .lock()
            .await
            .insert("sess-reap-db".to_string(), transport);

        let resp = handle_container_exit(&state, "mcp-session-reap", "die", Some(1))
            .await
            .expect("no error");
        assert_eq!(resp.status(), StatusCode::OK);
        // Transport should be gone.
        assert!(
            !state
                .transport_sessions
                .lock()
                .await
                .contains_key("sess-reap-db"),
            "transport removed after reap"
        );
        assert_eq!(
            mock.drain_recorded_reaps().await,
            vec!["mcp-session-reap".to_string()]
        );
    }
}
