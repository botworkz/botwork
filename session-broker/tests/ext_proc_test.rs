use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;

use botwork_session_broker::ext_proc::{
    upstream_header_mutation, ExternalProcessorService, PerStreamState, TeardownInfo,
};
use botwork_session_broker::plugin_registry::{PluginConfig, PluginResources, UpstreamAuth};
use botwork_session_broker::session_registry::SessionRegistry;
use botwork_session_broker::test_support::{start_log_capture, take_log_capture};
use botwork_session_broker::{AppState, PendingInit, TransportState};
use envoy_proto::envoy::config::core::v3::{HeaderMap, HeaderValue};
use envoy_proto::envoy::service::ext_proc::v3::{
    processing_response, CommonResponse, HeadersResponse, HttpBody, HttpHeaders, ProcessingResponse,
};
use tempfile::tempdir;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::Mutex;

fn session_registry_path() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| "/tmp/botwork-session-broker-ext-proc-tests-sessions.json".to_string())
        .as_str()
}

fn app_state_with_plugins(launcher_socket_path: String) -> AppState {
    app_state_with_plugins_and_auth(launcher_socket_path, "http://127.0.0.1:1".to_string())
}

fn app_state_with_plugins_and_auth(
    launcher_socket_path: String,
    auth_broker_url: String,
) -> AppState {
    app_state_with_plugins_and_auth_and_path(launcher_socket_path, auth_broker_url, "/mcp")
}

fn app_state_with_plugins_and_auth_and_path(
    launcher_socket_path: String,
    auth_broker_url: String,
    plugin_path: &str,
) -> AppState {
    app_state_with_plugins_and_auth_and_path_and_upstream_auth(
        launcher_socket_path,
        auth_broker_url,
        plugin_path,
        UpstreamAuth::None,
    )
}

fn app_state_with_plugins_and_auth_and_path_and_upstream_auth(
    launcher_socket_path: String,
    auth_broker_url: String,
    plugin_path: &str,
    upstream_auth: UpstreamAuth,
) -> AppState {
    let mut plugin_registry = HashMap::new();
    plugin_registry.insert(
        "plugin-a".to_string(),
        sample_plugin_config_with_path_and_auth(plugin_path, upstream_auth),
    );
    AppState {
        plugin_registry,
        session_registry: Arc::new(SessionRegistry::new(session_registry_path())),
        transport_sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_init: Arc::new(Mutex::new(HashMap::new())),
        launcher_socket_path,
        auth_broker_url,
    }
}

fn app_state_with_empty_plugins(launcher_socket_path: String) -> AppState {
    AppState {
        plugin_registry: HashMap::new(),
        session_registry: Arc::new(SessionRegistry::new(session_registry_path())),
        transport_sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_init: Arc::new(Mutex::new(HashMap::new())),
        launcher_socket_path,
        auth_broker_url: "http://127.0.0.1:1".to_string(),
    }
}

fn headers(values: &[(&str, &str)]) -> HttpHeaders {
    HttpHeaders {
        headers: Some(HeaderMap {
            headers: values
                .iter()
                .map(|(k, v)| HeaderValue {
                    key: (*k).to_string(),
                    value: (*v).to_string(),
                    raw_value: Default::default(),
                })
                .collect(),
        }),
        ..HttpHeaders::default()
    }
}

fn body(body: &[u8], end_of_stream: bool) -> HttpBody {
    HttpBody {
        body: body.to_vec(),
        end_of_stream,
        ..HttpBody::default()
    }
}

fn immediate_status(response: &ProcessingResponse) -> Option<u32> {
    let processing_response::Response::ImmediateResponse(immediate) = response.response.as_ref()?
    else {
        return None;
    };
    Some(immediate.status.as_ref()?.code as u32)
}

fn immediate_body(response: &ProcessingResponse) -> Option<String> {
    let processing_response::Response::ImmediateResponse(immediate) = response.response.as_ref()?
    else {
        return None;
    };
    Some(String::from_utf8_lossy(&immediate.body).to_string())
}

fn extract_upstream_mutation(response: &ProcessingResponse) -> Option<(String, Option<String>)> {
    let headers = match response.response.as_ref()? {
        processing_response::Response::RequestHeaders(h) => h,
        _ => return None,
    };
    let mutation = headers.response.as_ref()?.header_mutation.as_ref()?;
    let mut upstream = None;
    let mut path = None;
    for opt in &mutation.set_headers {
        let h = opt.header.as_ref()?;
        let value = String::from_utf8(h.raw_value.to_vec()).ok()?;
        if h.key == "x-session-upstream" {
            upstream = Some(value);
        } else if h.key == ":path" {
            path = Some(value);
        }
    }
    Some((upstream?, path))
}

fn extract_removed_headers(response: &ProcessingResponse) -> Vec<String> {
    let headers = match response.response.as_ref() {
        Some(processing_response::Response::RequestHeaders(h)) => h,
        _ => return Vec::new(),
    };
    headers
        .response
        .as_ref()
        .and_then(|common| common.header_mutation.as_ref())
        .map(|mutation| mutation.remove_headers.clone())
        .unwrap_or_default()
}

fn extract_set_header(response: &ProcessingResponse, name: &str) -> Option<String> {
    let headers = match response.response.as_ref()? {
        processing_response::Response::RequestHeaders(h) => h,
        _ => return None,
    };
    let mutation = headers.response.as_ref()?.header_mutation.as_ref()?;
    mutation
        .set_headers
        .iter()
        .filter_map(|option| option.header.as_ref())
        .find(|header| header.key == name)
        .and_then(|header| String::from_utf8(header.raw_value.clone()).ok())
}

