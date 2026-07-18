use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use envoy_proto::envoy::config::core::v3::{
    header_value_option, HeaderMap, HeaderValue, HeaderValueOption,
};
use envoy_proto::envoy::r#type::v3::HttpStatus;
use envoy_proto::envoy::service::ext_proc::v3::external_processor_server::{
    ExternalProcessor, ExternalProcessorServer,
};
use envoy_proto::envoy::service::ext_proc::v3::{
    common_response, processing_request, processing_response, BodyResponse, CommonResponse,
    HeaderMutation, HeadersResponse, HttpBody, HttpHeaders, ImmediateResponse, ProcessingRequest,
    ProcessingResponse,
};
use rand::RngCore;
use regex::Regex;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use chrono::Utc;

use crate::config_broker::{self, EnvEntry, PluginDescriptor, UpstreamAuth, CONFIG_ENV_NAME};
use crate::control_plane::{self, PostSessionRequest};
use crate::docker::is_container_running;
use crate::launcher::{call_bind_agent, call_teardown, launch_session, probe_ready, LauncherError};
use crate::secrets;
use crate::{
    log_info, redact_token, AppState, PendingInit, SessionLiveness, TransportState,
    COLD_START_TIMEOUT, LIVENESS_TTL, TENANT_RE, TENANT_WORKSPACE_PLUGIN_PATH_RE, TOMBSTONE_TTL,
    WORKSPACE_RE,
};

/// `chrono::Utc::now()` formatted to the `%Y-%m-%dT%H:%M:%SZ` wire
/// shape used across every session-broker log line that surfaces a
/// timestamp. Pre-round-3 this lived in `session_registry.rs`; the
/// session-registry cleanup PR moved it down here next to the only
/// remaining caller.
fn utc_now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

static TENANT_PATTERN: OnceLock<Regex> = OnceLock::new();
static WORKSPACE_PATTERN: OnceLock<Regex> = OnceLock::new();
static TENANT_WORKSPACE_PLUGIN_PATH_PATTERN: OnceLock<Regex> = OnceLock::new();

fn tenant_pattern() -> &'static Regex {
    TENANT_PATTERN.get_or_init(|| Regex::new(TENANT_RE).expect("valid tenant regex"))
}

fn workspace_pattern() -> &'static Regex {
    WORKSPACE_PATTERN.get_or_init(|| Regex::new(WORKSPACE_RE).expect("valid workspace regex"))
}

fn tenant_workspace_plugin_path_pattern() -> &'static Regex {
    TENANT_WORKSPACE_PLUGIN_PATH_PATTERN.get_or_init(|| {
        Regex::new(TENANT_WORKSPACE_PLUGIN_PATH_RE).expect("valid tenant/workspace/plugin regex")
    })
}

#[derive(Debug, Clone)]
pub struct TeardownInfo {
    pub mcp_session_id: String,
    pub container_name: String,
    pub staging_path: String,
}

#[derive(Debug, Clone)]
pub struct PerStreamState {
    pub stream_id: String,
    pub method: String,
    pub path: String,
    pub authority: String,
    pub mcp_session_id: Option<String>,
    pub content_type: String,
    pub content_type_is_json: bool,
    pub client_addr: String,
    pub request_body: Vec<u8>,
    pub chosen_upstream: Option<String>,
    pub trusted_tenant: String,
    pub cap: Option<String>,
    pub teardown_on_response: Option<TeardownInfo>,
    /// Set to the `Mcp-Session-Id` whose liveness counter we incremented for
    /// this stream.  Used by the end-of-stream cleanup to decrement the counter.
    pub liveness_session_id: Option<String>,
}

impl Default for PerStreamState {
    fn default() -> Self {
        let mut bytes = [0u8; 8];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self {
            stream_id: bytes.iter().map(|b| format!("{b:02x}")).collect(),
            method: "POST".to_string(),
            path: String::new(),
            authority: String::new(),
            mcp_session_id: None,
            content_type: String::new(),
            content_type_is_json: false,
            client_addr: "unknown".to_string(),
            request_body: Vec::new(),
            chosen_upstream: None,
            trusted_tenant: String::new(),
            cap: None,
            teardown_on_response: None,
            liveness_session_id: None,
        }
    }
}

pub fn split_path_and_query(path: &str) -> (&str, &str) {
    if let Some(idx) = path.find('?') {
        (&path[..idx], &path[idx..])
    } else {
        (path, "")
    }
}

pub fn parse_tenant_workspace_plugin(path: &str) -> Option<(String, String, String)> {
    let (base_path, _) = split_path_and_query(path);
    let captures = tenant_workspace_plugin_path_pattern().captures(base_path)?;
    let tenant = captures.get(1)?.as_str().to_string();
    let workspace = captures.get(2)?.as_str().to_string();
    let plugin = captures.get(3)?.as_str().to_string();
    Some((tenant, workspace, plugin))
}

pub fn parse_full_path(path: &str, plugin_path: &str) -> Option<(String, String, String, String)> {
    let (base_path, query) = split_path_and_query(path);
    let captures = tenant_workspace_plugin_path_pattern().captures(base_path)?;
    let tenant = captures.get(1)?.as_str().to_string();
    let workspace = captures.get(2)?.as_str().to_string();
    let plugin = captures.get(3)?.as_str().to_string();
    let remainder = captures.get(4).map_or("", |m| m.as_str());
    let prefix = if plugin_path == "/" { "" } else { plugin_path };
    let body_raw = format!("{prefix}{remainder}");
    let body = if body_raw.is_empty() {
        "/".to_string()
    } else {
        body_raw
    };
    Some((tenant, workspace, plugin, format!("{body}{query}")))
}

pub fn forward_path(path: &str, plugin_path: &str) -> String {
    if let Some((_, _, _, rewritten)) = parse_full_path(path, plugin_path) {
        rewritten
    } else if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    }
}

pub fn extract_headers(headers: &HeaderMap) -> HashMap<String, String> {
    let mut extracted = HashMap::new();
    for header in &headers.headers {
        let key = header.key.to_lowercase();
        let value = if !header.value.is_empty() {
            header.value.clone()
        } else {
            String::from_utf8_lossy(&header.raw_value).to_string()
        };
        extracted.insert(key, value);
    }
    extracted
}

pub fn extract_agent_session_id(payload: &serde_json::Value) -> Result<Option<String>, String> {
    let params = payload.get("params");
    let Some(params) = params else {
        return Ok(None);
    };
    let Some(meta) = params.get("_meta") else {
        return Ok(None);
    };
    let Some(meta_object) = meta.as_object() else {
        return Err("params._meta must be an object".to_string());
    };
    let Some(session_id) = meta_object.get("agent-session-id") else {
        return Ok(None);
    };
    let Some(session_id) = session_id.as_str() else {
        return Err("agent-session-id must be a non-empty string".to_string());
    };
    if session_id.trim().is_empty() {
        return Err("agent-session-id must be a non-empty string".to_string());
    }
    Ok(Some(session_id.to_string()))
}

pub fn content_type_is_json(method: &str, content_type: &str) -> bool {
    if method == "GET" {
        return true;
    }
    content_type
        .trim()
        .to_ascii_lowercase()
        .starts_with("application/json")
}

fn staging_path(tenant_name: &str, token: &str) -> String {
    format!("/var/lib/botwork/tenants/{tenant_name}/staging/{token}")
}

fn agent_dir(tenant_name: &str, workspace: &str, agent_id: &str) -> String {
    format!("/var/lib/botwork/tenants/{tenant_name}/workspaces/{workspace}/agents/{agent_id}")
}

fn upstream(container_name: &str, port: u16) -> String {
    format!("{container_name}:{port}")
}

pub fn upstream_header_mutation(
    upstream_name: &str,
    rewritten_path: Option<&str>,
    upstream_authorization: Option<&str>,
    strip_authorization: bool,
) -> HeaderMutation {
    let mut set_headers = vec![HeaderValueOption {
        header: Some(HeaderValue {
            key: "x-session-upstream".to_string(),
            value: String::new(),
            raw_value: upstream_name.as_bytes().to_vec(),
        }),
        // MUST be OverwriteIfExistsOrAdd, not Append: x-session-upstream drives dynamic_forward_proxy
        // upstream host selection, so client-supplied values are an SSRF vector. Overwrite clobbers
        // any inbound value; Envoy edge stripping is defense-in-depth.
        append_action: header_value_option::HeaderAppendAction::OverwriteIfExistsOrAdd as i32,
        ..HeaderValueOption::default()
    }];

    if let Some(rewritten_path) = rewritten_path {
        set_headers.push(HeaderValueOption {
            header: Some(HeaderValue {
                key: ":path".to_string(),
                value: String::new(),
                raw_value: rewritten_path.as_bytes().to_vec(),
            }),
            append_action: header_value_option::HeaderAppendAction::OverwriteIfExistsOrAdd as i32,
            ..HeaderValueOption::default()
        });
    }

    if let Some(upstream_authorization) = upstream_authorization {
        let bearer = format!("Bearer {upstream_authorization}");
        set_headers.push(HeaderValueOption {
            header: Some(HeaderValue {
                key: "authorization".to_string(),
                value: String::new(),
                raw_value: bearer.into_bytes(),
            }),
            append_action: header_value_option::HeaderAppendAction::OverwriteIfExistsOrAdd as i32,
            ..HeaderValueOption::default()
        });
    }

    let mut remove_headers = vec!["x-botwork-cap".to_string()];
    if strip_authorization {
        remove_headers.push("authorization".to_string());
    }

    HeaderMutation {
        set_headers,
        remove_headers,
    }
}

fn upstream_common_response(
    upstream_name: &str,
    rewritten_path: Option<&str>,
    upstream_authorization: Option<&str>,
    strip_authorization: bool,
) -> CommonResponse {
    CommonResponse {
        header_mutation: Some(upstream_header_mutation(
            upstream_name,
            rewritten_path,
            upstream_authorization,
            strip_authorization,
        )),
        clear_route_cache: true,
        status: common_response::ResponseStatus::Continue as i32,
        ..CommonResponse::default()
    }
}

fn request_headers_continue() -> ProcessingResponse {
    ProcessingResponse {
        response: Some(processing_response::Response::RequestHeaders(
            HeadersResponse {
                response: Some(CommonResponse::default()),
            },
        )),
        ..ProcessingResponse::default()
    }
}

fn request_body_continue() -> ProcessingResponse {
    ProcessingResponse {
        response: Some(processing_response::Response::RequestBody(BodyResponse {
            response: Some(CommonResponse::default()),
        })),
        ..ProcessingResponse::default()
    }
}

fn response_headers_continue() -> ProcessingResponse {
    ProcessingResponse {
        response: Some(processing_response::Response::ResponseHeaders(
            HeadersResponse {
                response: Some(CommonResponse::default()),
            },
        )),
        ..ProcessingResponse::default()
    }
}

fn request_headers_route(
    upstream_name: &str,
    rewritten_path: Option<&str>,
    upstream_authorization: Option<&str>,
    strip_authorization: bool,
) -> ProcessingResponse {
    ProcessingResponse {
        response: Some(processing_response::Response::RequestHeaders(
            HeadersResponse {
                response: Some(upstream_common_response(
                    upstream_name,
                    rewritten_path,
                    upstream_authorization,
                    strip_authorization,
                )),
            },
        )),
        ..ProcessingResponse::default()
    }
}

fn route_authorization_for_transport(
    _state: &AppState,
    mcp_session_id: Option<&str>,
    transport: &TransportState,
) -> (Option<String>, bool) {
    let session_id = mcp_session_id.unwrap_or("");
    match (&transport.upstream_auth, &transport.upstream_authorization) {
        (UpstreamAuth::Bearer { service }, Some(value)) => {
            log_info(&format!(
                "route auth: session={session_id} plugin={} upstream_auth=bearer/{service} set (token={})",
                transport.plugin_name,
                redact_token(value)
            ));
            (Some(value.clone()), false)
        }
        (UpstreamAuth::None, _) => {
            log_info(&format!(
                "route auth: session={session_id} plugin={} upstream_auth=none stripped",
                transport.plugin_name
            ));
            (None, true)
        }
        (UpstreamAuth::Bearer { service }, None) => {
            log_info(&format!(
                "route auth: session={session_id} plugin={} upstream_auth=bearer/{service} configured but no resolved token on transport — stripped",
                transport.plugin_name
            ));
            (None, true)
        }
    }
}

fn resolve_spawn_upstream_authorization(
    tenant_name: &str,
    plugin_name: &str,
    upstream_auth: &UpstreamAuth,
    secrets: &[secrets::FetchedSecret],
) -> Result<Option<String>, String> {
    let UpstreamAuth::Bearer { service } = upstream_auth else {
        log_info(&format!(
            "tenant={tenant_name} plugin={plugin_name} upstream_auth: none — no upstream authorization header will be set"
        ));
        return Ok(None);
    };

    let matching: Vec<&secrets::FetchedSecret> = secrets
        .iter()
        .filter(|secret| secret.service == *service)
        .collect();

    match matching.as_slice() {
        [] => {
            let available_services = secret_services(secrets);
            log_info(&format!(
                "upstream_auth: bearer/{service} configured but no matching secret in vault for tenant={tenant_name} plugin={plugin_name} available_services=[{}]",
                available_services.join(",")
            ));
            Err("configured upstream authorization secret was not found".to_string())
        }
        [secret] => std::str::from_utf8(&secret.value)
            .map(|value| {
                log_info(&format!(
                    "upstream_auth: bearer/{service} resolved (token={}) tenant={tenant_name} plugin={plugin_name}",
                    redact_token(value)
                ));
                Some(value.to_string())
            })
            .map_err(|_| {
                log_info(&format!(
                    "upstream_auth: bearer/{service} matched non-UTF-8 secret for tenant={tenant_name} plugin={plugin_name}"
                ));
                "configured upstream authorization secret must be valid UTF-8".to_string()
            }),
        matches => {
            let mut matching_names: Vec<String> =
                matches.iter().map(|secret| secret.name.clone()).collect();
            matching_names.sort();
            log_info(&format!(
                "upstream_auth: bearer/{service} matched {} secrets (expected exactly 1) — spawn will fail tenant={tenant_name} plugin={plugin_name} names=[{}]",
                matches.len(),
                matching_names.join(",")
            ));
            Err(format!(
                "ambiguous upstream authorization secret for service '{service}'"
            ))
        }
    }
}

fn secret_services(secrets: &[secrets::FetchedSecret]) -> Vec<String> {
    let mut services: Vec<String> = secrets
        .iter()
        .map(|secret| secret.service.clone())
        .collect();
    services.sort();
    services.dedup();
    services
}

fn immediate_response(status: u32, body: &str) -> ProcessingResponse {
    ProcessingResponse {
        response: Some(processing_response::Response::ImmediateResponse(
            ImmediateResponse {
                status: Some(HttpStatus {
                    code: status as i32,
                }),
                body: body.as_bytes().to_vec(),
                ..ImmediateResponse::default()
            },
        )),
        ..ProcessingResponse::default()
    }
}

