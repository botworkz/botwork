use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;

use botwork_session_broker::ext_proc::{ExternalProcessorService, PerStreamState};
use botwork_session_broker::plugin_registry::PluginConfig;
use botwork_session_broker::session_registry::SessionRegistry;
use botwork_session_broker::{AppState, PendingInit, TransportState};
use envoy_proto::envoy::config::core::v3::{HeaderMap, HeaderValue};
use envoy_proto::envoy::service::ext_proc::v3::{
    processing_response, HttpBody, HttpHeaders, ProcessingResponse,
};
use tempfile::tempdir;
use tokio::sync::Mutex;

fn session_registry_path() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| "/tmp/botwork-session-broker-ext-proc-tests-sessions.json".to_string())
        .as_str()
}

fn app_state_with_plugins(launcher_socket_path: String) -> AppState {
    let mut plugin_registry = HashMap::new();
    plugin_registry.insert("plugin-a".to_string(), sample_plugin_config());
    AppState {
        plugin_registry,
        session_registry: Arc::new(SessionRegistry::new(session_registry_path())),
        transport_sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_init: Arc::new(Mutex::new(HashMap::new())),
        launcher_socket_path,
    }
}

fn app_state_with_empty_plugins(launcher_socket_path: String) -> AppState {
    AppState {
        plugin_registry: HashMap::new(),
        session_registry: Arc::new(SessionRegistry::new(session_registry_path())),
        transport_sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_init: Arc::new(Mutex::new(HashMap::new())),
        launcher_socket_path,
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

fn sample_transport(tenant: &str, plugin: &str, container: &str) -> TransportState {
    TransportState {
        container_name: container.to_string(),
        staging_token: "abcdef".to_string(),
        tenant_name: tenant.to_string(),
        plugin_name: plugin.to_string(),
        port: 8000,
        agent_id: None,
    }
}

fn sample_plugin_config() -> PluginConfig {
    PluginConfig {
        image: "botwork/plugin-a:local".to_string(),
        port: 8000,
        network: "botwork".to_string(),
    }
}

fn sample_pending(tenant: &str, plugin: &str, container: &str) -> PendingInit {
    PendingInit {
        container_name: container.to_string(),
        staging_token: "abcdef".to_string(),
        tenant_name: tenant.to_string(),
        plugin_name: plugin.to_string(),
        plugin_config: sample_plugin_config(),
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
async fn request_headers_get_without_session_returns_continue() {
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

    assert!(matches!(
        response.response,
        Some(processing_response::Response::RequestHeaders(_))
    ));
    assert_eq!(immediate_status(&response), None);
    assert_eq!(extract_upstream_mutation(&response), None);
}

#[tokio::test]
async fn request_headers_get_unknown_session_returns_continue() {
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

    assert!(matches!(
        response.response,
        Some(processing_response::Response::RequestHeaders(_))
    ));
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