fn response_with_auth_mutation(
    upstream_authorization: Option<&str>,
    strip_authorization: bool,
) -> ProcessingResponse {
    ProcessingResponse {
        response: Some(processing_response::Response::RequestHeaders(
            HeadersResponse {
                response: Some(CommonResponse {
                    header_mutation: Some(upstream_header_mutation(
                        "mcp_session_abc:8000",
                        Some("/mcp"),
                        upstream_authorization,
                        strip_authorization,
                    )),
                    ..CommonResponse::default()
                }),
            },
        )),
        ..ProcessingResponse::default()
    }
}

fn sample_transport(tenant: &str, plugin: &str, container: &str) -> TransportState {
    sample_transport_with_path(tenant, plugin, container, "/mcp")
}

fn sample_transport_with_path(
    tenant: &str,
    plugin: &str,
    container: &str,
    plugin_path: &str,
) -> TransportState {
    TransportState {
        container_name: container.to_string(),
        staging_token: "abcdef".to_string(),
        tenant_name: tenant.to_string(),
        plugin_name: plugin.to_string(),
        port: 8000,
        path: plugin_path.to_string(),
        upstream_authorization: None,
        agent_id: None,
    }
}

fn sample_plugin_config() -> PluginConfig {
    sample_plugin_config_with_path("/mcp")
}

fn sample_plugin_config_with_path(path: &str) -> PluginConfig {
    sample_plugin_config_with_path_and_auth(path, UpstreamAuth::None)
}

fn sample_plugin_config_with_path_and_auth(
    path: &str,
    upstream_auth: UpstreamAuth,
) -> PluginConfig {
    PluginConfig {
        image: "botwork/plugin-a:local".to_string(),
        port: 8000,
        network: "botwork".to_string(),
        path: path.to_string(),
        upstream_auth,
        env: vec![],
        resources: PluginResources::default(),
    }
}

fn sample_pending(tenant: &str, plugin: &str, container: &str) -> PendingInit {
    PendingInit {
        container_name: container.to_string(),
        staging_token: "abcdef".to_string(),
        tenant_name: tenant.to_string(),
        plugin_name: plugin.to_string(),
        plugin_config: sample_plugin_config(),
        upstream_authorization: None,
        created_at: "2026-01-01T00:00:00Z".to_string(),
    }
}

async fn insert_transport(state: &AppState, mcp_session_id: &str, transport: TransportState) {
    state
        .transport_sessions
        .lock()
        .await
        .insert(mcp_session_id.to_string(), transport);
}

async fn insert_pending(state: &AppState, stream_id: &str, pending: PendingInit) {
    state
        .pending_init
        .lock()
        .await
        .insert(stream_id.to_string(), pending);
}

#[tokio::test]
async fn request_headers_invalid_tenant_format_returns_400() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "BadTenant"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(400));
}

#[tokio::test]
async fn request_headers_get_without_session_returns_400() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "GET"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(400));
    assert_eq!(extract_upstream_mutation(&response), None);
}

#[tokio::test]
async fn request_headers_get_unknown_session_returns_404() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "GET"),
            (":path", "/tenant1/plugin-a/foo"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-unknown"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(404));
    assert_eq!(extract_upstream_mutation(&response), None);
}

#[tokio::test]
async fn request_headers_get_known_session_routes_to_upstream() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    insert_transport(
        &state,
        "sess-1",
        sample_transport("tenant1", "plugin-a", "mcp_session_abc"),
    )
    .await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "GET"),
            (":path", "/tenant1/plugin-a/foo"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    assert_eq!(
        extract_upstream_mutation(&response),
        Some((
            "mcp_session_abc:8000".to_string(),
            Some("/mcp/foo".to_string())
        ))
    );
}

#[tokio::test]
async fn request_headers_get_known_session_routes_to_upstream_root_path() {
    let state = app_state_with_plugins_and_auth_and_path(
        "/tmp/no-launcher.sock".to_string(),
        "http://127.0.0.1:1".to_string(),
        "/",
    );
    insert_transport(
        &state,
        "sess-1",
        sample_transport_with_path("tenant1", "plugin-a", "mcp_session_abc", "/"),
    )
    .await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "GET"),
            (":path", "/tenant1/plugin-a/foo"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    assert_eq!(
        extract_upstream_mutation(&response),
        Some(("mcp_session_abc:8000".to_string(), Some("/foo".to_string())))
    );
}

#[tokio::test]
async fn request_headers_strips_x_botwork_cap_on_get_route() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    insert_transport(
        &state,
        "sess-1",
        sample_transport("tenant1", "plugin-a", "mcp_session_abc"),
    )
    .await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "GET"),
            (":path", "/tenant1/plugin-a/foo"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    assert!(extract_removed_headers(&response).contains(&"x-botwork-cap".to_string()));
}

#[tokio::test]
async fn request_headers_get_tenant_mismatch_returns_403() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    insert_transport(
        &state,
        "sess-1",
        sample_transport("tenant1", "plugin-a", "mcp_session_abc"),
    )
    .await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "GET"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant2"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(403));
}

#[tokio::test]
async fn request_headers_get_path_tenant_mismatch_returns_403() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    insert_transport(
        &state,
        "sess-1",
        sample_transport("tenant1", "plugin-a", "mcp_session_abc"),
    )
    .await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "GET"),
            (":path", "/tenant2/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(403));
}

#[tokio::test]
async fn request_headers_delete_without_session_returns_404() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "DELETE"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(404));
}

#[tokio::test]
async fn request_headers_delete_unknown_session_returns_404() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "DELETE"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-unknown"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(404));
}

#[tokio::test]
async fn request_headers_delete_known_session_routes_to_upstream() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    insert_transport(
        &state,
        "sess-1",
        sample_transport("tenant1", "plugin-a", "mcp_session_abc"),
    )
    .await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "DELETE"),
            (":path", "/tenant1/plugin-a/foo"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    assert_eq!(
        extract_upstream_mutation(&response),
        Some((
            "mcp_session_abc:8000".to_string(),
            Some("/mcp/foo".to_string())
        ))
    );
}

#[tokio::test]
async fn request_headers_post_unknown_session_returns_404() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-unknown"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(404));
}