fn log_phase(state: &PerStreamState, phase: &str, decision: &str, fields: &[(&str, bool)]) {
    let mut parts = vec![
        format!("phase={phase}"),
        format!("method={}", state.method),
        format!(
            "has_mcp_session_id={}",
            if state.mcp_session_id.is_some() {
                "yes"
            } else {
                "no"
            }
        ),
        format!("decision={decision}"),
    ];
    for (field, include) in fields {
        if *include {
            parts.push(format!("{field}=<redacted>"));
        }
    }
    log_info(&parts.join(" "));
}

fn logged_immediate_response(
    state: &PerStreamState,
    phase: &str,
    status: u32,
    body: &str,
) -> ProcessingResponse {
    log_phase(
        state,
        "immediate_response",
        &format!("{phase}:{status}"),
        &[],
    );
    immediate_response(status, body)
}

async fn teardown_session(state: &AppState, teardown: &TeardownInfo) {
    // Cancel any pending grace timer first so it doesn't fire after teardown.
    liveness_remove(state, &teardown.mcp_session_id).await;
    // RFE #105 PR2: capture the agent_session identity BEFORE the
    // transport is removed below. We need (tenant, workspace,
    // agent_id) to write `state=inactive` after the container is
    // actually torn down. agent_id may be None (the container was
    // torn down before any /bind-agent saw goose); in that case
    // there's no DB row to transition and we skip the write.
    let agent_session_identity = {
        let sessions = state.transport_sessions.lock().await;
        sessions
            .get(&teardown.mcp_session_id)
            .and_then(|transport| {
                transport.agent_id.as_ref().map(|agent_id| {
                    (
                        transport.tenant_name.clone(),
                        transport.workspace.clone(),
                        agent_id.clone(),
                    )
                })
            })
    };
    call_teardown(
        &state.launcher_socket_path,
        &teardown.container_name,
        &teardown.staging_path,
    )
    .await;
    {
        let mut tombstones = state.tombstones.lock().await;
        tombstones.insert(
            teardown.mcp_session_id.clone(),
            Instant::now() + TOMBSTONE_TTL,
        );
    }
    {
        let mut sessions = state.transport_sessions.lock().await;
        sessions.remove(&teardown.mcp_session_id);
    }
    if let (Some(writer), Some((tenant, workspace, agent_id))) =
        (state.agent_session_writer.as_ref(), agent_session_identity)
    {
        writer.record_inactive(&tenant, &workspace, &agent_id).await;
    }
    // RFE #105 round-3 PR2: mark the session_worker row as reaped.
    // Container is gone, row stays for audit/billing until the
    // janitor sweeps it. Lookup is by container_name (globally
    // unique); we don't need the agent_session linkage here.
    if let Some(writer) = state.session_worker_writer.as_ref() {
        writer.record_reap(&teardown.container_name).await;
    }
}

/// Cancels any pending grace timer and removes the liveness entry for
/// `mcp_session_id`.  Called by all teardown paths so the grace timer cannot
/// fire after a session has already been reaped by another code path.
pub(crate) async fn liveness_remove(state: &AppState, mcp_session_id: &str) {
    let entry = state.stream_liveness.lock().await.remove(mcp_session_id);
    if let Some(liveness) = entry {
        let handle = liveness.grace_handle.lock().await.take();
        if let Some(handle) = handle {
            handle.abort();
        }
    }
}

/// Increments the open-stream counter for `mcp_session_id` and cancels any
/// pending grace timer.  The caller must then set `stream.liveness_session_id`
/// so the matching [`liveness_drop`] fires when the stream ends.
async fn liveness_bump(state: &AppState, mcp_session_id: &str) {
    let liveness = {
        let mut map = state.stream_liveness.lock().await;
        map.entry(mcp_session_id.to_string())
            .or_insert_with(|| Arc::new(SessionLiveness::default()))
            .clone()
    };
    liveness.open_streams.fetch_add(1, Ordering::SeqCst);
    // Use an explicit binding for the guard so Rust can reason that it is
    // dropped before `liveness` goes out of scope at the end of the function.
    let handle = liveness.grace_handle.lock().await.take();
    if let Some(handle) = handle {
        handle.abort();
        log_info(&format!(
            "liveness: session={mcp_session_id} reconnected, grace cancelled"
        ));
        // RFE #105 PR2: a cancelled grace handle means we were in
        // `state=grace` and a new stream came in before the grace
        // timer fired. Flip the agent_session row back to `active`
        // and bump `reactivation_count`. record_bind_agent's
        // UPDATE branch handles both — it INSERTs only if the row
        // doesn't exist, and the `is_reactivation` check inside it
        // is what bumps the counter.
        if let Some(writer) = state.agent_session_writer.as_ref() {
            let identity = {
                let sessions = state.transport_sessions.lock().await;
                sessions.get(mcp_session_id).and_then(|transport| {
                    transport.agent_id.as_ref().map(|agent_id| {
                        (
                            transport.tenant_name.clone(),
                            transport.workspace.clone(),
                            agent_id.clone(),
                        )
                    })
                })
            };
            if let Some((tenant, workspace, agent_id)) = identity {
                writer
                    .record_bind_agent(&tenant, &workspace, &agent_id)
                    .await;
            }
        }
    }
}

/// Decrements the open-stream counter for `mcp_session_id`.  When the counter
/// reaches zero a grace timer is armed; if no new stream opens within the grace
/// period the session is automatically reaped.
pub async fn liveness_drop(state: &AppState, mcp_session_id: &str) {
    let liveness = {
        let map = state.stream_liveness.lock().await;
        map.get(mcp_session_id).cloned()
    };
    let Some(liveness) = liveness else { return };
    let result = liveness
        .open_streams
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            if n == 0 {
                None
            } else {
                Some(n - 1)
            }
        });
    match result {
        Ok(1) => {
            // Was 1, now 0 — arm the grace timer.
            //
            // RFE #105 PR2: also flip the agent_session row to
            // `state=grace`. The reap_session path (if the grace
            // timer fires) transitions to `inactive` via the
            // teardown_session write below; if the client
            // reconnects, liveness_bump above moves the row back to
            // `active`.
            if let Some(writer) = state.agent_session_writer.as_ref() {
                let identity = {
                    let sessions = state.transport_sessions.lock().await;
                    sessions.get(mcp_session_id).and_then(|transport| {
                        transport.agent_id.as_ref().map(|agent_id| {
                            (
                                transport.tenant_name.clone(),
                                transport.workspace.clone(),
                                agent_id.clone(),
                            )
                        })
                    })
                };
                if let Some((tenant, workspace, agent_id)) = identity {
                    writer.record_grace(&tenant, &workspace, &agent_id).await;
                }
            }
            schedule_grace_timer(state.clone(), mcp_session_id.to_string(), liveness).await;
        }
        Ok(_) => {} // still > 0
        Err(_) => log_info(&format!(
            "liveness_drop: counter underflow guard for {mcp_session_id} (unexpected)"
        )),
    }
}

/// Arms a grace timer for `sid`.  When the timer fires the session is reaped
/// via [`reap_session`].  The timer handle is stored in `liveness` so it can be
/// cancelled if the client reconnects before expiry.
async fn schedule_grace_timer(state: AppState, sid: String, liveness: Arc<SessionLiveness>) {
    let grace = state.disconnect_grace;

    // Hold the handle lock across spawn-and-store so liveness_bump cannot
    // observe "no handle to abort" while we are mid-spawn.
    let mut guard = liveness.grace_handle.lock().await;

    // A bump might have raced in before we got the lock; if so the counter
    // is already > 0 and we must not arm the timer at all.
    if liveness.open_streams.load(Ordering::SeqCst) > 0 {
        return;
    }

    let state_clone = state.clone();
    let sid_clone = sid.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(grace).await;
        log_info(&format!(
            "liveness: session={sid_clone} grace expired, reaping"
        ));
        // Remove the liveness entry ourselves before calling reap_session.
        // teardown_session (called by reap_session) also calls liveness_remove,
        // which aborts the grace_handle stored in the SessionLiveness entry.
        // If we did NOT remove the entry here, that abort would cancel *this*
        // task — aborting call_teardown and leaking the container.  Removing
        // the entry first makes the liveness_remove inside teardown_session a
        // no-op (entry already gone).  Dropping a JoinHandle without .abort()
        // does not cancel the task, so we continue running safely.
        state_clone.stream_liveness.lock().await.remove(&sid_clone);
        reap_session(&state_clone, &sid_clone).await;
    });

    *guard = Some(handle);
    drop(guard);
    log_info(&format!(
        "liveness: session={sid} all streams closed, grace started ({grace:?})"
    ));
}

/// Performs session teardown by looking up teardown info from
/// `transport_sessions` (normal case) or falling back to the session registry
/// (broker-restart reconciliation case where `transport_sessions` is empty).
async fn reap_session(state: &AppState, mcp_session_id: &str) {
    // Try transport_sessions first (normal running case).
    let teardown = {
        let sessions = state.transport_sessions.lock().await;
        sessions.get(mcp_session_id).map(|t| TeardownInfo {
            mcp_session_id: mcp_session_id.to_string(),
            container_name: t.container_name.clone(),
            staging_path: staging_path(&t.tenant_name, &t.staging_token),
        })
    };
    // Round-3 cutover: the registry-fallback scan is gone with
    // sessions.json. If the in-memory transport entry is absent
    // here, the session is already torn down — nothing to do.
    if let Some(teardown) = teardown {
        teardown_session(state, &teardown).await;
    } else {
        log_info(&format!(
            "liveness: session={mcp_session_id} grace expired but no teardown info found; skipping"
        ));
    }
}

/// Seeds a grace timer for every session already present in the on-disk
/// registry at broker startup.  Sessions whose client does not reconnect within
/// the grace period are reaped; sessions that do reconnect have the timer
/// cancelled by the normal [`liveness_bump`] path.
pub async fn seed_startup_liveness(state: &AppState) {
    // Round-3 cutover: walks the recovered `transport_sessions` map
    // (populated by `recovery::recover_live_workers`) and arms a
    // grace timer for each. Pre-cutover this drew from
    // sessions.json; same shape, different feed.
    let sessions = state.transport_sessions.lock().await;
    let mcp_session_ids: Vec<String> = sessions.keys().cloned().collect();
    drop(sessions);
    for mcp_session_id in mcp_session_ids {
        let liveness = Arc::new(SessionLiveness::default());
        state
            .stream_liveness
            .lock()
            .await
            .insert(mcp_session_id.clone(), Arc::clone(&liveness));
        schedule_grace_timer(state.clone(), mcp_session_id, liveness).await;
    }
}

/// Returns `true` if `mcp_session_id` is currently tombstoned (i.e. a session
/// was recently torn down with this id and requests should get an immediate 404).
/// Expired tombstones are removed lazily on access.
async fn is_tombstoned(state: &AppState, mcp_session_id: &str) -> bool {
    let mut tombstones = state.tombstones.lock().await;
    match tombstones.get(mcp_session_id).copied() {
        Some(expires_at) if Instant::now() < expires_at => true,
        Some(_) => {
            // Expired tombstone — purge it
            tombstones.remove(mcp_session_id);
            false
        }
        None => false,
    }
}

/// Returns `true` if the container backing this transport session is running.
///
/// Uses a TTL'd in-memory cache (`LIVENESS_TTL`) keyed by container name to
/// avoid a `docker inspect` fork on every request.
///
/// - **Cache hit** (entry present and not yet expired): returns `true` immediately.
/// - **Cache miss or expired entry**: performs a single blocking `docker inspect`
///   call, which may add a brief delay for the first request after expiry.
///   On success, the container name is re-inserted into the cache.
/// - **Docker unavailable** (`None` from `is_container_running`): defaults to
///   `true` to avoid false-positive eviction when the docker CLI is unreachable.
async fn check_container_liveness(state: &AppState, container_name: &str) -> bool {
    {
        let cache = state.liveness_cache.lock().await;
        if let Some(&expires_at) = cache.get(container_name) {
            if Instant::now() < expires_at {
                return true; // cache hit
            }
        }
    }
    // Cache miss or expired — run docker inspect (blocking, but infrequent)
    let is_running = is_container_running(container_name).unwrap_or(true);
    if is_running {
        let mut cache = state.liveness_cache.lock().await;
        cache.insert(container_name.to_string(), Instant::now() + LIVENESS_TTL);
    }
    is_running
}

/// Evicts a transport session that was found to be backed by a dead container.
///
/// Tombstones the session id, removes the transport entry, records teardown in
/// the registry, and best-effort-calls the launcher teardown helper (spawned as
/// a background task so the calling request handler is not delayed).
async fn evict_dead_session(state: &AppState, mcp_session_id: &str, transport: &TransportState) {
    let container_name = transport.container_name.clone();
    let staging_path = staging_path(&transport.tenant_name, &transport.staging_token);
    log_info(&format!(
        "liveness_check: evicting dead session={mcp_session_id} container={container_name}"
    ));
    liveness_remove(state, mcp_session_id).await;
    {
        let mut tombstones = state.tombstones.lock().await;
        tombstones.insert(mcp_session_id.to_string(), Instant::now() + TOMBSTONE_TTL);
    }
    {
        let mut sessions = state.transport_sessions.lock().await;
        sessions.remove(mcp_session_id);
    }
    // RFE #105 PR2: shadow the eviction into agent_session. Same
    // shape as teardown_session above — capture the identity before
    // we lose the in-memory transport, then transition to `inactive`
    // after the launcher's teardown returns. agent_id is None when
    // the container died before /bind-agent ran; nothing to write.
    let agent_session_identity = transport.agent_id.as_ref().map(|agent_id| {
        (
            transport.tenant_name.clone(),
            transport.workspace.clone(),
            agent_id.clone(),
        )
    });
    let agent_session_writer = state.agent_session_writer.clone();
    // RFE #105 round-3 PR2: per the failure-model in
    // session_worker.rs, mark the worker row reaped on the
    // background teardown task — same shape as the agent_session
    // inactive transition above.
    let session_worker_writer = state.session_worker_writer.clone();
    let container_name_for_reap = container_name.clone();
    let launcher_path = state.launcher_socket_path.clone();
    tokio::spawn(async move {
        call_teardown(&launcher_path, &container_name, &staging_path).await;
        if let (Some(writer), Some((tenant, workspace, agent_id))) =
            (agent_session_writer, agent_session_identity)
        {
            writer.record_inactive(&tenant, &workspace, &agent_id).await;
        }
        if let Some(writer) = session_worker_writer {
            writer.record_reap(&container_name_for_reap).await;
        }
    });
}

