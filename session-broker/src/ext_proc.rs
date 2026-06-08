use std::collections::HashMap;
use std::sync::OnceLock;
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

use crate::launcher::{call_bind_agent, launch_session, probe_ready, LauncherError};
use crate::plugin_registry::PluginConfig;
use crate::secrets;
use crate::session_registry::utc_now;
use crate::{
    log_info, AppState, PendingInit, TransportState, COLD_START_TIMEOUT, TENANT_PLUGIN_PATH_RE,
    TENANT_RE,
};

static TENANT_PATTERN: OnceLock<Regex> = OnceLock::new();
static TENANT_PLUGIN_PATH_PATTERN: OnceLock<Regex> = OnceLock::new();

fn tenant_pattern() -> &'static Regex {
    TENANT_PATTERN.get_or_init(|| Regex::new(TENANT_RE).expect("valid tenant regex"))
}

fn tenant_plugin_path_pattern() -> &'static Regex {
    TENANT_PLUGIN_PATH_PATTERN
        .get_or_init(|| Regex::new(TENANT_PLUGIN_PATH_RE).expect("valid tenant/plugin regex"))
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

pub fn parse_tenant_plugin(path: &str) -> Option<(String, String)> {
    let (base_path, _) = split_path_and_query(path);
    let captures = tenant_plugin_path_pattern().captures(base_path)?;
    let tenant = captures.get(1)?.as_str().to_string();
    let plugin = captures.get(2)?.as_str().to_string();
    Some((tenant, plugin))
}

pub fn parse_plugin_path(path: &str, plugin_path: &str) -> Option<(String, String, String)> {
    let (base_path, query) = split_path_and_query(path);
    let captures = tenant_plugin_path_pattern().captures(base_path)?;
    let tenant = captures.get(1)?.as_str().to_string();
    let plugin = captures.get(2)?.as_str().to_string();
    let remainder = captures.get(3).map_or("", |m| m.as_str());
    let prefix = if plugin_path == "/" { "" } else { plugin_path };
    let body = format!("{prefix}{remainder}");
    let body = if body.is_empty() {
        "/".to_string()
    } else {
        body
    };
    Some((tenant, plugin, format!("{body}{query}")))
}