#[tokio::test]
async fn request_headers_post_known_session_routes_to_upstream() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    insert_transport(
        &state,
        "sess-1",
        sample_transport("tenant1", "plugin-a", "mcp_session_abc"),
    )
    .await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a/foo"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    assert_eq!(
        extract_upstream_mutation(&response),
        Some((
            "mcp_session_abc:8000".to_string(),
            Some("/mcp/foo".to_string())
        ))
    );
}

#[tokio::test]
async fn request_headers_strips_x_botwork_cap_on_existing_session_route() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    insert_transport(
        &state,
        "sess-1",
        sample_transport("tenant1", "plugin-a", "mcp_session_abc"),
    )
    .await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a/foo"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    assert!(extract_removed_headers(&response).contains(&"x-botwork-cap".to_string()));
}

#[tokio::test]
async fn request_headers_strips_authorization_when_upstream_auth_none_existing_session() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    insert_transport(
        &state,
        "sess-1",
        sample_transport("tenant1", "plugin-a", "mcp_session_abc"),
    )
    .await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a/foo"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    let removed = extract_removed_headers(&response);
    assert!(removed.contains(&"authorization".to_string()));
    assert!(removed.contains(&"x-botwork-cap".to_string()));
}

#[tokio::test]
async fn request_headers_existing_session_with_cached_bearer_emits_set_authorization() {
    let state = app_state_with_plugins_and_auth_and_path_and_upstream_auth(
        "/tmp/no-launcher.sock".to_string(),
        "http://127.0.0.1:1".to_string(),
        "/mcp",
        UpstreamAuth::Bearer {
            service: "github.com".to_string(),
        },
    );
    let mut transport = sample_transport("tenant1", "plugin-a", "mcp_session_abc");
    transport.upstream_authorization = Some("ghp_CACHED".to_string());
    insert_transport(&state, "sess-1", transport).await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a/foo"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    assert_eq!(
        extract_set_header(&response, "authorization").as_deref(),
        Some("Bearer ghp_CACHED")
    );
    let removed = extract_removed_headers(&response);
    assert!(!removed.contains(&"authorization".to_string()));
    assert!(removed.contains(&"x-botwork-cap".to_string()));
}

#[tokio::test]
async fn request_headers_existing_session_without_cached_bearer_strips_authorization() {
    let state = app_state_with_plugins_and_auth_and_path_and_upstream_auth(
        "/tmp/no-launcher.sock".to_string(),
        "http://127.0.0.1:1".to_string(),
        "/mcp",
        UpstreamAuth::Bearer {
            service: "github.com".to_string(),
        },
    );
    insert_transport(
        &state,
        "sess-1",
        sample_transport("tenant1", "plugin-a", "mcp_session_abc"),
    )
    .await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a/foo"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    assert_eq!(extract_set_header(&response, "authorization"), None);
    let removed = extract_removed_headers(&response);
    assert!(removed.contains(&"authorization".to_string()));
    assert!(removed.contains(&"x-botwork-cap".to_string()));
}

#[tokio::test]
async fn request_headers_existing_session_missing_from_registry_falls_back_to_strip() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut transport = sample_transport("tenant1", "plugin-missing", "mcp_session_abc");
    transport.upstream_authorization = Some("ghp_CACHED".to_string());
    insert_transport(&state, "sess-1", transport).await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a/foo"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), None);
    let removed = extract_removed_headers(&response);
    assert!(removed.contains(&"authorization".to_string()));
    assert!(removed.contains(&"x-botwork-cap".to_string()));
}

#[tokio::test]
async fn request_headers_post_known_session_tenant_mismatch_returns_403() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    insert_transport(
        &state,
        "sess-1",
        sample_transport("tenant1", "plugin-a", "mcp_session_abc"),
    )
    .await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant2"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(403));
}

#[tokio::test]
async fn request_headers_post_initialize_without_path_plugin_returns_400() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/something"),
            ("x-botwork-tenant", "tenant1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(400));
}

#[tokio::test]
async fn request_headers_post_initialize_path_tenant_mismatch_returns_403() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant2/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(403));
}

#[tokio::test]
async fn request_headers_post_initialize_empty_plugin_registry_returns_500() {
    let state = app_state_with_empty_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(500));
}

#[tokio::test]
async fn request_headers_post_initialize_unknown_plugin_returns_404() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-x"),
            ("x-botwork-tenant", "tenant1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(404));
    assert!(immediate_body(&response)
        .unwrap_or_default()
        .contains("unknown plugin: plugin-x"));
}

#[tokio::test]
async fn request_body_non_post_returns_continue() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState {
        method: "GET".to_string(),
        ..PerStreamState::default()
    };
    let response =
        ExternalProcessorService::handle_request_body(&state, &mut stream, body(b"abc", true))
            .await;

    assert!(matches!(
        response.response,
        Some(processing_response::Response::RequestBody(_))
    ));
}

#[tokio::test]
async fn request_body_buffering_returns_continue() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState {
        method: "POST".to_string(),
        ..PerStreamState::default()
    };
    let first =
        ExternalProcessorService::handle_request_body(&state, &mut stream, body(b"abc", false))
            .await;
    let second =
        ExternalProcessorService::handle_request_body(&state, &mut stream, body(b"{}", true)).await;

    assert!(matches!(
        first.response,
        Some(processing_response::Response::RequestBody(_))
    ));
    assert!(matches!(
        second.response,
        Some(processing_response::Response::RequestBody(_))
    ));
    assert_eq!(stream.request_body, b"abc{}");
}

#[tokio::test]
async fn request_body_invalid_json_returns_continue() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState {
        method: "POST".to_string(),
        content_type_is_json: true,
        mcp_session_id: Some("s1".to_string()),
        ..PerStreamState::default()
    };
    let response =
        ExternalProcessorService::handle_request_body(&state, &mut stream, body(b"not json", true))
            .await;

    assert!(matches!(
        response.response,
        Some(processing_response::Response::RequestBody(_))
    ));
}