/// Evict all live sessions for a tenant triggered by a secret mutation.
///
/// **Sync:** tombstones every matching `Mcp-Session-Id` and removes it from
/// the routing table so subsequent requests with those ids receive an
/// immediate 404.  Per the MCP Streamable HTTP spec, clients that receive a
/// 404 for their session MUST re-initialize; the re-initialize is a
/// session-less POST that re-enters the spawn path and re-fetches secrets.
///
/// **Async:** spawns a background task for each evicted session that calls
/// the launcher teardown helper and records the appropriate DB transitions
/// (`agent_session` inactive, `session_worker` reaped).  Container teardown
/// is explicitly off the caller's request path.
///
/// Returns the number of sessions evicted.
pub async fn evict_sessions_for_tenant(state: &AppState, tenant: &str) -> usize {
    // Collect all (session_id, TransportState) pairs for this tenant
    // under a single short-lived lock.  We hold nothing across the
    // async teardown tasks.
    let to_evict: Vec<(String, TransportState)> = {
        let sessions = state.transport_sessions.lock().await;
        sessions
            .iter()
            .filter(|(_, t)| t.tenant_name == tenant)
            .map(|(id, t)| (id.clone(), t.clone()))
            .collect()
    };

    let count = to_evict.len();
    if count == 0 {
        return 0;
    }

    log_info(&format!(
        "secret_evict: tenant={tenant:?} evicting {count} session(s)"
    ));

    for (mcp_session_id, transport) in to_evict {
        // ── Sync: make the session unreachable immediately ──────────
        liveness_remove(state, &mcp_session_id).await;
        {
            let mut tombstones = state.tombstones.lock().await;
            tombstones.insert(mcp_session_id.clone(), Instant::now() + TOMBSTONE_TTL);
        }
        {
            let mut sessions = state.transport_sessions.lock().await;
            sessions.remove(&mcp_session_id);
        }

        // ── Async: reap container + DB writes ────────────────────────
        let container_name = transport.container_name.clone();
        let staging_path = staging_path(&transport.tenant_name, &transport.staging_token);
        let agent_session_identity = transport.agent_id.as_ref().map(|agent_id| {
            (
                transport.tenant_name.clone(),
                transport.workspace.clone(),
                agent_id.clone(),
            )
        });
        let agent_session_writer = state.agent_session_writer.clone();
        let session_worker_writer = state.session_worker_writer.clone();
        let launcher_path = state.launcher_socket_path.clone();
        let container_name_for_reap = container_name.clone();
        log_info(&format!(
            "secret_evict: tombstoned session={mcp_session_id} container={container_name}"
        ));
        tokio::spawn(async move {
            call_teardown(&launcher_path, &container_name, &staging_path).await;
            if let (Some(writer), Some((ten, ws, agent_id))) =
                (agent_session_writer, agent_session_identity)
            {
                writer.record_inactive(&ten, &ws, &agent_id).await;
            }
            if let Some(writer) = session_worker_writer {
                writer.record_reap(&container_name_for_reap).await;
            }
        });
    }

    count
}

#[allow(clippy::too_many_arguments)]
async fn spawn_new_container(
    state: &AppState,
    client_addr: &str,
    tenant_name: &str,
    workspace: &str,
    plugin_name: &str,
    descriptor: &PluginDescriptor,
    upstream_authorization: Option<String>,
    env: &[(String, String)],
) -> Result<PendingInit, LauncherError> {
    let mut token_bytes = [0u8; 6];
    rand::thread_rng().fill_bytes(&mut token_bytes);
    let token: String = token_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let container_name = format!("mcp_session_{token}");
    let staging_path = staging_path(tenant_name, &token);
    let start = Instant::now();
    // RFE #105 round-3 PR2: stamp the three identity labels on every
    // spawned container. session-broker is the only writer; the
    // launcher's wire validator (#115) enforces the `io.botworkz.*`
    // namespace + value-shape rules. The agent_session_id is
    // intentionally NOT here — it arrives one round-trip later on
    // the first non-init JSON-RPC call, and `docker` doesn't allow
    // labels to be added to a running container. The DB-side
    // `session_worker.agent_session_id` column carries that linkage
    // instead.
    let labels: Vec<(String, String)> = vec![
        ("io.botworkz.tenant".to_string(), tenant_name.to_string()),
        ("io.botworkz.workspace".to_string(), workspace.to_string()),
        ("io.botworkz.plugin".to_string(), plugin_name.to_string()),
    ];
    let launch_outcome = launch_session(
        &state.launcher_socket_path,
        &container_name,
        &descriptor.image,
        &staging_path,
        env,
        &descriptor.resources,
        &labels,
    )
    .await?;
    let container_ip = launch_outcome.container_ip.clone();
    if let Ok(serialized) = serde_json::to_string(&launch_outcome.raw) {
        log_info(&format!("launcher response: {serialized}"));
    }
    let elapsed = start.elapsed();
    let remaining = COLD_START_TIMEOUT.saturating_sub(elapsed);
    if remaining.is_zero()
        || !probe_ready(
            &container_name,
            descriptor.port,
            std::time::Duration::from_millis(200),
            remaining,
        )
        .await
    {
        // Container is up but not listening. Tear down and surface
        // probe-timeout; control-plane is never told about it.
        teardown_unannounced_container(state, &container_name, &staging_path).await;
        return Err(LauncherError::ProbeTimeout {
            host: container_name,
            port: descriptor.port,
        });
    }

    // Hard gate: control-plane MUST 2xx the POST before this container
    // is allowed to serve traffic. Anything else (4xx, 5xx, transport
    // failure, timeout) tears the container down and surfaces a 503 to
    // the client. This is the load-bearing property the whole control-
    // plane design is built on (botwork #81): no plugin container ever
    // sees a single byte of client data without being announced.
    let post_request = PostSessionRequest {
        session_id: &container_name,
        container_ip: &container_ip,
        tenant: tenant_name,
        workspace,
        plugin: plugin_name,
        egress_policy: &descriptor.egress,
    };
    if let Err(err) = control_plane::post_session(
        &state.control_plane_endpoint,
        &post_request,
        std::time::Duration::from_secs(5),
    )
    .await
    {
        log_info(&format!(
            "control-plane gate failed for {container_name}: {err}; tearing down"
        ));
        teardown_unannounced_container(state, &container_name, &staging_path).await;
        return Err(LauncherError::ControlPlane(err));
    }

    // RFE #105 round-3 PR2: INSERT session_worker row now that
    // control-plane has 2xx'd the container. The row carries the
    // plugin + container_name + container_ip; agent_session_id is
    // NULL (the agent identity arrives one round-trip later on the
    // first non-init JSON-RPC call) and mcp_session_id is the empty
    // string (we backfill once response_headers observes it).
    //
    // Per the failure model in session_worker.rs::record_spawn,
    // INSERT failure logs a warn and carries on — the container is
    // up + control-plane knows about it; routing the user's first
    // request is more important than the audit row landing. On the
    // next cold-start recovery the missing row leads to immediate
    // reap (the agreed posture for "live container with no DB row").
    if let Some(writer) = state.session_worker_writer.as_ref() {
        writer
            .record_spawn(plugin_name, &container_name, &container_ip)
            .await;
    }

    log_info(&format!(
        "spawned container {container_name} ip={container_ip} (plugin={plugin_name} client={client_addr})"
    ));
    Ok(PendingInit {
        container_name,
        container_ip,
        staging_token: token,
        tenant_name: tenant_name.to_string(),
        workspace: workspace.to_string(),
        plugin_name: plugin_name.to_string(),
        descriptor: descriptor.clone(),
        upstream_authorization,
        created_at: utc_now(),
    })
}

/// Best-effort teardown of a container that was spawned but never made
/// it past the control-plane gate (or that timed out on probe_ready).
/// Fire-and-forget on the launcher side; we do not wait. The launcher's
/// docker-events watcher will fire the exit-listener handler on the
/// actual container exit, but at that point there is no transport
/// session to drop, so the cleanup is staging-mount-only.
async fn teardown_unannounced_container(
    state: &AppState,
    container_name: &str,
    staging_path: &str,
) {
    let launcher_path = state.launcher_socket_path.clone();
    let container = container_name.to_string();
    let staging = staging_path.to_string();
    tokio::spawn(async move {
        call_teardown(&launcher_path, &container, &staging).await;
    });
}

#[derive(Clone)]
pub struct ExternalProcessorService {
    state: AppState,
}