pub fn forward_path(path: &str, plugin_path: &str) -> String {
    if let Some((_, _, rewritten)) = parse_plugin_path(path, plugin_path) {
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

fn agent_dir(tenant_name: &str, agent_id: &str) -> String {
    format!("/var/lib/botwork/tenants/{tenant_name}/agents/{agent_id}")
}

fn upstream(container_name: &str, port: u16) -> String {
    format!("{container_name}:{port}")
}

pub fn upstream_header_mutation(
    upstream_name: &str,
    rewritten_path: Option<&str>,
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

    HeaderMutation {
        set_headers,
        remove_headers: vec!["x-botwork-cap".to_string()],
    }
}

fn upstream_common_response(upstream_name: &str, rewritten_path: Option<&str>) -> CommonResponse {
    CommonResponse {
        header_mutation: Some(upstream_header_mutation(upstream_name, rewritten_path)),
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

fn request_headers_route(upstream_name: &str, rewritten_path: Option<&str>) -> ProcessingResponse {
    ProcessingResponse {
        response: Some(processing_response::Response::RequestHeaders(
            HeadersResponse {
                response: Some(upstream_common_response(upstream_name, rewritten_path)),
            },
        )),
        ..ProcessingResponse::default()
    }
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

async fn spawn_new_container(
    state: &AppState,
    client_addr: &str,
    tenant_name: &str,
    plugin_name: &str,
    plugin_config: &PluginConfig,
    env: &[(String, String)],
) -> Result<PendingInit, LauncherError> {
    let mut token_bytes = [0u8; 6];
    rand::thread_rng().fill_bytes(&mut token_bytes);
    let token: String = token_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let container_name = format!("mcp_session_{token}");
    let staging_path = staging_path(tenant_name, &token);
    let start = Instant::now();
    let launch_result = launch_session(
        &state.launcher_socket_path,
        &container_name,
        &plugin_config.image,
        &staging_path,
        &plugin_config.network,
        env,
    )
    .await?;
    if let Ok(serialized) = serde_json::to_string(&launch_result) {
        log_info(&format!("launcher response: {serialized}"));
    }
    let elapsed = start.elapsed();
    let remaining = COLD_START_TIMEOUT.saturating_sub(elapsed);
    if remaining.is_zero()
        || !probe_ready(
            &container_name,
            plugin_config.port,
            std::time::Duration::from_millis(200),
            remaining,
        )
        .await
    {
        return Err(LauncherError::ProbeTimeout {
            host: container_name,
            port: plugin_config.port,
        });
    }
    log_info(&format!(
        "spawned container {} (plugin={} client={})",
        container_name, plugin_name, client_addr
    ));
    Ok(PendingInit {
        container_name,
        staging_token: token,
        tenant_name: tenant_name.to_string(),
        plugin_name: plugin_name.to_string(),
        plugin_config: plugin_config.clone(),
        created_at: utc_now(),
    })
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
                log_phase(
                    stream,
                    "request_headers",
                    "continue_get_without_session",
                    &[("authority", !stream.authority.is_empty())],
                );
                return request_headers_continue();
            }
            let transport = {
                let sessions = state.transport_sessions.lock().await;
                sessions
                    .get(stream.mcp_session_id.as_deref().unwrap())
                    .cloned()
            };
            let Some(transport) = transport else {
                log_phase(
                    stream,
                    "request_headers",
                    "continue_unknown_get_session",
                    &[],
                );
                return request_headers_continue();
            };
            if transport.tenant_name != stream.trusted_tenant {
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    403,
                    "session tenant mismatch",
                );
            }
            if let Some((path_tenant_name, path_plugin_name)) = parse_tenant_plugin(&stream.path) {
                if path_tenant_name != stream.trusted_tenant {
                    return logged_immediate_response(
                        stream,
                        "request_headers",
                        403,
                        "tenant path/header mismatch",
                    );
                }
                if path_plugin_name != transport.plugin_name {
                    log_info(&format!(
                        "session={} bound_plugin={} request_plugin={}",
                        stream.mcp_session_id.as_deref().unwrap_or(""),
                        transport.plugin_name,
                        path_plugin_name
                    ));
                }
            }
            let rewritten_path = forward_path(&stream.path, &transport.path);
            stream.chosen_upstream = Some(upstream(&transport.container_name, transport.port));
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
            let transport = {
                let sessions = state.transport_sessions.lock().await;
                sessions
                    .get(stream.mcp_session_id.as_deref().unwrap())
                    .cloned()
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
            if let Some((path_tenant_name, path_plugin_name)) = parse_tenant_plugin(&stream.path) {
                if path_tenant_name != stream.trusted_tenant {
                    return logged_immediate_response(
                        stream,
                        "request_headers",
                        403,
                        "tenant path/header mismatch",
                    );
                }
                if path_plugin_name != transport.plugin_name {
                    log_info(&format!(
                        "session={} bound_plugin={} request_plugin={}",
                        stream.mcp_session_id.as_deref().unwrap_or(""),
                        transport.plugin_name,
                        path_plugin_name
                    ));
                }
            }
            let rewritten_path = forward_path(&stream.path, &transport.path);
            stream.chosen_upstream = Some(upstream(&transport.container_name, transport.port));
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
            );
        }

        if stream.method == "POST" {
            if let Some(ref mcp_session_id) = stream.mcp_session_id {
                let transport = {
                    let sessions = state.transport_sessions.lock().await;
                    sessions.get(mcp_session_id).cloned()
                };
                if let Some(transport) = transport {
                    if transport.tenant_name != stream.trusted_tenant {
                        return logged_immediate_response(
                            stream,
                            "request_headers",
                            403,
                            "session tenant mismatch",
                        );
                    }
                    if let Some((path_tenant_name, path_plugin_name)) =
                        parse_tenant_plugin(&stream.path)
                    {
                        if path_tenant_name != stream.trusted_tenant {
                            return logged_immediate_response(
                                stream,
                                "request_headers",
                                403,
                                "tenant path/header mismatch",
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
                    stream.chosen_upstream =
                        Some(upstream(&transport.container_name, transport.port));
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
                    );
                }
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    404,
                    "unknown mcp-session-id",
                );
            }

            let Some((path_tenant_name, plugin_name)) = parse_tenant_plugin(&stream.path) else {
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    400,
                    "plugin required: use /<tenant>/<plugin>",
                );
            };

            if path_tenant_name != stream.trusted_tenant {
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    403,
                    "tenant path/header mismatch",
                );
            }
            if state.plugin_registry.is_empty() {
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    500,
                    "plugin registry not loaded",
                );
            }
            let Some(plugin_config) = state.plugin_registry.get(&plugin_name).cloned() else {
                return logged_immediate_response(
                    stream,
                    "request_headers",
                    404,
                    &format!("unknown plugin: {plugin_name}"),
                );
            };
            let rewritten_path = forward_path(&stream.path, &plugin_config.path);

            // Fail open: secret lookup failures should not break session spawn.
            let env = match stream.cap.as_deref() {
                Some(cap) if !cap.is_empty() => {
                    match secrets::fetch_secrets(
                        &state.auth_broker_url,
                        cap,
                        std::time::Duration::from_secs(5),
                    )
                    .await
                    {
                        Ok(secrets) => secrets::build_env_entries(&secrets),
                        Err(err) => {
                            log_info(&format!(
                                "spawn_secrets fetch failed tenant={} plugin={} err={err}",
                                stream.trusted_tenant, plugin_name
                            ));
                            Vec::new()
                        }
                    }
                }
                _ => Vec::new(),
            };
            log_info(&format!(
                "spawn_secrets tenant={} plugin={} cap_present={} secrets_injected={}",
                stream.trusted_tenant,
                plugin_name,
                matches!(stream.cap.as_deref(), Some(cap) if !cap.is_empty()),
                env.len()
            ));

            let pending = match spawn_new_container(
                state,
                &stream.client_addr,
                &stream.trusted_tenant,
                &plugin_name,
                &plugin_config,
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

            state
                .session_registry
                .record_spawn(
                    &pending.container_name,
                    &staging_path(&pending.tenant_name, &pending.staging_token),
                    &pending.plugin_config.image,
                    &pending.created_at,
                )
                .await;

            {
                let mut pending_init = state.pending_init.lock().await;
                pending_init.insert(stream.stream_id.clone(), pending.clone());
            }

            stream.chosen_upstream = Some(upstream(
                &pending.container_name,
                pending.plugin_config.port,
            ));
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
                    &agent_dir(&transport.tenant_name, &agent_id),
                )
                .await;
                if result.is_ok() {
                    let bound_at = utc_now();
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
                    state
                        .session_registry
                        .record_agent_bound(&transport.container_name, &agent_id, &bound_at)
                        .await;
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
                    staging_token: pending.staging_token.clone(),
                    tenant_name: pending.tenant_name.clone(),
                    plugin_name: pending.plugin_name.clone(),
                    port: pending.plugin_config.port,
                    path: pending.plugin_config.path.clone(),
                    agent_id: None,
                },
            );
        }
        state
            .session_registry
            .record_mcp_session_id(&pending.container_name, &mcp_session_id)
            .await;

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
            let mut pending_init = state.pending_init.lock().await;
            pending_init.remove(&stream_state.stream_id);
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

    #[test]
    fn parse_plugin_path_legacy_mcp() {
        let parsed = parse_plugin_path("/tenant/plugin", "/mcp").expect("valid path");
        assert_eq!(parsed.0, "tenant");
        assert_eq!(parsed.1, "plugin");
        assert_eq!(parsed.2, "/mcp");

        let parsed_with_extra = parse_plugin_path("/tenant/plugin/extra?query=1", "/mcp")
            .expect("valid path with extra");
        assert_eq!(parsed_with_extra.2, "/mcp/extra?query=1");

        assert!(parse_plugin_path("/tenant", "/mcp").is_none());
        assert!(parse_plugin_path("/Tenant/plugin", "/mcp").is_none());
        assert!(parse_plugin_path("/tenant/", "/mcp").is_none());
    }

    #[test]
    fn parse_plugin_path_root_default() {
        let parsed = parse_plugin_path("/tenant/plugin", "/").expect("valid path");
        assert_eq!(parsed.2, "/");

        let parsed_with_extra =
            parse_plugin_path("/tenant/plugin/extra?query=1", "/").expect("valid path with extra");
        assert_eq!(parsed_with_extra.2, "/extra?query=1");
    }

    #[test]
    fn parse_plugin_path_custom_prefix() {
        let parsed = parse_plugin_path("/tenant/plugin/foo", "/api/v1").expect("valid path");
        assert_eq!(parsed.2, "/api/v1/foo");
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
        let mutation = upstream_header_mutation("mcp_session_abc:8000", Some("/mcp"));
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
        let mutation = upstream_header_mutation("mcp_session_abc:8000", Some("/mcp"));
        assert!(mutation
            .remove_headers
            .iter()
            .any(|name| name == "x-botwork-cap"));
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
        assert_eq!(forward_path("/tenant/plugin", "/"), "/");
        assert_eq!(forward_path("/tenant/plugin/foo?x=1", "/"), "/foo?x=1");
        assert_eq!(forward_path("/tenant/plugin", "/mcp"), "/mcp");
        assert_eq!(
            forward_path("/tenant/plugin/foo?x=1", "/mcp"),
            "/mcp/foo?x=1"
        );
        assert_eq!(forward_path("/tenant/plugin/foo", "/api/v1"), "/api/v1/foo");
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
        let mutation = upstream_header_mutation("mcp_session_abc:8000", None);
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
        let mutation = upstream_header_mutation("mcp_session_abc:8000", None);
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
}