#[tokio::test]
async fn request_body_no_agent_session_id_returns_continue() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState {
        method: "POST".to_string(),
        content_type_is_json: true,
        mcp_session_id: Some("s1".to_string()),
        ..PerStreamState::default()
    };
    let response = ExternalProcessorService::handle_request_body(
        &state,
        &mut stream,
        body(br#"{"params": {}}"#, true),
    )
    .await;

    assert!(matches!(
        response.response,
        Some(processing_response::Response::RequestBody(_))
    ));
}

#[tokio::test]
async fn request_body_with_agent_session_id_attempts_bind_then_continues() {
    let temp = tempdir().unwrap();
    let missing_socket = temp.path().join("missing.sock");
    let state = app_state_with_plugins(missing_socket.to_string_lossy().to_string());
    insert_transport(
        &state,
        "sess-1",
        sample_transport("tenant1", "plugin-a", "mcp_session_abc"),
    )
    .await;

    let mut stream = PerStreamState {
        method: "POST".to_string(),
        content_type_is_json: true,
        mcp_session_id: Some("sess-1".to_string()),
        ..PerStreamState::default()
    };
    let response = ExternalProcessorService::handle_request_body(
        &state,
        &mut stream,
        body(
            br#"{"params": {"_meta": {"agent-session-id": "agent-1"}}}"#,
            true,
        ),
    )
    .await;

    assert!(matches!(
        response.response,
        Some(processing_response::Response::RequestBody(_))
    ));
    let sessions = state.transport_sessions.lock().await;
    assert_eq!(
        sessions
            .get("sess-1")
            .and_then(|transport| transport.agent_id.clone()),
        None
    );
}

#[tokio::test]
async fn request_body_invalid_agent_session_id_type_returns_continue() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState {
        method: "POST".to_string(),
        content_type_is_json: true,
        mcp_session_id: Some("sess-1".to_string()),
        ..PerStreamState::default()
    };
    let response = ExternalProcessorService::handle_request_body(
        &state,
        &mut stream,
        body(br#"{"params": {"_meta": {"agent-session-id": 42}}}"#, true),
    )
    .await;

    assert!(matches!(
        response.response,
        Some(processing_response::Response::RequestBody(_))
    ));
}

#[tokio::test]
async fn response_headers_no_pending_returns_continue() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_response_headers(
        &state,
        &mut stream,
        headers(&[(":status", "200")]),
    )
    .await;

    assert!(matches!(
        response.response,
        Some(processing_response::Response::ResponseHeaders(_))
    ));
    assert!(state.transport_sessions.lock().await.is_empty());
}

#[tokio::test]
async fn response_headers_pending_with_session_id_creates_transport() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let stream_id = "stream-1";
    let mut pending = sample_pending("tenant1", "plugin-a", "mcp_session_abc");
    pending.upstream_authorization = Some("ghp_PENDING".to_string());
    insert_pending(&state, stream_id, pending).await;
    let mut stream = PerStreamState {
        stream_id: stream_id.to_string(),
        ..PerStreamState::default()
    };
    let response = ExternalProcessorService::handle_response_headers(
        &state,
        &mut stream,
        headers(&[(":status", "200"), ("mcp-session-id", "sess-new")]),
    )
    .await;

    assert!(matches!(
        response.response,
        Some(processing_response::Response::ResponseHeaders(_))
    ));
    assert!(state.pending_init.lock().await.get(stream_id).is_none());
    let sessions = state.transport_sessions.lock().await;
    let transport = sessions.get("sess-new").expect("transport for sess-new");
    assert_eq!(transport.container_name, "mcp_session_abc");
    assert_eq!(transport.plugin_name, "plugin-a");
    assert_eq!(transport.port, 8000);
    assert_eq!(transport.path, "/mcp");
    assert_eq!(
        transport.upstream_authorization.as_deref(),
        Some("ghp_PENDING")
    );
}

#[tokio::test]
async fn response_headers_pending_missing_session_id_discards() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let stream_id = "stream-1";
    insert_pending(
        &state,
        stream_id,
        sample_pending("tenant1", "plugin-a", "mcp_session_abc"),
    )
    .await;
    let mut stream = PerStreamState {
        stream_id: stream_id.to_string(),
        ..PerStreamState::default()
    };
    let response = ExternalProcessorService::handle_response_headers(
        &state,
        &mut stream,
        headers(&[(":status", "200")]),
    )
    .await;

    assert!(matches!(
        response.response,
        Some(processing_response::Response::ResponseHeaders(_))
    ));
    assert!(state.pending_init.lock().await.get(stream_id).is_none());
    assert!(state.transport_sessions.lock().await.is_empty());
}

#[tokio::test]
async fn spawn_passes_cap_to_secrets_fetch_and_envs_to_launcher() {
    let temp = tempdir().unwrap();
    let socket_path = temp.path().join("launcher.sock");
    let launcher_body = Arc::new(Mutex::new(None));
    let launcher_task = spawn_launcher_capture(
        &socket_path,
        500,
        r#"{"error":"boom"}"#,
        Arc::clone(&launcher_body),
    )
    .await;

    let captured_cap = Arc::new(Mutex::new(None));
    let auth_url = spawn_auth_broker_capture(
        200,
        r#"{"tenant":"tenant1","plugin":"plugin-a","secrets":[{"service":"github.com","name":"pat","kind":"api-key","value_b64":"Z2hwX3h4eA=="},{"service":"shared","name":"secret","kind":"opaque","value_b64":"YW5vdGhlcg=="}]}"#,
        Arc::clone(&captured_cap),
    )
    .await;

    let state = app_state_with_plugins_and_auth(path_to_string(&socket_path), auth_url);
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("x-botwork-cap", "cap-123"),
        ]),
    )
    .await;
    launcher_task.await.unwrap();

    assert_eq!(immediate_status(&response), Some(502));
    assert_eq!(
        captured_cap.lock().await.clone().as_deref(),
        Some("cap-123")
    );

    let launcher_payload: serde_json::Value =
        serde_json::from_str(&launcher_body.lock().await.clone().expect("launcher body"))
            .expect("launcher json");
    let env = launcher_payload["env"].as_array().expect("env array");
    let env_names: Vec<&str> = env
        .iter()
        .map(|entry| entry["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        env_names,
        vec![
            "BOTWORK_SECRET_GITHUB_COM_PAT",
            "BOTWORK_SECRET_SHARED_SECRET"
        ]
    );
}