impl ExternalProcessorService {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }

    pub async fn handle_request_headers(
        state: &AppState,
        stream: &mut PerStreamState,
        msg: HttpHeaders,
    ) -> ProcessingResponse {
        let header_map = msg.headers.unwrap_or_default();
        let headers = extract_headers(&header_map);
        stream.method = headers
            .get(":method")
            .or_else(|| headers.get("method"))
            .cloned()
            .unwrap_or_else(|| "POST".to_string())
            .to_ascii_uppercase();
        stream.path = headers
            .get(":path")
            .or_else(|| headers.get("path"))
            .cloned()
            .unwrap_or_else(|| stream.path.clone());
        stream.authority = headers
            .get(":authority")
            .or_else(|| headers.get("host"))
            .cloned()
            .unwrap_or_else(|| stream.authority.clone());
        stream.mcp_session_id = headers.get("mcp-session-id").cloned();
        stream.content_type = headers.get("content-type").cloned().unwrap_or_default();
        stream.content_type_is_json = content_type_is_json(&stream.method, &stream.content_type);
        stream.client_addr = headers
            .get("x-envoy-external-address")
            .or_else(|| headers.get("x-forwarded-for"))
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        stream.trusted_tenant = headers
            .get("x-botwork-tenant")
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        stream.cap = headers.get("x-botwork-cap").map(|v| v.trim().to_string());

        if stream.trusted_tenant.is_empty() {
            return logged_immediate_response(
                stream,
                "request_headers",
                401,
                "missing x-botwork-tenant header",
            );
        }
        if !tenant_pattern().is_match(&stream.trusted_tenant) {
            return logged_immediate_response(
                stream,
                "request_headers",
                400,
                "invalid x-botwork-tenant header",
            );
        }

        if stream.method == "GET" {
            if stream.mcp_session_id.is_none() {
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    400,
                    "missing Mcp-Session-Id header",
                );
            }
            let mcp_session_id = stream.mcp_session_id.as_deref().unwrap();
            if is_tombstoned(state, mcp_session_id).await {
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    404,
                    "unknown mcp-session-id",
                );
            }
            let transport = {
                let sessions = state.transport_sessions.lock().await;
                sessions.get(mcp_session_id).cloned()
            };
            let Some(transport) = transport else {
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    404,
                    "unknown mcp-session-id",
                );
            };
            if transport.tenant_name != stream.trusted_tenant {
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    403,
                    "session tenant mismatch",
                );
            }
            if let Some((path_tenant_name, path_workspace, path_plugin_name)) =
                parse_tenant_workspace_plugin(&stream.path)
            {
                if path_tenant_name != stream.trusted_tenant {
                    return logged_immediate_response(
                        stream,
                        "request_headers",
                        403,
                        "tenant path/header mismatch",
                    );
                }
                if path_workspace != transport.workspace {
                    return logged_immediate_response(
                        stream,
                        "request_headers",
                        403,
                        "session workspace mismatch",
                    );
                }
                if path_plugin_name != transport.plugin_name {
                    log_info(&format!(
                        "session={} bound_plugin={} request_plugin={}",
                        mcp_session_id, transport.plugin_name, path_plugin_name
                    ));
                }
            }
            let rewritten_path = forward_path(&stream.path, &transport.path);
            let (upstream_authorization, strip_authorization) =
                route_authorization_for_transport(state, Some(mcp_session_id), &transport);
            stream.chosen_upstream = Some(upstream(&transport.container_name, transport.port));
            // Clone before calling bump so the &str borrow on stream.mcp_session_id
            // is released; split-field borrows let us write to liveness_session_id
            // in the same expression as reading from mcp_session_id.
            let lsid = mcp_session_id.to_string();
            liveness_bump(state, &lsid).await;
            stream.liveness_session_id = Some(lsid);
            log_phase(
                stream,
                "request_headers",
                "route_known_get_session",
                &[
                    ("upstream", stream.chosen_upstream.is_some()),
                    ("rewritten_path", true),
                ],
            );
            return request_headers_route(
                stream.chosen_upstream.as_deref().unwrap_or(""),
                Some(&rewritten_path),
                upstream_authorization.as_deref(),
                strip_authorization,
            );
        }

        if stream.method == "DELETE" {
            if stream.mcp_session_id.is_none() {
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    404,
                    "missing Mcp-Session-Id header",
                );
            }
            let mcp_session_id = stream.mcp_session_id.as_deref().unwrap();
            if is_tombstoned(state, mcp_session_id).await {
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    404,
                    "unknown mcp-session-id",
                );
            }
            let transport = {
                let sessions = state.transport_sessions.lock().await;
                sessions.get(mcp_session_id).cloned()
            };
            let Some(transport) = transport else {
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    404,
                    "unknown mcp-session-id",
                );
            };
            // Liveness check: if container is dead, evict and return 404
            if !check_container_liveness(state, &transport.container_name).await {
                evict_dead_session(state, mcp_session_id, &transport).await;
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    404,
                    "unknown mcp-session-id",
                );
            }
            if transport.tenant_name != stream.trusted_tenant {
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    403,
                    "session tenant mismatch",
                );
            }
            if let Some((path_tenant_name, path_workspace, path_plugin_name)) =
                parse_tenant_workspace_plugin(&stream.path)
            {
                if path_tenant_name != stream.trusted_tenant {
                    return logged_immediate_response(
                        stream,
                        "request_headers",
                        403,
                        "tenant path/header mismatch",
                    );
                }
                if path_workspace != transport.workspace {
                    return logged_immediate_response(
                        stream,
                        "request_headers",
                        403,
                        "session workspace mismatch",
                    );
                }
                if path_plugin_name != transport.plugin_name {
                    log_info(&format!(
                        "session={} bound_plugin={} request_plugin={}",
                        mcp_session_id, transport.plugin_name, path_plugin_name
                    ));
                }
            }
            let rewritten_path = forward_path(&stream.path, &transport.path);
            let (upstream_authorization, strip_authorization) =
                route_authorization_for_transport(state, Some(mcp_session_id), &transport);
            stream.chosen_upstream = Some(upstream(&transport.container_name, transport.port));
            stream.teardown_on_response = Some(TeardownInfo {
                mcp_session_id: mcp_session_id.to_string(),
                container_name: transport.container_name.clone(),
                staging_path: staging_path(&transport.tenant_name, &transport.staging_token),
            });
            log_phase(
                stream,
                "request_headers",
                "route_known_delete_session",
                &[
                    ("upstream", stream.chosen_upstream.is_some()),
                    ("rewritten_path", true),
                ],
            );
            return request_headers_route(
                stream.chosen_upstream.as_deref().unwrap_or(""),
                Some(&rewritten_path),
                upstream_authorization.as_deref(),
                strip_authorization,
            );
        }

        if stream.method == "POST" {
            if let Some(ref mcp_session_id) = stream.mcp_session_id.clone() {
                if is_tombstoned(state, mcp_session_id).await {
                    return logged_immediate_response(
                        stream,
                        "request_headers",
                        404,
                        "unknown mcp-session-id",
                    );
                }
                let transport = {
                    let sessions = state.transport_sessions.lock().await;
                    sessions.get(mcp_session_id).cloned()
                };
                if let Some(transport) = transport {
                    // Liveness check: if container is dead, evict and return 404
                    if !check_container_liveness(state, &transport.container_name).await {
                        evict_dead_session(state, mcp_session_id, &transport).await;
                        return logged_immediate_response(
                            stream,
                            "request_headers",
                            404,
                            "unknown mcp-session-id",
                        );
                    }
                    if transport.tenant_name != stream.trusted_tenant {
                        return logged_immediate_response(
                            stream,
                            "request_headers",
                            403,
                            "session tenant mismatch",
                        );
                    }
                    if let Some((path_tenant_name, path_workspace, path_plugin_name)) =
                        parse_tenant_workspace_plugin(&stream.path)
                    {
                        if path_tenant_name != stream.trusted_tenant {
                            return logged_immediate_response(
                                stream,
                                "request_headers",
                                403,
                                "tenant path/header mismatch",
                            );
                        }
                        if path_workspace != transport.workspace {
                            return logged_immediate_response(
                                stream,
                                "request_headers",
                                403,
                                "session workspace mismatch",
                            );
                        }
                        if path_plugin_name != transport.plugin_name {
                            log_info(&format!(
                                "session={} bound_plugin={} request_plugin={}",
                                mcp_session_id, transport.plugin_name, path_plugin_name
                            ));
                        }
                    }
                    let rewritten_path = forward_path(&stream.path, &transport.path);
                    let (upstream_authorization, strip_authorization) =
                        route_authorization_for_transport(state, Some(mcp_session_id), &transport);
                    stream.chosen_upstream =
                        Some(upstream(&transport.container_name, transport.port));
                    liveness_bump(state, mcp_session_id).await;
                    stream.liveness_session_id = Some(mcp_session_id.clone());
                    log_phase(
                        stream,
                        "request_headers",
                        "route_known_session",
                        &[
                            ("upstream", stream.chosen_upstream.is_some()),
                            ("rewritten_path", true),
                            ("content_type", !stream.content_type.is_empty()),
                        ],
                    );
                    return request_headers_route(
                        stream.chosen_upstream.as_deref().unwrap_or(""),
                        Some(&rewritten_path),
                        upstream_authorization.as_deref(),
                        strip_authorization,
                    );
                }
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    404,
                    "unknown mcp-session-id",
                );
            }

            let Some((path_tenant_name, workspace, plugin_name)) =
                parse_tenant_workspace_plugin(&stream.path)
            else {
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    400,
                    "workspace required: use /<tenant>/<workspace>/<plugin>",
                );
            };

            if !workspace_pattern().is_match(&workspace) {
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    400,
                    "invalid workspace",
                );
            }

            if path_tenant_name != stream.trusted_tenant {
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    403,
                    "tenant path/header mismatch",
                );
            }
            // Resolve descriptor from config-broker. Spawn fails closed on
            // any non-success — operator-fault 4xx pass through, infra 5xx /
            // transport collapses to 502.
            let descriptor = match config_broker::resolve(
                &state.config_broker_endpoint,
                &stream.trusted_tenant,
                &workspace,
                &plugin_name,
                std::time::Duration::from_secs(5),
            )
            .await
            {
                Ok(descriptor) => descriptor,
                Err(err) => {
                    return logged_immediate_response(
                        stream,
                        "request_headers",
                        err.status_code(),
                        &err.to_string(),
                    );
                }
            };
            let rewritten_path = forward_path(&stream.path, &descriptor.path);

            let cap = match stream.cap.as_deref() {
                Some(cap) if !cap.is_empty() => cap,
                _ => {
                    // Spawn requests should always arrive with a short-lived cap
                    // minted by ext_authz; missing cap indicates an auth edge
                    // misconfiguration or bypass. Fail closed.
                    log_info(&format!(
                        "spawn_secrets no_cap_on_spawn tenant={} plugin={}",
                        stream.trusted_tenant, plugin_name
                    ));
                    return logged_immediate_response(
                        stream,
                        "request_headers",
                        503,
                        "spawn requires x-botwork-cap",
                    );
                }
            };
            let fetched_secrets = match secrets::fetch_secrets(
                &state.auth_broker_url,
                cap,
                std::time::Duration::from_secs(5),
            )
            .await
            {
                Ok(secrets) => secrets,
                Err(err) => {
                    log_info(&format!(
                        "spawn_secrets fetch_failed tenant={} plugin={} fetch_error={err}",
                        stream.trusted_tenant, plugin_name
                    ));
                    return logged_immediate_response(
                        stream,
                        "request_headers",
                        503,
                        "spawn secrets fetch failed",
                    );
                }
            };
            let secret_env = secrets::build_env_entries(&fetched_secrets);
            let static_env_count = descriptor.env.len();
            let mut env: Vec<(String, String)> =
                Vec::with_capacity(static_env_count + secret_env.len() + 1);
            // Static plugin env first (deterministic config), secrets appended after.
            env.extend(
                descriptor
                    .env
                    .iter()
                    .map(|EnvEntry { name, value }| (name.clone(), value.clone())),
            );
            // Structured config follows static env. config-broker already
            // returns it as a compact-JSON string ready to drop in.
            if let Some(blob) = &descriptor.config_blob {
                env.push((CONFIG_ENV_NAME.to_string(), blob.clone()));
            }
            for entry in &secret_env {
                if env.len() >= secrets::MAX_ENV_ENTRIES {
                    log_info(&format!(
                        "spawn_secrets tenant={} plugin={} total env cap reached; truncating secrets",
                        stream.trusted_tenant, plugin_name
                    ));
                    break;
                }
                // Defensive: BOTWORK_SECRET_ prefix is reserved; static env cannot collide.
                if entry.0.starts_with(secrets::SECRET_ENV_PREFIX) {
                    env.push(entry.clone());
                } else {
                    log_info(&format!(
                        "spawn_secrets unexpected non-secret env name from secrets: {}; skipping",
                        entry.0
                    ));
                }
            }
            log_info(&format!(
                "spawn_secrets tenant={} plugin={} cap_present={} static_env={} secrets_injected={}",
                stream.trusted_tenant,
                plugin_name,
                matches!(stream.cap.as_deref(), Some(cap) if !cap.is_empty()),
                static_env_count,
                secret_env.len()
            ));
            let upstream_authorization = match resolve_spawn_upstream_authorization(
                &stream.trusted_tenant,
                &plugin_name,
                &descriptor.upstream_auth,
                &fetched_secrets,
            ) {
                Ok(value) => value,
                Err(message) => {
                    return logged_immediate_response(stream, "request_headers", 500, &message)
                }
            };

            let pending = match spawn_new_container(
                state,
                &stream.client_addr,
                &stream.trusted_tenant,
                &workspace,
                &plugin_name,
                &descriptor,
                upstream_authorization.clone(),
                &env,
            )
            .await
            {
                Ok(pending) => pending,
                Err(err) => {
                    return logged_immediate_response(
                        stream,
                        "request_headers",
                        err.status_code(),
                        &err.to_string(),
                    )
                }
            };

            {
                let mut pending_init = state.pending_init.lock().await;
                pending_init.insert(stream.stream_id.clone(), pending.clone());
            }

            stream.chosen_upstream =
                Some(upstream(&pending.container_name, pending.descriptor.port));
            log_phase(
                stream,
                "request_headers",
                "route_initialize_spawned",
                &[
                    ("upstream", stream.chosen_upstream.is_some()),
                    ("tenant", true),
                    ("plugin", true),
                    ("rewritten_path", true),
                ],
            );
            return request_headers_route(
                stream.chosen_upstream.as_deref().unwrap_or(""),
                Some(&rewritten_path),
                upstream_authorization.as_deref(),
                upstream_authorization.is_none(),
            );
        }

        log_phase(
            stream,
            "request_headers",
            "continue",
            &[
                ("content_type", !stream.content_type.is_empty()),
                ("authority", !stream.authority.is_empty()),
            ],
        );
        request_headers_continue()
    }

    pub async fn handle_request_body(
        state: &AppState,
        stream: &mut PerStreamState,
        msg: HttpBody,
    ) -> ProcessingResponse {
        if stream.method != "POST" {
            log_phase(
                stream,
                "request_body",
                "continue_non_post",
                &[("body_bytes", false), ("end_of_stream", true)],
            );
            return request_body_continue();
        }
        stream.request_body.extend_from_slice(&msg.body);
        if !msg.end_of_stream {
            log_phase(
                stream,
                "request_body",
                "continue_buffering",
                &[("body_bytes", true), ("end_of_stream", true)],
            );
            return request_body_continue();
        }
        if !stream.content_type_is_json || stream.mcp_session_id.is_none() {
            log_phase(
                stream,
                "request_body",
                "continue_non_json_or_no_session",
                &[
                    ("body_bytes", true),
                    ("content_type", !stream.content_type.is_empty()),
                ],
            );
            return request_body_continue();
        }
        let payload: serde_json::Value = match serde_json::from_slice(&stream.request_body) {
            Ok(payload) => payload,
            Err(_) => {
                log_phase(stream, "request_body", "continue_invalid_json", &[]);
                return request_body_continue();
            }
        };
        if !payload.is_object() {
            log_phase(stream, "request_body", "continue_non_object_json", &[]);
            return request_body_continue();
        }
        let agent_id = match extract_agent_session_id(&payload) {
            Ok(agent_id) => agent_id,
            Err(_) => {
                log_phase(
                    stream,
                    "request_body",
                    "continue_invalid_agent_session_id",
                    &[],
                );
                return request_body_continue();
            }
        };
        let Some(agent_id) = agent_id else {
            log_phase(stream, "request_body", "continue_no_agent_session_id", &[]);
            return request_body_continue();
        };
        let transport = {
            let sessions = state.transport_sessions.lock().await;
            sessions
                .get(stream.mcp_session_id.as_deref().unwrap_or_default())
                .cloned()
        };
        if let Some(mut transport) = transport {
            if transport.agent_id.is_none() {
                let result = call_bind_agent(
                    &state.launcher_socket_path,
                    &staging_path(&transport.tenant_name, &transport.staging_token),
                    &agent_dir(&transport.tenant_name, &transport.workspace, &agent_id),
                )
                .await;
                if result.is_ok() {
                    transport.agent_id = Some(agent_id.clone());
                    {
                        let mut sessions = state.transport_sessions.lock().await;
                        if let Some(existing) =
                            sessions.get_mut(stream.mcp_session_id.as_deref().unwrap_or_default())
                        {
                            if existing.agent_id.is_none() {
                                existing.agent_id = Some(agent_id.clone());
                            }
                        }
                    }
                    // RFE #105 PR2: mirror the bind into postgres.
                    // Round-3 deleted the JSON registry; this is now the
                    // only durable record of the bind.
                    if let Some(writer) = state.agent_session_writer.as_ref() {
                        writer
                            .record_bind_agent(
                                &transport.tenant_name,
                                &transport.workspace,
                                &agent_id,
                            )
                            .await;
                    }
                    // RFE #105 round-3 PR2: backfill the
                    // session_worker.agent_session_id linkage now
                    // that the agent_session row exists. We need the
                    // PK uuid, so resolve it via the shared DB handle
                    // — the writer surface only takes names. Failure
                    // here is `warn!` + carry on (the row stays with
                    // agent_session_id NULL until either the next
                    // bind retry or cold-start recovery cleans up).
                    if let (Some(writer), Some(db)) =
                        (state.session_worker_writer.as_ref(), state.db.as_ref())
                    {
                        // Identity slugs → PKs via the AgentSessionWriter
                        // cache surface (cheap, in-memory after first
                        // lookup).
                        if let Some(agent_writer) = state.agent_session_writer.as_ref() {
                            if let Some(agent_session_pk) = agent_writer
                                .resolve_pk(&transport.tenant_name, &transport.workspace, &agent_id)
                                .await
                            {
                                writer
                                    .record_agent_binding(
                                        &transport.container_name,
                                        agent_session_pk,
                                    )
                                    .await;
                            }
                        }
                        // Suppress the unused warning on `db` — kept
                        // on AppState for a future direct-SELECT
                        // recovery path that bypasses the writer's
                        // caches.
                        let _ = db;
                    }
                }
            }
        }
        log_phase(
            stream,
            "request_body",
            "continue_observe_bind_agent",
            &[("agent_id", true)],
        );
        request_body_continue()
    }

    pub async fn handle_response_headers(
        state: &AppState,
        stream: &mut PerStreamState,
        msg: HttpHeaders,
    ) -> ProcessingResponse {
        let header_map = msg.headers.unwrap_or_default();
        let headers = extract_headers(&header_map);

        if let Some(teardown) = stream.teardown_on_response.take() {
            teardown_session(state, &teardown).await;
        }

        let pending = {
            let mut pending_init = state.pending_init.lock().await;
            pending_init.remove(&stream.stream_id)
        };
        let Some(pending) = pending else {
            log_phase(
                stream,
                "response_headers",
                "continue_no_pending_initialize",
                &[("status", headers.contains_key(":status"))],
            );
            return response_headers_continue();
        };
        let Some(mcp_session_id) = headers.get("mcp-session-id").cloned() else {
            log_info(&format!(
                "initialize response missing Mcp-Session-Id; discarding pending container={}",
                pending.container_name
            ));
            log_phase(
                stream,
                "response_headers",
                "discard_pending_missing_session_id",
                &[
                    ("upstream", true),
                    ("status", headers.contains_key(":status")),
                ],
            );
            return response_headers_continue();
        };

        {
            let mut sessions = state.transport_sessions.lock().await;
            sessions.insert(
                mcp_session_id.clone(),
                TransportState {
                    container_name: pending.container_name.clone(),
                    container_ip: pending.container_ip.clone(),
                    staging_token: pending.staging_token.clone(),
                    tenant_name: pending.tenant_name.clone(),
                    workspace: pending.workspace.clone(),
                    plugin_name: pending.plugin_name.clone(),
                    port: pending.descriptor.port,
                    path: pending.descriptor.path.clone(),
                    upstream_auth: pending.descriptor.upstream_auth.clone(),
                    upstream_authorization: pending.upstream_authorization.clone(),
                    agent_id: None,
                    egress_policy: pending.descriptor.egress.clone(),
                },
            );
        }
        // RFE #105 round-3 PR2: backfill the session_worker row's
        // mcp_session_id now that the upstream's initialize response
        // has surfaced it. The row was INSERTed at spawn with the
        // empty default; this UPDATE makes it queryable by the
        // recovery path (which joins live docker containers to
        // session_worker rows by container_name).
        if let Some(writer) = state.session_worker_writer.as_ref() {
            writer
                .record_mcp_session_id(&pending.container_name, &mcp_session_id)
                .await;
        }
        liveness_bump(state, &mcp_session_id).await;
        stream.liveness_session_id = Some(mcp_session_id);

        log_phase(
            stream,
            "response_headers",
            "record_session_id",
            &[
                ("upstream", true),
                ("status", headers.contains_key(":status")),
                ("mcp_session_id", true),
            ],
        );
        response_headers_continue()
    }
}