#[tokio::test]
async fn spawn_without_cap_fetches_no_secrets_and_passes_empty_env() {
    let temp = tempdir().unwrap();
    let socket_path = temp.path().join("launcher.sock");
    let launcher_body = Arc::new(Mutex::new(None));
    let launcher_task = spawn_launcher_capture(
        &socket_path,
        500,
        r#"{"error":"boom"}"#,
        Arc::clone(&launcher_body),
    )
    .await;

    let state = app_state_with_plugins(path_to_string(&socket_path));
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
        ]),
    )
    .await;
    launcher_task.await.unwrap();
    assert_eq!(immediate_status(&response), Some(502));
    let launcher_payload: serde_json::Value =
        serde_json::from_str(&launcher_body.lock().await.clone().expect("launcher body"))
            .expect("launcher json");
    assert!(launcher_payload.get("env").is_none());
}

#[tokio::test]
async fn request_headers_strips_authorization_when_upstream_auth_none_spawn_path() {
    let temp = tempdir().unwrap();
    let socket_path = temp.path().join("launcher.sock");
    let launcher_body = Arc::new(Mutex::new(None));
    let launcher_task = spawn_launcher_capture(
        &socket_path,
        500,
        r#"{"error":"boom"}"#,
        Arc::clone(&launcher_body),
    )
    .await;
    let state = app_state_with_plugins_and_auth_and_path_and_upstream_auth(
        path_to_string(&socket_path),
        "http://127.0.0.1:1".to_string(),
        "/mcp",
        UpstreamAuth::None,
    );
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
        ]),
    )
    .await;
    launcher_task.await.unwrap();

    assert_eq!(immediate_status(&response), Some(502));
    let removed = extract_removed_headers(&response_with_auth_mutation(None, true));
    assert!(removed.contains(&"authorization".to_string()));
    assert!(removed.contains(&"x-botwork-cap".to_string()));
}

#[tokio::test]
async fn request_headers_bearer_one_match_emits_set_authorization() {
    let response = response_with_auth_mutation(Some("ghp_TEST"), false);
    assert_eq!(
        extract_set_header(&response, "authorization").as_deref(),
        Some("Bearer ghp_TEST")
    );
    let removed = extract_removed_headers(&response);
    assert!(!removed.contains(&"authorization".to_string()));
    assert!(removed.contains(&"x-botwork-cap".to_string()));
}

#[tokio::test]
async fn request_headers_bearer_no_match_returns_5xx_no_spawn() {
    let auth_url = spawn_auth_broker_capture(
        200,
        r#"{"tenant":"tenant1","plugin":"plugin-a","secrets":[{"service":"shared","name":"secret","kind":"opaque","value_b64":"YWJj"}]}"#,
        Arc::new(Mutex::new(None)),
    )
    .await;
    let state = app_state_with_plugins_and_auth_and_path_and_upstream_auth(
        "/tmp/missing-launcher.sock".to_string(),
        auth_url,
        "/mcp",
        UpstreamAuth::Bearer {
            service: "github.com".to_string(),
        },
    );
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("x-botwork-cap", "cap-123"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(500));
    assert!(immediate_body(&response)
        .unwrap_or_default()
        .contains("configured upstream authorization secret was not found"));
}

#[tokio::test]
async fn request_headers_bearer_multiple_matches_returns_5xx() {
    let auth_url = spawn_auth_broker_capture(
        200,
        r#"{"tenant":"tenant1","plugin":"plugin-a","secrets":[{"service":"github.com","name":"pat","kind":"api-key","value_b64":"Z2hwX09ORQ=="},{"service":"github.com","name":"pat2","kind":"api-key","value_b64":"Z2hwX1RXTw=="}]}"#,
        Arc::new(Mutex::new(None)),
    )
    .await;
    let state = app_state_with_plugins_and_auth_and_path_and_upstream_auth(
        "/tmp/missing-launcher.sock".to_string(),
        auth_url,
        "/mcp",
        UpstreamAuth::Bearer {
            service: "github.com".to_string(),
        },
    );
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("x-botwork-cap", "cap-123"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(500));
    assert!(immediate_body(&response)
        .unwrap_or_default()
        .contains("ambiguous upstream authorization secret"));
}

#[tokio::test]
async fn request_headers_bearer_non_utf8_secret_returns_5xx() {
    let auth_url = spawn_auth_broker_capture(
        200,
        r#"{"tenant":"tenant1","plugin":"plugin-a","secrets":[{"service":"github.com","name":"pat","kind":"api-key","value_b64":"//4="}]}"#,
        Arc::new(Mutex::new(None)),
    )
    .await;
    let state = app_state_with_plugins_and_auth_and_path_and_upstream_auth(
        "/tmp/missing-launcher.sock".to_string(),
        auth_url,
        "/mcp",
        UpstreamAuth::Bearer {
            service: "github.com".to_string(),
        },
    );
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("x-botwork-cap", "cap-123"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(500));
    assert!(immediate_body(&response)
        .unwrap_or_default()
        .contains("must be valid UTF-8"));
}

#[tokio::test]
async fn spawn_with_cap_but_auth_broker_unreachable_continues_with_empty_env() {
    let temp = tempdir().unwrap();
    let socket_path = temp.path().join("launcher.sock");
    let launcher_body = Arc::new(Mutex::new(None));
    let launcher_task = spawn_launcher_capture(
        &socket_path,
        500,
        r#"{"error":"boom"}"#,
        Arc::clone(&launcher_body),
    )
    .await;
    let state = app_state_with_plugins_and_auth(
        path_to_string(&socket_path),
        "http://127.0.0.1:1".to_string(),
    );
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("x-botwork-cap", "cap-123"),
        ]),
    )
    .await;
    launcher_task.await.unwrap();
    assert_eq!(immediate_status(&response), Some(502));
    let launcher_payload: serde_json::Value =
        serde_json::from_str(&launcher_body.lock().await.clone().expect("launcher body"))
            .expect("launcher json");
    assert!(launcher_payload.get("env").is_none());
}

#[tokio::test]
async fn spawn_with_cap_but_auth_broker_401_continues_with_empty_env() {
    let temp = tempdir().unwrap();
    let socket_path = temp.path().join("launcher.sock");
    let launcher_body = Arc::new(Mutex::new(None));
    let launcher_task = spawn_launcher_capture(
        &socket_path,
        500,
        r#"{"error":"boom"}"#,
        Arc::clone(&launcher_body),
    )
    .await;
    let auth_url = spawn_auth_broker_capture(401, "{}", Arc::new(Mutex::new(None))).await;
    let state = app_state_with_plugins_and_auth(path_to_string(&socket_path), auth_url);
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("x-botwork-cap", "cap-123"),
        ]),
    )
    .await;
    launcher_task.await.unwrap();
    assert_eq!(immediate_status(&response), Some(502));
    let launcher_payload: serde_json::Value =
        serde_json::from_str(&launcher_body.lock().await.clone().expect("launcher body"))
            .expect("launcher json");
    assert!(launcher_payload.get("env").is_none());
}

#[tokio::test]
async fn cap_present_in_per_stream_state_after_request_headers() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState::default();
    let _ = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "GET"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("x-botwork-cap", "cap-123"),
        ]),
    )
    .await;
    assert_eq!(stream.cap.as_deref(), Some("cap-123"));
}

#[tokio::test]
async fn bearer_value_not_logged_in_clear() {
    let auth_url = spawn_auth_broker_capture(
        200,
        r#"{"tenant":"tenant1","plugin":"plugin-a","secrets":[{"service":"github.com","name":"pat","kind":"api-key","value_b64":"Z2hwX1NFQ1JFVA=="}]}"#,
        Arc::new(Mutex::new(None)),
    )
    .await;
    let state = app_state_with_plugins_and_auth_and_path_and_upstream_auth(
        "/tmp/missing-launcher.sock".to_string(),
        auth_url,
        "/mcp",
        UpstreamAuth::Bearer {
            service: "github.com".to_string(),
        },
    );

    start_log_capture();
    let mut stream = PerStreamState::default();
    let _ = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("x-botwork-cap", "cap-123"),
        ]),
    )
    .await;
    let logs = take_log_capture().join("\n");

    assert!(
        !logs.contains("ghp_SECRET"),
        "logs should redact bearer values: {logs}"
    );
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn app_state_with_plugin_env(
    launcher_socket_path: String,
    auth_broker_url: String,
    static_env: Vec<(String, String)>,
) -> AppState {
    app_state_with_plugin_env_and_resources(
        launcher_socket_path,
        auth_broker_url,
        static_env,
        PluginResources::default(),
    )
}

fn app_state_with_plugin_env_and_resources(
    launcher_socket_path: String,
    auth_broker_url: String,
    static_env: Vec<(String, String)>,
    resources: PluginResources,
) -> AppState {
    let mut plugin_registry = HashMap::new();
    plugin_registry.insert(
        "plugin-a".to_string(),
        PluginConfig {
            image: "botwork/plugin-a:local".to_string(),
            port: 8000,
            network: "botwork".to_string(),
            path: "/mcp".to_string(),
            upstream_auth: UpstreamAuth::None,
            env: static_env,
            resources,
        },
    );
    AppState {
        plugin_registry,
        session_registry: Arc::new(SessionRegistry::new(session_registry_path())),
        transport_sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_init: Arc::new(Mutex::new(HashMap::new())),
        launcher_socket_path,
        auth_broker_url,
    }
}

#[tokio::test]
async fn spawn_static_env_appears_in_launcher_payload() {
    let temp = tempdir().unwrap();
    let socket_path = temp.path().join("launcher.sock");
    let launcher_body = Arc::new(Mutex::new(None));
    let launcher_task = spawn_launcher_capture(
        &socket_path,
        500,
        r#"{"error":"boom"}"#,
        Arc::clone(&launcher_body),
    )
    .await;

    let state = app_state_with_plugin_env(
        path_to_string(&socket_path),
        "http://127.0.0.1:1".to_string(),
        vec![
            ("GITHUB_TOOLSETS".to_string(), "default,actions".to_string()),
            ("GITHUB_TERSE_DESCRIPTIONS".to_string(), "true".to_string()),
        ],
    );
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
        ]),
    )
    .await;
    launcher_task.await.unwrap();
    assert_eq!(immediate_status(&response), Some(502));

    let launcher_payload: serde_json::Value =
        serde_json::from_str(&launcher_body.lock().await.clone().expect("launcher body"))
            .expect("launcher json");
    let env = launcher_payload["env"].as_array().expect("env array");
    let env_names: Vec<&str> = env
        .iter()
        .map(|entry| entry["name"].as_str().expect("name"))
        .collect();
    assert!(
        env_names.contains(&"GITHUB_TOOLSETS"),
        "expected GITHUB_TOOLSETS in env: {env_names:?}"
    );
    assert!(
        env_names.contains(&"GITHUB_TERSE_DESCRIPTIONS"),
        "expected GITHUB_TERSE_DESCRIPTIONS in env: {env_names:?}"
    );
    let toolsets_entry = env
        .iter()
        .find(|e| e["name"] == "GITHUB_TOOLSETS")
        .expect("GITHUB_TOOLSETS entry");
    assert_eq!(toolsets_entry["value"], "default,actions");
}