#[tonic::async_trait]
impl ExternalProcessor for ExternalProcessorService {
    type ProcessStream = ReceiverStream<Result<ProcessingResponse, Status>>;

    async fn process(
        &self,
        request: Request<Streaming<ProcessingRequest>>,
    ) -> Result<Response<Self::ProcessStream>, Status> {
        let mut inbound = request.into_inner();
        let state = self.state.clone();
        let (tx, rx) = mpsc::channel(16);

        tokio::spawn(async move {
            let mut stream_state = PerStreamState::default();
            loop {
                let next = inbound.message().await;
                let Some(message) = (match next {
                    Ok(value) => value,
                    Err(status) => {
                        let _ = tx.send(Err(status)).await;
                        break;
                    }
                }) else {
                    break;
                };

                let response = match message.request {
                    Some(processing_request::Request::RequestHeaders(msg)) => {
                        ExternalProcessorService::handle_request_headers(
                            &state,
                            &mut stream_state,
                            msg,
                        )
                        .await
                    }
                    Some(processing_request::Request::RequestBody(msg)) => {
                        ExternalProcessorService::handle_request_body(
                            &state,
                            &mut stream_state,
                            msg,
                        )
                        .await
                    }
                    Some(processing_request::Request::ResponseHeaders(msg)) => {
                        ExternalProcessorService::handle_response_headers(
                            &state,
                            &mut stream_state,
                            msg,
                        )
                        .await
                    }
                    _ => continue,
                };
                if tx.send(Ok(response)).await.is_err() {
                    break;
                }
            }
            {
                let mut pending_init = state.pending_init.lock().await;
                pending_init.remove(&stream_state.stream_id);
            }
            // Decrement the open-stream counter.  This is the single exit
            // path for every stream regardless of how it ends (clean close,
            // error, or envoy-initiated disconnect), so the grace timer is
            // always armed when the last stream for a session closes.
            if let Some(ref sid) = stream_state.liveness_session_id {
                liveness_drop(&state, sid).await;
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

pub async fn serve_grpc(state: AppState, addr: &str) -> Result<(), String> {
    let socket_addr = addr
        .parse()
        .map_err(|e| format!("failed to parse gRPC address {addr}: {e}"))?;
    tonic::transport::Server::builder()
        .add_service(ExternalProcessorServer::new(ExternalProcessorService::new(
            state,
        )))
        .serve(socket_addr)
        .await
        .map_err(|e| format!("gRPC ext_proc service error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{start_log_capture, take_log_capture};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex as StdMutex, MutexGuard, OnceLock};
    use tokio::sync::Mutex;

    fn test_app_state(_plugin_name: &str, _upstream_auth: UpstreamAuth) -> AppState {
        // Pre-config-broker tests took a (plugin_name, upstream_auth) pair so
        // the registry could be seeded for the routing path's lookup. Routing
        // now reads `upstream_auth` directly off TransportState, so neither
        // argument is consulted by the routing-of-known-sessions tests in
        // this file. Kept on the signature to minimise call-site churn.
        AppState {
            transport_sessions: Arc::new(Mutex::new(HashMap::new())),
            pending_init: Arc::new(Mutex::new(HashMap::new())),
            launcher_socket_path: "/tmp/ext-proc-unit-launcher.sock".to_string(),
            auth_broker_url: "http://127.0.0.1:1".to_string(),
            config_broker_endpoint: "http://127.0.0.1:1".to_string(),
            control_plane_endpoint: "http://127.0.0.1:1".to_string(),
            tombstones: Arc::new(Mutex::new(HashMap::new())),
            liveness_cache: Arc::new(Mutex::new(HashMap::new())),
            stream_liveness: Arc::new(Mutex::new(HashMap::new())),
            disconnect_grace: std::time::Duration::from_secs(300),
            // The ext_proc unit tests live within the crate and don't
            // touch the agent_session write-through path. Production
            // sets this via `run()`; tests pass `None` so they don't
            // need to stand up a testcontainers postgres.
            agent_session_writer: None,
            // RFE #105 round-3 PR2: the cutover wires two
            // additional DB-bound handles next to
            // agent_session_writer. Test builders pass `None`
            // the same way to stay hermetic — production
            // populates both via `run()` once the
            // `connect_from_env()` handle is in hand.
            session_worker_writer: None,
            db: None,
        }
    }

    fn test_secret(service: &str, name: &str, value: &[u8]) -> secrets::FetchedSecret {
        secrets::FetchedSecret {
            service: service.to_string(),
            name: name.to_string(),
            kind: "api-key".to_string(),
            value: value.to_vec(),
        }
    }

    fn log_capture_guard() -> MutexGuard<'static, ()> {
        static LOG_CAPTURE_LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        LOG_CAPTURE_LOCK
            .get_or_init(|| StdMutex::new(()))
            .lock()
            // Recover from poisoning: if a prior test panicked while holding
            // this lock, the data (a unit `()`) is still valid and safe to use.
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn parse_full_path_three_components_required() {
        // Two components is the old shape — must return None
        assert!(parse_full_path("/tenant/plugin", "/mcp").is_none());
        // Three components: tenant/workspace/plugin
        let parsed = parse_full_path("/tenant/mcp/plugin", "/mcp").expect("valid path");
        assert_eq!(parsed.0, "tenant");
        assert_eq!(parsed.1, "mcp");
        assert_eq!(parsed.2, "plugin");
        assert_eq!(parsed.3, "/mcp");

        let parsed_with_extra = parse_full_path("/tenant/mcp/plugin/extra?query=1", "/mcp")
            .expect("valid path with extra");
        assert_eq!(parsed_with_extra.3, "/mcp/extra?query=1");

        assert!(parse_full_path("/tenant", "/mcp").is_none());
        assert!(parse_full_path("/Tenant/mcp/plugin", "/mcp").is_none());
        assert!(parse_full_path("/tenant/mcp/", "/mcp").is_none());
    }

    #[test]
    fn parse_full_path_root_default() {
        let parsed = parse_full_path("/tenant/mcp/plugin", "/").expect("valid path");
        assert_eq!(parsed.3, "/");

        let parsed_with_extra = parse_full_path("/tenant/mcp/plugin/extra?query=1", "/")
            .expect("valid path with extra");
        assert_eq!(parsed_with_extra.3, "/extra?query=1");
    }

    #[test]
    fn parse_full_path_custom_prefix() {
        let parsed = parse_full_path("/tenant/mcp/plugin/foo", "/api/v1").expect("valid path");
        assert_eq!(parsed.3, "/api/v1/foo");
    }

    #[test]
    fn parse_tenant_workspace_plugin_three_components() {
        assert!(parse_tenant_workspace_plugin("/t/p").is_none());
        let parsed = parse_tenant_workspace_plugin("/t/n/p").expect("valid path");
        assert_eq!(parsed, ("t".to_string(), "n".to_string(), "p".to_string()));
    }

    #[test]
    fn content_type_json_detection_matches_python() {
        assert!(content_type_is_json("GET", "text/plain"));
        assert!(content_type_is_json("POST", "application/json"));
        assert!(content_type_is_json(
            "POST",
            "application/json; charset=utf-8"
        ));
        assert!(!content_type_is_json("POST", "text/plain"));
    }

    #[test]
    fn extract_agent_session_id_variants() {
        let present = serde_json::json!({
            "params": { "_meta": { "agent-session-id": "agent-1" } }
        });
        assert_eq!(
            extract_agent_session_id(&present).unwrap(),
            Some("agent-1".to_string())
        );

        let missing = serde_json::json!({ "params": { "_meta": {} } });
        assert_eq!(extract_agent_session_id(&missing).unwrap(), None);

        let invalid_type = serde_json::json!({
            "params": { "_meta": { "agent-session-id": 42 } }
        });
        assert!(extract_agent_session_id(&invalid_type).is_err());

        let empty = serde_json::json!({
            "params": { "_meta": { "agent-session-id": " " } }
        });
        assert!(extract_agent_session_id(&empty).is_err());
    }

    #[test]
    fn header_mutation_uses_raw_value() {
        let mutation = upstream_header_mutation("mcp_session_abc:8000", Some("/mcp"), None, true);
        let first = mutation
            .set_headers
            .first()
            .and_then(|h| h.header.as_ref())
            .expect("first header mutation");
        assert_eq!(first.key, "x-session-upstream");
        let decoded = String::from_utf8(first.raw_value.to_vec()).expect("utf8");
        assert_eq!(decoded, "mcp_session_abc:8000");
    }

    #[test]
    fn header_mutation_removes_x_botwork_cap() {
        let mutation = upstream_header_mutation("mcp_session_abc:8000", Some("/mcp"), None, true);
        assert!(mutation
            .remove_headers
            .iter()
            .any(|name| name == "x-botwork-cap"));
    }

    #[test]
    fn header_mutation_removes_authorization_for_upstream_auth_none() {
        let mutation = upstream_header_mutation("mcp_session_abc:8000", Some("/mcp"), None, true);
        assert!(mutation
            .remove_headers
            .iter()
            .any(|name| name == "authorization"));
    }

    #[test]
    fn header_mutation_sets_authorization_for_cached_bearer() {
        let mutation = upstream_header_mutation(
            "mcp_session_abc:8000",
            Some("/mcp"),
            Some("ghp_cached"),
            false,
        );
        assert!(!mutation
            .remove_headers
            .iter()
            .any(|name| name == "authorization"));
        let authorization = mutation
            .set_headers
            .iter()
            .filter_map(|header| header.header.as_ref())
            .find(|header| header.key == "authorization")
            .expect("authorization header");
        assert_eq!(
            String::from_utf8(authorization.raw_value.clone()).expect("utf8"),
            "Bearer ghp_cached"
        );
    }

    #[test]
    fn split_path_and_query_variants() {
        assert_eq!(split_path_and_query("/foo"), ("/foo", ""));
        assert_eq!(split_path_and_query("/foo?bar=1"), ("/foo", "?bar=1"));
        assert_eq!(split_path_and_query(""), ("", ""));
    }

    #[test]
    fn forward_path_passes_through_non_plugin_paths() {
        assert_eq!(forward_path("/sessions", "/mcp"), "/sessions");
        assert_eq!(forward_path("", "/mcp"), "/");
    }

    #[test]
    fn forward_path_uses_plugin_path() {
        assert_eq!(forward_path("/tenant/mcp/plugin", "/"), "/");
        assert_eq!(forward_path("/tenant/mcp/plugin/foo?x=1", "/"), "/foo?x=1");
        assert_eq!(forward_path("/tenant/mcp/plugin", "/mcp"), "/mcp");
        assert_eq!(
            forward_path("/tenant/mcp/plugin/foo?x=1", "/mcp"),
            "/mcp/foo?x=1"
        );
        assert_eq!(
            forward_path("/tenant/mcp/plugin/foo", "/api/v1"),
            "/api/v1/foo"
        );
    }

    #[test]
    fn extract_headers_falls_back_to_raw_value() {
        let headers = HeaderMap {
            headers: vec![
                HeaderValue {
                    key: "x-test".to_string(),
                    value: String::new(),
                    raw_value: b"abc".to_vec(),
                },
                HeaderValue {
                    key: "X-Mixed-Case".to_string(),
                    value: "v".to_string(),
                    raw_value: Vec::new(),
                },
            ],
        };

        let extracted = extract_headers(&headers);
        assert_eq!(extracted.get("x-test").map(String::as_str), Some("abc"));
        assert_eq!(extracted.get("x-mixed-case").map(String::as_str), Some("v"));
    }

    #[test]
    fn upstream_header_mutation_without_path_only_sets_upstream() {
        let mutation = upstream_header_mutation("mcp_session_abc:8000", None, None, true);
        assert_eq!(mutation.set_headers.len(), 1);
        assert_eq!(
            mutation.set_headers[0]
                .header
                .as_ref()
                .map(|h| h.key.as_str()),
            Some("x-session-upstream")
        );
    }

    #[test]
    fn upstream_header_mutation_overwrites_existing_upstream_header() {
        let mutation = upstream_header_mutation("mcp_session_abc:8000", None, None, true);
        assert_eq!(
            mutation.set_headers[0].append_action,
            header_value_option::HeaderAppendAction::OverwriteIfExistsOrAdd as i32
        );
    }

    #[test]
    fn content_type_is_json_post_case_insensitive() {
        assert!(content_type_is_json("POST", "APPLICATION/JSON"));
        assert!(content_type_is_json("POST", "  application/json  "));
    }

    #[test]
    fn resolve_spawn_upstream_authorization_none_logs_no_auth() {
        let _guard = log_capture_guard();
        start_log_capture();
        let resolved =
            resolve_spawn_upstream_authorization("tenant1", "plugin-a", &UpstreamAuth::None, &[])
                .expect("none should resolve");
        let logs = take_log_capture().join("\n");

        assert_eq!(resolved, None);
        assert!(
            logs.contains("upstream_auth: none — no upstream authorization header will be set"),
            "missing upstream_auth none log: {logs}"
        );
        assert!(
            !logs.contains("ghp_SECRET"),
            "logs should not contain secret tokens: {logs}"
        );
    }

    #[test]
    fn resolve_spawn_upstream_authorization_bearer_match_one_logs_redacted_token() {
        let _guard = log_capture_guard();
        start_log_capture();
        let resolved = resolve_spawn_upstream_authorization(
            "tenant1",
            "plugin-a",
            &UpstreamAuth::Bearer {
                service: "github.com".to_string(),
            },
            &[test_secret("github.com", "pat", b"ghp_SECRET")],
        )
        .expect("single match should resolve");
        let logs = take_log_capture().join("\n");

        assert_eq!(resolved.as_deref(), Some("ghp_SECRET"));
        assert!(
            logs.contains("upstream_auth: bearer/github.com resolved"),
            "missing bearer resolution log: {logs}"
        );
        assert!(
            logs.contains(&redact_token("ghp_SECRET")),
            "missing redacted token in logs: {logs}"
        );
        assert!(
            !logs.contains("ghp_SECRET"),
            "logs should not contain raw bearer token: {logs}"
        );
    }

    #[test]
    fn resolve_spawn_upstream_authorization_bearer_match_zero_logs_available_services() {
        let _guard = log_capture_guard();
        start_log_capture();
        let err = resolve_spawn_upstream_authorization(
            "tenant1",
            "plugin-a",
            &UpstreamAuth::Bearer {
                service: "github".to_string(),
            },
            &[
                test_secret("github.com", "pat", b"ghp_one"),
                test_secret("shared", "token", b"shh"),
            ],
        )
        .expect_err("no match should fail");
        let logs = take_log_capture().join("\n");

        assert!(err.contains("not found"));
        assert!(
            logs.contains("available_services=[github.com,shared]"),
            "missing available_services log: {logs}"
        );
    }

    #[test]
    fn resolve_spawn_upstream_authorization_bearer_match_many_logs_names() {
        let _guard = log_capture_guard();
        start_log_capture();
        let err = resolve_spawn_upstream_authorization(
            "tenant1",
            "plugin-a",
            &UpstreamAuth::Bearer {
                service: "github.com".to_string(),
            },
            &[
                test_secret("github.com", "pat-a", b"ghp_one"),
                test_secret("github.com", "pat-b", b"ghp_two"),
            ],
        )
        .expect_err("multiple matches should fail");
        let logs = take_log_capture().join("\n");

        assert!(err.contains("ambiguous"));
        assert!(
            logs.contains("matched 2 secrets (expected exactly 1) — spawn will fail"),
            "missing multiple match log: {logs}"
        );
        assert!(
            logs.contains("names=[pat-a,pat-b]"),
            "missing matching names in log: {logs}"
        );
        assert!(
            !logs.contains("ghp_one"),
            "logs should not contain raw token values: {logs}"
        );
        assert!(
            !logs.contains("ghp_two"),
            "logs should not contain raw token values: {logs}"
        );
    }

    #[test]
    fn route_authorization_for_transport_logs_each_outcome() {
        let _guard = log_capture_guard();
        let base_transport = TransportState {
            container_name: "mcp_session_123".to_string(),
            container_ip: "172.20.0.5".to_string(),
            staging_token: "stage".to_string(),
            tenant_name: "tenant1".to_string(),
            workspace: "mcp".to_string(),
            plugin_name: "plugin-a".to_string(),
            port: 8000,
            path: "/mcp".to_string(),
            upstream_auth: UpstreamAuth::Bearer {
                service: "github.com".to_string(),
            },
            upstream_authorization: Some("ghp_SECRET".to_string()),
            agent_id: None,
            egress_policy: None,
        };
        // Routing reads upstream_auth straight off TransportState; the
        // AppState argument is unused on the routing-of-known-sessions path
        // since the config-broker split.
        let state = test_app_state("plugin-a", UpstreamAuth::None);

        // Bearer + resolved token → forwarded.
        start_log_capture();
        let (upstream_auth, strip) =
            route_authorization_for_transport(&state, Some("sid-1"), &base_transport);
        let logs_forward = take_log_capture().join("\n");
        assert_eq!(upstream_auth.as_deref(), Some("ghp_SECRET"));
        assert!(!strip);
        assert!(
            logs_forward.contains(
                "route auth: session=sid-1 plugin=plugin-a upstream_auth=bearer/github.com set"
            ),
            "missing route set log: {logs_forward}"
        );
        assert!(
            !logs_forward.contains("ghp_SECRET"),
            "logs should not contain raw route token: {logs_forward}"
        );

        // upstream_auth = None → stripped regardless of any cached token.
        start_log_capture();
        let mut transport_none = base_transport.clone();
        transport_none.upstream_auth = UpstreamAuth::None;
        let (upstream_auth, strip) =
            route_authorization_for_transport(&state, Some("sid-3"), &transport_none);
        let logs_none = take_log_capture().join("\n");
        assert_eq!(upstream_auth, None);
        assert!(strip);
        assert!(
            logs_none
                .contains("route auth: session=sid-3 plugin=plugin-a upstream_auth=none stripped"),
            "missing upstream_auth none stripped log: {logs_none}"
        );

        // Bearer configured but no resolved token cached → stripped, with a
        // distinguishing log line so the operator can tell this case apart
        // from the explicit upstream_auth=none case above.
        start_log_capture();
        let mut transport_no_token = base_transport.clone();
        transport_no_token.upstream_authorization = None;
        let (upstream_auth, strip) =
            route_authorization_for_transport(&state, Some("sid-4"), &transport_no_token);
        let logs_bearer_no_token = take_log_capture().join("\n");
        assert_eq!(upstream_auth, None);
        assert!(strip);
        assert!(
            logs_bearer_no_token.contains(
                "route auth: session=sid-4 plugin=plugin-a upstream_auth=bearer/github.com configured but no resolved token on transport — stripped"
            ),
            "missing bearer configured stripped log: {logs_bearer_no_token}"
        );
    }

    #[test]
    fn secret_services_are_sorted_and_deduplicated() {
        let services = secret_services(&[
            test_secret("github.com", "pat-a", b"ghp_one"),
            test_secret("shared", "token", b"shh"),
            test_secret("github.com", "pat-b", b"ghp_two"),
        ]);
        assert_eq!(
            services,
            vec!["github.com".to_string(), "shared".to_string()]
        );
    }

    #[test]
    fn immediate_response_sets_status_and_body() {
        let response = immediate_response(403, "forbidden");
        let processing_response::Response::ImmediateResponse(inner) =
            response.response.expect("immediate response")
        else {
            panic!("expected immediate response");
        };
        assert_eq!(inner.status.expect("status").code, 403);
        assert_eq!(inner.body, b"forbidden");
    }

    #[test]
    fn request_headers_route_applies_path_and_authorization_mutation() {
        let response =
            request_headers_route("mcp_session_abc:8000", Some("/mcp"), Some("token"), false);
        let processing_response::Response::RequestHeaders(headers) =
            response.response.expect("request headers response")
        else {
            panic!("expected request headers response");
        };
        let common = headers.response.expect("common response");
        assert!(common.clear_route_cache);
        let mutation = common.header_mutation.expect("header mutation");
        assert!(mutation
            .set_headers
            .iter()
            .filter_map(|h| h.header.as_ref())
            .any(|h| h.key == ":path"));
        assert!(mutation
            .set_headers
            .iter()
            .filter_map(|h| h.header.as_ref())
            .any(|h| h.key == "authorization"));
        assert!(!mutation.remove_headers.iter().any(|h| h == "authorization"));
    }

    #[test]
    fn logged_immediate_response_logs_phase_decision() {
        let _guard = log_capture_guard();
        start_log_capture();
        let state = PerStreamState {
            method: "POST".to_string(),
            mcp_session_id: Some("sid-1".to_string()),
            ..PerStreamState::default()
        };
        let _ = logged_immediate_response(&state, "request_headers", 404, "not found");
        let logs = take_log_capture().join("\n");
        assert!(logs.contains("phase=immediate_response"));
        assert!(logs.contains("decision=request_headers:404"));
    }

    #[test]
    fn route_authorization_with_missing_session_id_uses_empty_value() {
        let _guard = log_capture_guard();
        start_log_capture();
        let transport = TransportState {
            container_name: "mcp_session_123".to_string(),
            container_ip: "172.20.0.5".to_string(),
            staging_token: "stage".to_string(),
            tenant_name: "tenant1".to_string(),
            workspace: "mcp".to_string(),
            plugin_name: "plugin-a".to_string(),
            port: 8000,
            path: "/mcp".to_string(),
            upstream_auth: UpstreamAuth::None,
            upstream_authorization: Some("ghp_SECRET".to_string()),
            agent_id: None,
            egress_policy: None,
        };
        let state = test_app_state("plugin-a", UpstreamAuth::None);
        let (auth, strip) = route_authorization_for_transport(&state, None, &transport);
        assert_eq!(auth, None);
        assert!(strip);
        let logs = take_log_capture().join("\n");
        assert!(logs.contains("session="));
        assert!(logs.contains("upstream_auth=none stripped"));
    }

    // ── transport helpers ─────────────────────────────────────────────────────

    fn sample_transport_state() -> TransportState {
        TransportState {
            container_name: "mcp_session_test".to_string(),
            container_ip: "10.0.0.10".to_string(),
            staging_token: "tok1".to_string(),
            tenant_name: "acme".to_string(),
            workspace: "mcp".to_string(),
            plugin_name: "mcp-bash".to_string(),
            port: 8000,
            path: "/mcp".to_string(),
            upstream_auth: UpstreamAuth::None,
            upstream_authorization: None,
            agent_id: None,
            egress_policy: None,
        }
    }

    fn make_headers(
        pairs: &[(&str, &str)],
    ) -> envoy_proto::envoy::service::ext_proc::v3::HttpHeaders {
        use envoy_proto::envoy::config::core::v3::{HeaderMap, HeaderValue};
        envoy_proto::envoy::service::ext_proc::v3::HttpHeaders {
            headers: Some(HeaderMap {
                headers: pairs
                    .iter()
                    .map(|(k, v)| HeaderValue {
                        key: k.to_string(),
                        value: v.to_string(),
                        raw_value: Vec::new(),
                    })
                    .collect(),
            }),
            ..Default::default()
        }
    }

    fn make_body(body: &[u8], end_of_stream: bool) -> HttpBody {
        HttpBody {
            body: body.to_vec(),
            end_of_stream,
            ..Default::default()
        }
    }

    fn immediate_status(resp: &ProcessingResponse) -> i32 {
        match &resp.response {
            Some(processing_response::Response::ImmediateResponse(ir)) => {
                ir.status.as_ref().map(|s| s.code).unwrap_or(0)
            }
            _ => -1,
        }
    }

    fn is_continue_request_headers(resp: &ProcessingResponse) -> bool {
        matches!(
            resp.response,
            Some(processing_response::Response::RequestHeaders(_))
        )
    }

    fn is_continue_request_body(resp: &ProcessingResponse) -> bool {
        matches!(
            resp.response,
            Some(processing_response::Response::RequestBody(_))
        )
    }

    fn is_continue_response_headers(resp: &ProcessingResponse) -> bool {
        matches!(
            resp.response,
            Some(processing_response::Response::ResponseHeaders(_))
        )
    }

    async fn seed_transport(state: &AppState, mcp_session_id: &str, transport: TransportState) {
        state
            .transport_sessions
            .lock()
            .await
            .insert(mcp_session_id.to_string(), transport);
    }

    // ── handle_request_headers: missing / invalid tenant ─────────────────────

    #[tokio::test]
    async fn request_headers_missing_tenant_returns_401() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState::default();
        let msg = make_headers(&[(":method", "POST"), (":path", "/acme/mcp/mcp-bash")]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 401);
    }

    #[tokio::test]
    async fn request_headers_invalid_tenant_returns_400() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "POST"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "INVALID_TENANT!!"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 400);
    }

    // ── handle_request_headers: GET path ──────────────────────────────────────

    #[tokio::test]
    async fn request_headers_get_missing_session_id_returns_400() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "GET"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 400);
    }

    #[tokio::test]
    async fn request_headers_get_tombstoned_session_returns_404() {
        let state = test_app_state("p", UpstreamAuth::None);
        // Tombstone the session
        {
            let mut t = state.tombstones.lock().await;
            t.insert(
                "sess-tombstoned".to_string(),
                std::time::Instant::now() + std::time::Duration::from_secs(300),
            );
        }
        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "GET"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-tombstoned"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 404);
    }

    #[tokio::test]
    async fn request_headers_get_unknown_session_returns_404() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "GET"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-unknown"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 404);
    }

    #[tokio::test]
    async fn request_headers_get_tenant_mismatch_returns_403() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut transport = sample_transport_state();
        transport.tenant_name = "other-tenant".to_string(); // different from x-botwork-tenant
        seed_transport(&state, "sess-1", transport).await;
        // Seed liveness so liveness_bump doesn't panic
        state.stream_liveness.lock().await.insert(
            "sess-1".to_string(),
            Arc::new(crate::SessionLiveness::default()),
        );

        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "GET"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-1"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 403);
    }