#[tokio::test]
async fn spawn_plugin_resources_appear_in_launcher_payload() {
    let temp = tempdir().unwrap();
    let socket_path = temp.path().join("launcher.sock");
    let launcher_body = Arc::new(Mutex::new(None));
    let launcher_task = spawn_launcher_capture(
        &socket_path,
        500,
        r#"{"error":"boom"}"#,
        Arc::clone(&launcher_body),
    )
    .await;

    let state = app_state_with_plugin_env_and_resources(
        path_to_string(&socket_path),
        "http://127.0.0.1:1".to_string(),
        vec![],
        PluginResources {
            cpus: Some("4.0".to_string()),
            memory: Some("4g".to_string()),
            pids: Some(1024),
        },
    );
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
        ]),
    )
    .await;
    launcher_task.await.unwrap();
    assert_eq!(immediate_status(&response), Some(502));

    let launcher_payload: serde_json::Value =
        serde_json::from_str(&launcher_body.lock().await.clone().expect("launcher body"))
            .expect("launcher json");
    assert_eq!(launcher_payload["resources"]["cpus"], "4.0");
    assert_eq!(launcher_payload["resources"]["memory"], "4g");
    assert_eq!(launcher_payload["resources"]["pids"], 1024);
}

#[tokio::test]
async fn spawn_static_env_appears_before_secrets() {
    let temp = tempdir().unwrap();
    let socket_path = temp.path().join("launcher.sock");
    let launcher_body = Arc::new(Mutex::new(None));
    let launcher_task = spawn_launcher_capture(
        &socket_path,
        500,
        r#"{"error":"boom"}"#,
        Arc::clone(&launcher_body),
    )
    .await;

    let auth_url = spawn_auth_broker_capture(
        200,
        r#"{"tenant":"tenant1","plugin":"plugin-a","secrets":[{"service":"github.com","name":"pat","kind":"api-key","value_b64":"Z2hwX3h4eA=="}]}"#,
        Arc::new(Mutex::new(None)),
    )
    .await;

    let state = app_state_with_plugin_env(
        path_to_string(&socket_path),
        auth_url,
        vec![("GITHUB_TOOLSETS".to_string(), "default,actions".to_string())],
    );
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("x-botwork-cap", "cap-123"),
        ]),
    )
    .await;
    launcher_task.await.unwrap();
    assert_eq!(immediate_status(&response), Some(502));

    let launcher_payload: serde_json::Value =
        serde_json::from_str(&launcher_body.lock().await.clone().expect("launcher body"))
            .expect("launcher json");
    let env = launcher_payload["env"].as_array().expect("env array");
    let env_names: Vec<&str> = env
        .iter()
        .map(|entry| entry["name"].as_str().expect("name"))
        .collect();
    // Static env first, then secrets.
    assert_eq!(
        env_names,
        vec!["GITHUB_TOOLSETS", "BOTWORK_SECRET_GITHUB_COM_PAT"],
        "static env must precede secrets"
    );
}

#[tokio::test]
async fn spawn_static_env_present_when_no_cap() {
    let temp = tempdir().unwrap();
    let socket_path = temp.path().join("launcher.sock");
    let launcher_body = Arc::new(Mutex::new(None));
    let launcher_task = spawn_launcher_capture(
        &socket_path,
        500,
        r#"{"error":"boom"}"#,
        Arc::clone(&launcher_body),
    )
    .await;

    let state = app_state_with_plugin_env(
        path_to_string(&socket_path),
        "http://127.0.0.1:1".to_string(),
        vec![("GITHUB_TOOLSETS".to_string(), "default,actions".to_string())],
    );
    let mut stream = PerStreamState::default();
    // No x-botwork-cap header — no secrets fetch.
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
        ]),
    )
    .await;
    launcher_task.await.unwrap();
    assert_eq!(immediate_status(&response), Some(502));

    let launcher_payload: serde_json::Value =
        serde_json::from_str(&launcher_body.lock().await.clone().expect("launcher body"))
            .expect("launcher json");
    let env = launcher_payload["env"].as_array().expect("env array");
    assert_eq!(env.len(), 1);
    assert_eq!(env[0]["name"], "GITHUB_TOOLSETS");
    assert_eq!(env[0]["value"], "default,actions");
}

async fn spawn_launcher_capture(
    socket_path: &Path,
    status_code: u16,
    body: &'static str,
    captured_body: Arc<Mutex<Option<String>>>,
) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(socket_path).expect("bind launcher socket");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept launcher request");
        let request = read_unix_http_request(&mut stream).await;
        let request_body = request
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        *captured_body.lock().await = Some(request_body);
        let reason = if status_code == 200 {
            "OK"
        } else {
            "Internal Server Error"
        };
        let response = format!(
            "HTTP/1.1 {status_code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write launcher response");
    })
}

async fn spawn_auth_broker_capture(
    status_code: u16,
    body: &'static str,
    captured_cap: Arc<Mutex<Option<String>>>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind auth broker");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept auth request");
        let request = read_tcp_http_request(&mut stream).await;
        *captured_cap.lock().await = extract_header_value(&request, "x-botwork-cap");
        let reason = if status_code == 200 {
            "OK"
        } else {
            "Unauthorized"
        };
        let response = format!(
            "HTTP/1.1 {status_code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write auth response");
    });
    format!("http://{addr}")
}

async fn read_unix_http_request(stream: &mut UnixStream) -> String {
    read_http_request_impl(stream).await
}

async fn read_tcp_http_request(stream: &mut tokio::net::TcpStream) -> String {
    read_http_request_impl(stream).await
}

async fn read_http_request_impl<S>(stream: &mut S) -> String
where
    S: AsyncRead + Unpin,
{
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

fn extract_header_value(request: &str, header_name: &str) -> Option<String> {
    request.split("\r\n").find_map(|line| {
        line.split_once(':').and_then(|(name, value)| {
            if name.eq_ignore_ascii_case(header_name) {
                Some(value.trim().to_string())
            } else {
                None
            }
        })
    })
}

fn app_state_with_plugins_and_registry(
    launcher_socket_path: String,
    registry: Arc<SessionRegistry>,
) -> AppState {
    let mut plugin_registry = HashMap::new();
    plugin_registry.insert("plugin-a".to_string(), sample_plugin_config());
    AppState {
        plugin_registry,
        session_registry: registry,
        transport_sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_init: Arc::new(Mutex::new(HashMap::new())),
        launcher_socket_path,
        auth_broker_url: "http://127.0.0.1:1".to_string(),
    }
}

// ---------------------------------------------------------------------------
// DELETE teardown tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn request_headers_delete_known_session_sets_teardown_on_response() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    insert_transport(
        &state,
        "sess-1",
        sample_transport("tenant1", "plugin-a", "mcp_session_abc"),
    )
    .await;
    let mut stream = PerStreamState::default();
    let _ = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "DELETE"),
            (":path", "/tenant1/plugin-a/foo"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    let teardown = stream
        .teardown_on_response
        .as_ref()
        .expect("teardown_on_response should be set after DELETE");
    assert_eq!(teardown.mcp_session_id, "sess-1");
    assert_eq!(teardown.container_name, "mcp_session_abc");
    assert!(
        teardown.staging_path.contains("abcdef"),
        "staging path should contain the staging token: {}",
        teardown.staging_path
    );
}

#[tokio::test]
async fn request_headers_delete_without_session_does_not_set_teardown() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "DELETE"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(404));
    assert!(stream.teardown_on_response.is_none());
}

#[tokio::test]
async fn request_headers_delete_tenant_mismatch_does_not_set_teardown() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    insert_transport(
        &state,
        "sess-1",
        sample_transport("tenant1", "plugin-a", "mcp_session_abc"),
    )
    .await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "DELETE"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant2"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(403));
    assert!(stream.teardown_on_response.is_none());
}

#[tokio::test]
async fn response_headers_delete_teardown_drops_session_and_calls_launcher() {
    let temp_dir = tempdir().unwrap();
    let socket_path = temp_dir.path().join("launcher.sock");
    let launcher_body = Arc::new(Mutex::new(None));
    let launcher_task = spawn_launcher_capture(
        &socket_path,
        200,
        r#"{"status":"torn_down"}"#,
        Arc::clone(&launcher_body),
    )
    .await;

    let registry_path = temp_dir.path().join("sessions.json");
    let registry = Arc::new(SessionRegistry::new(registry_path.to_str().unwrap()));
    registry
        .record_spawn(
            "mcp_session_abc",
            "/var/lib/botwork/tenants/tenant1/staging/abcdef",
            "botwork/plugin-a:local",
            "2026-01-01T00:00:00Z",
        )
        .await;

    let state = app_state_with_plugins_and_registry(path_to_string(&socket_path), registry);
    insert_transport(
        &state,
        "sess-1",
        sample_transport("tenant1", "plugin-a", "mcp_session_abc"),
    )
    .await;

    let mut stream = PerStreamState::default();
    let _ = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "DELETE"),
            (":path", "/tenant1/plugin-a/foo"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;
    assert!(stream.teardown_on_response.is_some());

    let response = ExternalProcessorService::handle_response_headers(
        &state,
        &mut stream,
        headers(&[(":status", "200")]),
    )
    .await;
    launcher_task.await.unwrap();

    // teardown_on_response consumed
    assert!(stream.teardown_on_response.is_none());

    // launcher was called with correct payload
    let body_str = launcher_body
        .lock()
        .await
        .clone()
        .expect("launcher should have been called");
    let payload: serde_json::Value = serde_json::from_str(&body_str).expect("valid json");
    assert_eq!(payload["name"], "mcp_session_abc");
    assert!(
        payload["staging_path"].as_str().unwrap().contains("abcdef"),
        "staging path in launcher payload: {}",
        payload["staging_path"]
    );

    // transport_sessions entry removed
    assert!(state
        .transport_sessions
        .lock()
        .await
        .get("sess-1")
        .is_none());

    // session_registry entry removed
    let reg_data = state.session_registry.read().await;
    assert!(
        !reg_data.sessions.contains_key("mcp_session_abc"),
        "registry should not contain mcp_session_abc after teardown"
    );

    // response still continues normally
    assert!(matches!(
        response.response,
        Some(processing_response::Response::ResponseHeaders(_))
    ));
}

#[tokio::test]
async fn response_headers_delete_teardown_called_on_5xx_upstream() {
    let temp_dir = tempdir().unwrap();
    let socket_path = temp_dir.path().join("launcher.sock");
    let launcher_body = Arc::new(Mutex::new(None));
    let launcher_task = spawn_launcher_capture(
        &socket_path,
        200,
        r#"{"status":"torn_down"}"#,
        Arc::clone(&launcher_body),
    )
    .await;

    let state = app_state_with_plugins(path_to_string(&socket_path));
    insert_transport(
        &state,
        "sess-1",
        sample_transport("tenant1", "plugin-a", "mcp_session_abc"),
    )
    .await;

    let mut stream = PerStreamState::default();
    let _ = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "DELETE"),
            (":path", "/tenant1/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    // upstream returns 5xx — teardown should still fire
    let _ = ExternalProcessorService::handle_response_headers(
        &state,
        &mut stream,
        headers(&[(":status", "500")]),
    )
    .await;
    launcher_task.await.unwrap();

    assert!(
        launcher_body.lock().await.is_some(),
        "launcher should have been called even on 5xx upstream"
    );
    assert!(state
        .transport_sessions
        .lock()
        .await
        .get("sess-1")
        .is_none());
}

#[allow(dead_code)]
fn _use_teardown_info_type(_: TeardownInfo) {}