    #[tokio::test]
    async fn request_headers_get_workspace_mismatch_returns_403() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut transport = sample_transport_state();
        transport.tenant_name = "acme".to_string();
        transport.workspace = "different-ws".to_string(); // path has "mcp"
        seed_transport(&state, "sess-ws-mismatch", transport).await;

        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "GET"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-ws-mismatch"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 403);
    }

    #[tokio::test]
    async fn request_headers_get_path_tenant_mismatch_returns_403() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut transport = sample_transport_state();
        transport.tenant_name = "acme".to_string();
        transport.workspace = "mcp".to_string();
        seed_transport(&state, "sess-ptmm", transport).await;

        let mut stream = PerStreamState::default();
        // x-botwork-tenant says "acme" but path says "other-tenant"
        let msg = make_headers(&[
            (":method", "GET"),
            (":path", "/other-tenant/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-ptmm"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 403);
    }

    #[tokio::test]
    async fn request_headers_get_known_session_routes() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut transport = sample_transport_state();
        transport.tenant_name = "acme".to_string();
        transport.workspace = "mcp".to_string();
        transport.plugin_name = "mcp-bash".to_string();
        seed_transport(&state, "sess-get-ok", transport).await;

        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "GET"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-get-ok"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert!(
            is_continue_request_headers(&resp),
            "expected RequestHeaders route response"
        );
        // Liveness should have been bumped
        let liveness = state.stream_liveness.lock().await;
        assert!(liveness.contains_key("sess-get-ok"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn request_headers_get_different_plugin_name_logs_but_routes() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut transport = sample_transport_state();
        transport.tenant_name = "acme".to_string();
        transport.workspace = "mcp".to_string();
        transport.plugin_name = "mcp-python".to_string(); // bound to different plugin
        seed_transport(&state, "sess-plugin-diff", transport).await;

        let _guard = log_capture_guard();
        start_log_capture();
        let mut stream = PerStreamState::default();
        // Path requests mcp-bash but session bound to mcp-python
        let msg = make_headers(&[
            (":method", "GET"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-plugin-diff"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        let logs = take_log_capture().join("\n");
        // Should still route (log a mismatch but don't reject)
        assert!(is_continue_request_headers(&resp));
        assert!(
            logs.contains("bound_plugin=mcp-python") && logs.contains("request_plugin=mcp-bash"),
            "expected plugin mismatch log, got: {logs}"
        );
    }

    // ── handle_request_headers: DELETE path ──────────────────────────────────

    #[tokio::test]
    async fn request_headers_delete_missing_session_id_returns_404() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "DELETE"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 404);
    }

    #[tokio::test]
    async fn request_headers_delete_tombstoned_returns_404() {
        let state = test_app_state("p", UpstreamAuth::None);
        {
            let mut t = state.tombstones.lock().await;
            t.insert(
                "sess-del-tomb".to_string(),
                std::time::Instant::now() + std::time::Duration::from_secs(300),
            );
        }
        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "DELETE"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-del-tomb"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 404);
    }

    #[tokio::test]
    async fn request_headers_delete_unknown_session_returns_404() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "DELETE"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-del-unknown"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 404);
    }

    #[tokio::test]
    async fn request_headers_delete_tenant_mismatch_returns_403() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut transport = sample_transport_state();
        transport.tenant_name = "other".to_string();
        // Seed liveness cache so liveness check passes
        {
            let mut cache = state.liveness_cache.lock().await;
            cache.insert(
                transport.container_name.clone(),
                std::time::Instant::now() + std::time::Duration::from_secs(300),
            );
        }
        seed_transport(&state, "sess-del-tmm", transport).await;

        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "DELETE"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-del-tmm"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 403);
    }

    #[tokio::test]
    async fn request_headers_delete_known_session_sets_teardown_on_response() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut transport = sample_transport_state();
        transport.tenant_name = "acme".to_string();
        transport.workspace = "mcp".to_string();
        transport.plugin_name = "mcp-bash".to_string();
        // Seed liveness cache so container-liveness check passes
        {
            let mut cache = state.liveness_cache.lock().await;
            cache.insert(
                transport.container_name.clone(),
                std::time::Instant::now() + std::time::Duration::from_secs(300),
            );
        }
        seed_transport(&state, "sess-del-ok", transport).await;

        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "DELETE"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-del-ok"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert!(
            is_continue_request_headers(&resp),
            "expected route response"
        );
        assert!(
            stream.teardown_on_response.is_some(),
            "teardown should be armed on DELETE"
        );
    }

    #[tokio::test]
    async fn request_headers_delete_workspace_mismatch_returns_403() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut transport = sample_transport_state();
        transport.tenant_name = "acme".to_string();
        transport.workspace = "other-ws".to_string(); // different from path "mcp"
        {
            let mut cache = state.liveness_cache.lock().await;
            cache.insert(
                transport.container_name.clone(),
                std::time::Instant::now() + std::time::Duration::from_secs(300),
            );
        }
        seed_transport(&state, "sess-del-wsmm", transport).await;

        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "DELETE"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-del-wsmm"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 403);
    }

    #[tokio::test]
    async fn request_headers_delete_path_tenant_mismatch_returns_403() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut transport = sample_transport_state();
        transport.tenant_name = "acme".to_string();
        transport.workspace = "mcp".to_string();
        {
            let mut cache = state.liveness_cache.lock().await;
            cache.insert(
                transport.container_name.clone(),
                std::time::Instant::now() + std::time::Duration::from_secs(300),
            );
        }
        seed_transport(&state, "sess-del-ptmm", transport).await;

        let mut stream = PerStreamState::default();
        // Path tenant ≠ header tenant
        let msg = make_headers(&[
            (":method", "DELETE"),
            (":path", "/other-tenant/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-del-ptmm"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 403);
    }

    // ── handle_request_headers: POST with existing session ───────────────────

    #[tokio::test]
    async fn request_headers_post_known_session_tombstoned_returns_404() {
        let state = test_app_state("p", UpstreamAuth::None);
        {
            let mut t = state.tombstones.lock().await;
            t.insert(
                "sess-post-tomb".to_string(),
                std::time::Instant::now() + std::time::Duration::from_secs(300),
            );
        }
        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "POST"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-post-tomb"),
            ("content-type", "application/json"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 404);
    }

    #[tokio::test]
    async fn request_headers_post_known_session_no_transport_returns_404() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "POST"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-post-no-transport"),
            ("content-type", "application/json"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 404);
    }

    #[tokio::test]
    async fn request_headers_post_known_session_tenant_mismatch_returns_403() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut transport = sample_transport_state();
        transport.tenant_name = "other".to_string();
        // Seed liveness cache so liveness check passes
        {
            let mut cache = state.liveness_cache.lock().await;
            cache.insert(
                transport.container_name.clone(),
                std::time::Instant::now() + std::time::Duration::from_secs(300),
            );
        }
        seed_transport(&state, "sess-post-tmm", transport).await;

        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "POST"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-post-tmm"),
            ("content-type", "application/json"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 403);
    }

    #[tokio::test]
    async fn request_headers_post_known_session_workspace_mismatch_returns_403() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut transport = sample_transport_state();
        transport.tenant_name = "acme".to_string();
        transport.workspace = "other-ws".to_string();
        {
            let mut cache = state.liveness_cache.lock().await;
            cache.insert(
                transport.container_name.clone(),
                std::time::Instant::now() + std::time::Duration::from_secs(300),
            );
        }
        seed_transport(&state, "sess-post-wsmm", transport).await;

        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "POST"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-post-wsmm"),
            ("content-type", "application/json"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 403);
    }

    #[tokio::test]
    async fn request_headers_post_known_session_path_tenant_mismatch_returns_403() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut transport = sample_transport_state();
        transport.tenant_name = "acme".to_string();
        transport.workspace = "mcp".to_string();
        {
            let mut cache = state.liveness_cache.lock().await;
            cache.insert(
                transport.container_name.clone(),
                std::time::Instant::now() + std::time::Duration::from_secs(300),
            );
        }
        seed_transport(&state, "sess-post-ptmm", transport).await;

        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "POST"),
            (":path", "/other-tenant/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-post-ptmm"),
            ("content-type", "application/json"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 403);
    }

    #[tokio::test]
    async fn request_headers_post_known_session_routes_ok() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut transport = sample_transport_state();
        transport.tenant_name = "acme".to_string();
        transport.workspace = "mcp".to_string();
        transport.plugin_name = "mcp-bash".to_string();
        {
            let mut cache = state.liveness_cache.lock().await;
            cache.insert(
                transport.container_name.clone(),
                std::time::Instant::now() + std::time::Duration::from_secs(300),
            );
        }
        seed_transport(&state, "sess-post-ok", transport).await;

        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "POST"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-post-ok"),
            ("content-type", "application/json"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert!(
            is_continue_request_headers(&resp),
            "expected route response"
        );
        assert_eq!(stream.liveness_session_id.as_deref(), Some("sess-post-ok"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn request_headers_post_known_session_different_plugin_logs() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut transport = sample_transport_state();
        transport.tenant_name = "acme".to_string();
        transport.workspace = "mcp".to_string();
        transport.plugin_name = "mcp-python".to_string();
        {
            let mut cache = state.liveness_cache.lock().await;
            cache.insert(
                transport.container_name.clone(),
                std::time::Instant::now() + std::time::Duration::from_secs(300),
            );
        }
        seed_transport(&state, "sess-post-diff-plugin", transport).await;

        let _guard = log_capture_guard();
        start_log_capture();
        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "POST"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("mcp-session-id", "sess-post-diff-plugin"),
            ("content-type", "application/json"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        let logs = take_log_capture().join("\n");
        assert!(is_continue_request_headers(&resp));
        assert!(
            logs.contains("mcp-python"),
            "expected plugin mismatch log: {logs}"
        );
    }

    // ── handle_request_headers: POST without session (new spawn) ─────────────

    #[tokio::test]
    async fn request_headers_post_no_session_invalid_path_returns_400() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "POST"),
            (":path", "/acme"), // not /<tenant>/<workspace>/<plugin>
            ("x-botwork-tenant", "acme"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 400);
    }

    #[tokio::test]
    async fn request_headers_post_no_session_invalid_workspace_returns_400() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "POST"),
            (":path", "/acme/INVALID_WS/mcp-bash"), // uppercase invalid
            ("x-botwork-tenant", "acme"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 400);
    }

    #[tokio::test]
    async fn request_headers_post_no_session_tenant_mismatch_returns_403() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "POST"),
            (":path", "/other-tenant/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"), // header says acme, path says other-tenant
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        assert_eq!(immediate_status(&resp), 403);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn request_headers_post_no_session_missing_cap_returns_503() {
        // To reach the cap check, config-broker must succeed first.
        // Spin up a minimal TCP server that returns a valid descriptor.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock config-broker");
        let port = listener.local_addr().unwrap().port();
        let descriptor_json = r#"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 69\r\n\r\n{"image":"img:1","port":8000,"path":"/mcp","upstream_auth":"none"}"#;
        let descriptor_body = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            r#"{"image":"img:1","port":8000,"path":"/mcp","upstream_auth":"none"}"#.len(),
            r#"{"image":"img:1","port":8000,"path":"/mcp","upstream_auth":"none"}"#
        );
        let _ = descriptor_json; // suppress warning
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::AsyncWriteExt;
                let _ = stream.write_all(descriptor_body.as_bytes()).await;
            }
        });
        let mut state = test_app_state("p", UpstreamAuth::None);
        state.config_broker_endpoint = format!("http://127.0.0.1:{port}");

        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "POST"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
            ("content-type", "application/json"),
            // deliberately no x-botwork-cap
        ]);
        let _guard = log_capture_guard();
        start_log_capture();
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        let logs = take_log_capture().join("\n");
        assert_eq!(immediate_status(&resp), 503, "missing cap should 503");
        assert!(logs.contains("no_cap_on_spawn"), "expected cap log: {logs}");
    }

    // ── handle_request_headers: non-POST/GET/DELETE → continue ───────────────

    #[tokio::test]
    async fn request_headers_other_method_continues() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState::default();
        let msg = make_headers(&[
            (":method", "OPTIONS"),
            (":path", "/acme/mcp/mcp-bash"),
            ("x-botwork-tenant", "acme"),
        ]);
        let resp = ExternalProcessorService::handle_request_headers(&state, &mut stream, msg).await;
        // Not POST/GET/DELETE falls through to continue
        assert!(is_continue_request_headers(&resp));
    }

    // ── handle_request_body ───────────────────────────────────────────────────

    #[tokio::test]
    async fn request_body_non_post_continues() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState {
            method: "GET".to_string(),
            ..PerStreamState::default()
        };
        let msg = make_body(b"irrelevant", true);
        let resp = ExternalProcessorService::handle_request_body(&state, &mut stream, msg).await;
        assert!(is_continue_request_body(&resp));
    }

    #[tokio::test]
    async fn request_body_partial_body_continues_without_processing() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState {
            method: "POST".to_string(),
            mcp_session_id: Some("sess-1".to_string()),
            content_type_is_json: true,
            ..PerStreamState::default()
        };
        let msg = make_body(b"{\"partial\":", false); // end_of_stream = false
        let resp = ExternalProcessorService::handle_request_body(&state, &mut stream, msg).await;
        assert!(is_continue_request_body(&resp));
        // Body accumulates even on partial chunk
        assert!(!stream.request_body.is_empty());
    }

    #[tokio::test]
    async fn request_body_non_json_content_type_continues() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState {
            method: "POST".to_string(),
            mcp_session_id: Some("sess-1".to_string()),
            content_type_is_json: false, // not JSON
            ..PerStreamState::default()
        };
        let msg = make_body(b"plain text", true);
        let resp = ExternalProcessorService::handle_request_body(&state, &mut stream, msg).await;
        assert!(is_continue_request_body(&resp));
    }

    #[tokio::test]
    async fn request_body_no_session_id_continues() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState {
            method: "POST".to_string(),
            mcp_session_id: None, // no session id
            content_type_is_json: true,
            ..PerStreamState::default()
        };
        let msg = make_body(b"{}", true);
        let resp = ExternalProcessorService::handle_request_body(&state, &mut stream, msg).await;
        assert!(is_continue_request_body(&resp));
    }

    #[tokio::test]
    async fn request_body_invalid_json_continues() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState {
            method: "POST".to_string(),
            mcp_session_id: Some("sess-1".to_string()),
            content_type_is_json: true,
            ..PerStreamState::default()
        };
        let msg = make_body(b"not-json{{{", true);
        let resp = ExternalProcessorService::handle_request_body(&state, &mut stream, msg).await;
        assert!(is_continue_request_body(&resp));
    }

    #[tokio::test]
    async fn request_body_non_object_json_continues() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState {
            method: "POST".to_string(),
            mcp_session_id: Some("sess-1".to_string()),
            content_type_is_json: true,
            ..PerStreamState::default()
        };
        let msg = make_body(b"[1,2,3]", true); // array, not object
        let resp = ExternalProcessorService::handle_request_body(&state, &mut stream, msg).await;
        assert!(is_continue_request_body(&resp));
    }

    #[tokio::test]
    async fn request_body_invalid_agent_session_id_continues() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState {
            method: "POST".to_string(),
            mcp_session_id: Some("sess-1".to_string()),
            content_type_is_json: true,
            ..PerStreamState::default()
        };
        // agent-session-id is a number, not a string
        let body = br#"{"params":{"_meta":{"agent-session-id":42}}}"#;
        let msg = make_body(body, true);
        let resp = ExternalProcessorService::handle_request_body(&state, &mut stream, msg).await;
        assert!(is_continue_request_body(&resp));
    }

    #[tokio::test]
    async fn request_body_no_agent_session_id_continues() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState {
            method: "POST".to_string(),
            mcp_session_id: Some("sess-1".to_string()),
            content_type_is_json: true,
            ..PerStreamState::default()
        };
        // Valid JSON object, no params at all
        let body = br#"{"method":"tools/list"}"#;
        let msg = make_body(body, true);
        let resp = ExternalProcessorService::handle_request_body(&state, &mut stream, msg).await;
        assert!(is_continue_request_body(&resp));
    }

    #[tokio::test]
    async fn request_body_session_missing_from_transport_continues() {
        // mcp_session_id set but not in transport_sessions → no bind call
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState {
            method: "POST".to_string(),
            mcp_session_id: Some("sess-no-transport".to_string()),
            content_type_is_json: true,
            ..PerStreamState::default()
        };
        let body = br#"{"params":{"_meta":{"agent-session-id":"agent-1"}}}"#;
        let msg = make_body(body, true);
        let resp = ExternalProcessorService::handle_request_body(&state, &mut stream, msg).await;
        assert!(is_continue_request_body(&resp));
    }

    #[tokio::test]
    async fn request_body_agent_already_bound_skips_bind() {
        // Transport has agent_id already set → no bind_agent call
        let state = test_app_state("p", UpstreamAuth::None);
        let mut transport = sample_transport_state();
        transport.agent_id = Some("existing-agent".to_string()); // already bound
        seed_transport(&state, "sess-already-bound", transport).await;

        let mut stream = PerStreamState {
            method: "POST".to_string(),
            mcp_session_id: Some("sess-already-bound".to_string()),
            content_type_is_json: true,
            ..PerStreamState::default()
        };
        let body = br#"{"params":{"_meta":{"agent-session-id":"new-agent"}}}"#;
        let msg = make_body(body, true);
        let resp = ExternalProcessorService::handle_request_body(&state, &mut stream, msg).await;
        assert!(is_continue_request_body(&resp));
        // Original agent_id should remain unchanged
        let sessions = state.transport_sessions.lock().await;
        let transport = sessions.get("sess-already-bound").unwrap();
        assert_eq!(transport.agent_id.as_deref(), Some("existing-agent"));
    }

    #[tokio::test]
    async fn request_body_chunked_accumulation() {
        // Two chunks, only process on end_of_stream
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState {
            method: "POST".to_string(),
            mcp_session_id: Some("sess-chunk".to_string()),
            content_type_is_json: true,
            ..PerStreamState::default()
        };
        // First chunk (no EOS)
        let chunk1 = make_body(b"{\"method\":", false);
        let resp1 =
            ExternalProcessorService::handle_request_body(&state, &mut stream, chunk1).await;
        assert!(is_continue_request_body(&resp1));
        assert_eq!(&stream.request_body, b"{\"method\":");

        // Second chunk with EOS
        let chunk2 = make_body(b"\"ping\"}", true);
        let resp2 =
            ExternalProcessorService::handle_request_body(&state, &mut stream, chunk2).await;
        assert!(is_continue_request_body(&resp2));
        assert_eq!(&stream.request_body, b"{\"method\":\"ping\"}");
    }

    // ── handle_response_headers ───────────────────────────────────────────────

    #[tokio::test]
    async fn response_headers_no_pending_continues() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState::default();
        let msg = make_headers(&[(":status", "200"), ("content-type", "application/json")]);
        let resp =
            ExternalProcessorService::handle_response_headers(&state, &mut stream, msg).await;
        assert!(is_continue_response_headers(&resp));
    }

    #[tokio::test]
    async fn response_headers_missing_mcp_session_id_discards_pending() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState {
            stream_id: "stream-rh-1".to_string(),
            ..PerStreamState::default()
        };
        // Insert a pending init
        let pending = crate::PendingInit {
            container_name: "mcp_session_rh1".to_string(),
            container_ip: "10.0.0.1".to_string(),
            staging_token: "tok".to_string(),
            tenant_name: "acme".to_string(),
            workspace: "mcp".to_string(),
            plugin_name: "mcp-bash".to_string(),
            descriptor: crate::config_broker::PluginDescriptor {
                image: "img".to_string(),
                port: 8000,
                path: "/mcp".to_string(),
                upstream_auth: crate::config_broker::UpstreamAuth::None,
                resources: Default::default(),
                env: Vec::new(),
                config_blob: None,
                egress: None,
            },
            upstream_authorization: None,
            created_at: "now".to_string(),
        };
        state
            .pending_init
            .lock()
            .await
            .insert("stream-rh-1".to_string(), pending);

        // Response headers without Mcp-Session-Id → discard pending
        let msg = make_headers(&[(":status", "200")]);
        let resp =
            ExternalProcessorService::handle_response_headers(&state, &mut stream, msg).await;
        assert!(is_continue_response_headers(&resp));
        // pending should be removed
        assert!(state.pending_init.lock().await.is_empty());
    }

    #[tokio::test]
    async fn response_headers_with_mcp_session_id_seeds_transport() {
        let state = test_app_state("p", UpstreamAuth::None);
        let mut stream = PerStreamState {
            stream_id: "stream-rh-2".to_string(),
            ..PerStreamState::default()
        };
        let pending = crate::PendingInit {
            container_name: "mcp_session_rh2".to_string(),
            container_ip: "10.0.0.2".to_string(),
            staging_token: "tok2".to_string(),
            tenant_name: "acme".to_string(),
            workspace: "mcp".to_string(),
            plugin_name: "mcp-bash".to_string(),
            descriptor: crate::config_broker::PluginDescriptor {
                image: "img".to_string(),
                port: 8000,
                path: "/mcp".to_string(),
                upstream_auth: crate::config_broker::UpstreamAuth::None,
                resources: Default::default(),
                env: Vec::new(),
                config_blob: None,
                egress: None,
            },
            upstream_authorization: None,
            created_at: "now".to_string(),
        };
        state
            .pending_init
            .lock()
            .await
            .insert("stream-rh-2".to_string(), pending);

        let msg = make_headers(&[(":status", "200"), ("mcp-session-id", "mcp-sess-new")]);
        let resp =
            ExternalProcessorService::handle_response_headers(&state, &mut stream, msg).await;
        assert!(is_continue_response_headers(&resp));

        // Transport should be seeded
        let sessions = state.transport_sessions.lock().await;
        assert!(
            sessions.contains_key("mcp-sess-new"),
            "transport should be seeded"
        );
        let t = sessions.get("mcp-sess-new").unwrap();
        assert_eq!(t.container_name, "mcp_session_rh2");
        assert_eq!(t.tenant_name, "acme");

        // Liveness should be bumped
        assert_eq!(stream.liveness_session_id.as_deref(), Some("mcp-sess-new"));
    }

    #[tokio::test]
    async fn response_headers_with_teardown_on_response_fires_teardown() {
        let state = test_app_state("p", UpstreamAuth::None);
        // Seed a transport that will be torn down
        let transport = sample_transport_state();
        seed_transport(&state, "sess-teardown", transport).await;

        let mut stream = PerStreamState {
            teardown_on_response: Some(TeardownInfo {
                mcp_session_id: "sess-teardown".to_string(),
                container_name: "mcp_session_test".to_string(),
                staging_path: "/var/lib/botwork/tenants/acme/staging/tok1".to_string(),
            }),
            ..PerStreamState::default()
        };
        let msg = make_headers(&[(":status", "200")]);
        let resp =
            ExternalProcessorService::handle_response_headers(&state, &mut stream, msg).await;
        assert!(is_continue_response_headers(&resp));
        assert!(
            stream.teardown_on_response.is_none(),
            "teardown_on_response should be consumed"
        );
        // Session should be tombstoned and removed
        assert!(state.transport_sessions.lock().await.is_empty());
    }

    // ── is_tombstoned ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn is_tombstoned_live_tombstone_returns_true() {
        let state = test_app_state("p", UpstreamAuth::None);
        {
            let mut t = state.tombstones.lock().await;
            t.insert(
                "live-tomb".to_string(),
                std::time::Instant::now() + std::time::Duration::from_secs(300),
            );
        }
        assert!(is_tombstoned(&state, "live-tomb").await);
    }

    #[tokio::test]
    async fn is_tombstoned_expired_tombstone_returns_false_and_removes() {
        let state = test_app_state("p", UpstreamAuth::None);
        {
            let mut t = state.tombstones.lock().await;
            // Expired: subtract duration (already in the past)
            t.insert(
                "expired-tomb".to_string(),
                std::time::Instant::now() - std::time::Duration::from_secs(1),
            );
        }
        assert!(!is_tombstoned(&state, "expired-tomb").await);
        // Should have been removed
        assert!(state.tombstones.lock().await.is_empty());
    }

    #[tokio::test]
    async fn is_tombstoned_unknown_returns_false() {
        let state = test_app_state("p", UpstreamAuth::None);
        assert!(!is_tombstoned(&state, "unknown").await);
    }

    // ── liveness_bump / liveness_drop ─────────────────────────────────────────

    #[tokio::test]
    async fn liveness_bump_creates_entry_and_increments_counter() {
        let state = test_app_state("p", UpstreamAuth::None);
        liveness_bump(&state, "sess-bump").await;
        let liveness = state.stream_liveness.lock().await;
        let entry = liveness.get("sess-bump").expect("entry");
        assert_eq!(
            entry.open_streams.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn liveness_drop_when_counter_is_one_arms_grace_timer() {
        let state = test_app_state("p", UpstreamAuth::None);
        // Bump first so there's something to drop
        liveness_bump(&state, "sess-drop-grace").await;
        // Drop → counter 1→0 → grace timer armed
        liveness_drop(&state, "sess-drop-grace").await;
        let liveness = state.stream_liveness.lock().await;
        let entry = liveness
            .get("sess-drop-grace")
            .expect("entry should still exist");
        let has_handle = entry.grace_handle.lock().await.is_some();
        assert!(has_handle, "grace timer should be armed after drop to 0");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn liveness_drop_when_counter_already_zero_logs_underflow() {
        let state = test_app_state("p", UpstreamAuth::None);
        // Insert entry with counter = 0 directly
        state.stream_liveness.lock().await.insert(
            "sess-underflow".to_string(),
            Arc::new(crate::SessionLiveness::default()),
        );
        let _guard = log_capture_guard();
        start_log_capture();
        // Drop without prior bump → underflow guard triggers
        liveness_drop(&state, "sess-underflow").await;
        let logs = take_log_capture().join("\n");
        assert!(
            logs.contains("underflow guard"),
            "expected underflow log: {logs}"
        );
    }

    #[tokio::test]
    async fn liveness_drop_unknown_session_is_noop() {
        let state = test_app_state("p", UpstreamAuth::None);
        // Should not panic
        liveness_drop(&state, "sess-unknown-drop").await;
    }

    #[tokio::test]
    async fn liveness_drop_counter_above_one_stays_above_zero() {
        let state = test_app_state("p", UpstreamAuth::None);
        liveness_bump(&state, "sess-multi-stream").await;
        liveness_bump(&state, "sess-multi-stream").await; // 2 streams
        liveness_drop(&state, "sess-multi-stream").await; // back to 1
        let liveness = state.stream_liveness.lock().await;
        let entry = liveness.get("sess-multi-stream").expect("entry");
        assert_eq!(
            entry.open_streams.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "counter should be 1 after one drop from 2"
        );
        let has_handle = entry.grace_handle.lock().await.is_some();
        assert!(!has_handle, "grace should NOT be armed when counter > 0");
    }

    #[tokio::test]
    async fn liveness_bump_cancels_pending_grace_timer() {
        let state = test_app_state("p", UpstreamAuth::None);
        liveness_bump(&state, "sess-reconnect").await;
        liveness_drop(&state, "sess-reconnect").await; // arms grace
                                                       // Verify grace armed
        {
            let liveness = state.stream_liveness.lock().await;
            let entry = liveness.get("sess-reconnect").expect("entry");
            assert!(
                entry.grace_handle.lock().await.is_some(),
                "grace should be armed"
            );
        }
        // Re-bump (reconnect) → grace should be cancelled
        liveness_bump(&state, "sess-reconnect").await;
        {
            let liveness = state.stream_liveness.lock().await;
            let entry = liveness.get("sess-reconnect").expect("entry");
            assert!(
                entry.grace_handle.lock().await.is_none(),
                "grace should be cancelled on reconnect"
            );
        }
    }

    // ── liveness_remove ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn liveness_remove_cancels_grace_and_removes_entry() {
        let state = test_app_state("p", UpstreamAuth::None);
        liveness_bump(&state, "sess-remove").await;
        liveness_drop(&state, "sess-remove").await; // arms grace
        liveness_remove(&state, "sess-remove").await;
        assert!(!state
            .stream_liveness
            .lock()
            .await
            .contains_key("sess-remove"));
    }

    #[tokio::test]
    async fn liveness_remove_unknown_is_noop() {
        let state = test_app_state("p", UpstreamAuth::None);
        liveness_remove(&state, "sess-remove-unknown").await; // should not panic
    }

    // ── seed_startup_liveness ────────────────────────────────────────────────

    #[tokio::test]
    async fn seed_startup_liveness_arms_grace_for_recovered_sessions() {
        let state = test_app_state("p", UpstreamAuth::None);
        // Seed a transport session (simulates recover_live_workers output)
        seed_transport(&state, "recovered-sess", sample_transport_state()).await;
        seed_startup_liveness(&state).await;
        // A grace timer should have been armed for the session
        let liveness = state.stream_liveness.lock().await;
        assert!(
            liveness.contains_key("recovered-sess"),
            "seed_startup_liveness should create liveness entry"
        );
    }

    #[tokio::test]
    async fn seed_startup_liveness_empty_transport_map_is_noop() {
        let state = test_app_state("p", UpstreamAuth::None);
        seed_startup_liveness(&state).await;
        assert!(state.stream_liveness.lock().await.is_empty());
    }

    // ── reap_session ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn reap_session_with_transport_does_teardown() {
        let state = test_app_state("p", UpstreamAuth::None);
        seed_transport(&state, "sess-reap", sample_transport_state()).await;
        reap_session(&state, "sess-reap").await;
        // Transport should be removed (tombstoned + removed from map)
        assert!(state.transport_sessions.lock().await.is_empty());
        // Tombstone should be set
        assert!(state.tombstones.lock().await.contains_key("sess-reap"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn reap_session_without_transport_logs_and_skips() {
        let state = test_app_state("p", UpstreamAuth::None);
        let _guard = log_capture_guard();
        start_log_capture();
        reap_session(&state, "sess-reap-missing").await;
        let logs = take_log_capture().join("\n");
        assert!(
            logs.contains("no teardown info found"),
            "expected skip log: {logs}"
        );
    }

    // ── evict_sessions_for_tenant ─────────────────────────────────────────────

    #[tokio::test]
    async fn evict_sessions_for_tenant_returns_zero_when_no_sessions() {
        let state = test_app_state("p", UpstreamAuth::None);
        let count = evict_sessions_for_tenant(&state, "acme").await;
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn evict_sessions_for_tenant_evicts_matching_sessions() {
        let state = test_app_state("p", UpstreamAuth::None);
        // Two sessions for "acme", one for "other"
        let mut t1 = sample_transport_state();
        t1.tenant_name = "acme".to_string();
        let mut t2 = sample_transport_state();
        t2.tenant_name = "acme".to_string();
        t2.container_name = "mcp_session_t2".to_string();
        let mut t3 = sample_transport_state();
        t3.tenant_name = "other".to_string();
        t3.container_name = "mcp_session_t3".to_string();
        seed_transport(&state, "sess-acme-1", t1).await;
        seed_transport(&state, "sess-acme-2", t2).await;
        seed_transport(&state, "sess-other", t3).await;

        let count = evict_sessions_for_tenant(&state, "acme").await;
        assert_eq!(count, 2);
        let sessions = state.transport_sessions.lock().await;
        assert!(!sessions.contains_key("sess-acme-1"));
        assert!(!sessions.contains_key("sess-acme-2"));
        assert!(
            sessions.contains_key("sess-other"),
            "other tenant session should remain"
        );
    }

    // ── check_container_liveness ─────────────────────────────────────────────

    #[tokio::test]
    async fn check_container_liveness_cache_hit_returns_true() {
        let state = test_app_state("p", UpstreamAuth::None);
        {
            let mut cache = state.liveness_cache.lock().await;
            cache.insert(
                "mcp_session_live".to_string(),
                std::time::Instant::now() + std::time::Duration::from_secs(300),
            );
        }
        assert!(check_container_liveness(&state, "mcp_session_live").await);
    }

    #[tokio::test]
    async fn check_container_liveness_cache_miss_calls_docker() {
        // Cache miss: docker inspect runs (returns false for non-existent container
        // if docker available, or true if docker unavailable per unwrap_or(true)).
        let state = test_app_state("p", UpstreamAuth::None);
        // No cache entry → falls through to docker inspect
        let result = check_container_liveness(&state, "mcp_session_definitely_nonexistent").await;
        // If docker not available → true (unwrap_or(true)). If docker available → false.
        // Either way, function is total (doesn't panic).
        let _ = result;
    }

    // ── utc_now / staging_path / upstream helpers ─────────────────────────────

    #[test]
    fn utc_now_is_formatted_correctly() {
        let s = utc_now();
        // Should match %Y-%m-%dT%H:%M:%SZ
        assert!(s.ends_with('Z'), "utc_now should end with Z");
        assert!(s.len() == 20, "utc_now should be 20 chars");
    }

    #[test]
    fn staging_path_format() {
        let p = staging_path("acme", "token123");
        assert_eq!(p, "/var/lib/botwork/tenants/acme/staging/token123");
    }

    #[test]
    fn agent_dir_format() {
        let d = agent_dir("acme", "mcp", "agent-1");
        assert_eq!(
            d,
            "/var/lib/botwork/tenants/acme/workspaces/mcp/agents/agent-1"
        );
    }

    #[test]
    fn upstream_format() {
        assert_eq!(upstream("mcp_session_abc", 8000), "mcp_session_abc:8000");
    }
}
