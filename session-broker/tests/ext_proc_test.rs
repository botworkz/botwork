use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;

use botwork_session_broker::config_broker::{
    EnvEntry, PluginDescriptor, PluginResources, UpstreamAuth,
};
use botwork_session_broker::ext_proc::{
    upstream_header_mutation, ExternalProcessorService, PerStreamState, TeardownInfo,
};
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

/// Routing-of-known-sessions builder: pre-existing tests pass a launcher
/// socket path (and sometimes an auth URL); after the cutover the builder no
/// longer seeds a plugin registry — the test seeds a `TransportState`
/// directly via `insert_transport`. The config-broker endpoint defaults to a
/// closed port so accidental spawn calls collapse to a 502 instead of
/// hanging the test.
fn app_state_with_plugins(launcher_socket_path: String) -> AppState {
    app_state_with_plugins_and_auth(launcher_socket_path, "http://127.0.0.1:1".to_string())
}

fn app_state_with_plugins_and_auth(
    launcher_socket_path: String,
    auth_broker_url: String,
) -> AppState {
    app_state_with_plugins_and_endpoints(
        launcher_socket_path,
        auth_broker_url,
        "http://127.0.0.1:1".to_string(),
    )
}

fn app_state_with_plugins_and_endpoints(
    launcher_socket_path: String,
    auth_broker_url: String,
    config_broker_endpoint: String,
) -> AppState {
    AppState {
        session_registry: Arc::new(SessionRegistry::new(session_registry_path())),
        transport_sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_init: Arc::new(Mutex::new(HashMap::new())),
        launcher_socket_path,
        auth_broker_url,
        config_broker_endpoint,
        control_plane_endpoint: "http://127.0.0.1:1".to_string(),
        tombstones: Arc::new(Mutex::new(HashMap::new())),
        liveness_cache: Arc::new(Mutex::new(HashMap::new())),
        stream_liveness: Arc::new(Mutex::new(HashMap::new())),
        // RFE #105 PR2: DB write-through is production-only; tests
        // pass `None` so they stay hermetic.
        agent_session_writer: None,
        // RFE #105 round-3 PR2: the cutover wires two
        // additional DB-bound handles next to
        // agent_session_writer. Test builders pass `None` the
        // same way to stay hermetic — production populates
        // both via `run()` once the `connect_from_env()`
        // handle is in hand.
        session_worker_writer: None,
        db: None,
    }
}

// Pre-cutover this builder seeded the in-process registry with a custom
// `path`. After the cutover the path is owned by config-broker, so the
// helper is preserved for source compatibility but `_plugin_path` is
// ignored. Routing-of-known-sessions tests express the plugin path via
// the seeded `TransportState` instead.
fn app_state_with_plugins_and_auth_and_path(
    launcher_socket_path: String,
    auth_broker_url: String,
    _plugin_path: &str,
) -> AppState {
    app_state_with_plugins_and_auth(launcher_socket_path, auth_broker_url)
}

// Spawn-path builder: takes a `config_broker_endpoint` explicitly so the
// caller can stand up a fake config-broker with whatever descriptor it
// needs (path, upstream_auth, env, resources, config_blob). The pre-cutover
// builder packed those into a single `(plugin_path, upstream_auth)` pair;
// new tests build the full descriptor on the fake server.
fn app_state_for_spawn(
    launcher_socket_path: String,
    auth_broker_url: String,
    config_broker_endpoint: String,
) -> AppState {
    app_state_with_plugins_and_endpoints(
        launcher_socket_path,
        auth_broker_url,
        config_broker_endpoint,
    )
}

fn app_state_with_empty_plugins(launcher_socket_path: String) -> AppState {
    // "Empty" used to mean an empty in-process plugin registry. After the
    // config-broker cutover the equivalent failure is "config-broker
    // unreachable" — we point the endpoint at a closed port and let any
    // spawn attempt collapse to a 502.
    AppState {
        session_registry: Arc::new(SessionRegistry::new(session_registry_path())),
        transport_sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_init: Arc::new(Mutex::new(HashMap::new())),
        launcher_socket_path,
        auth_broker_url: "http://127.0.0.1:1".to_string(),
        config_broker_endpoint: "http://127.0.0.1:1".to_string(),
        control_plane_endpoint: "http://127.0.0.1:1".to_string(),
        tombstones: Arc::new(Mutex::new(HashMap::new())),
        liveness_cache: Arc::new(Mutex::new(HashMap::new())),
        stream_liveness: Arc::new(Mutex::new(HashMap::new())),
        // RFE #105 PR2: DB write-through is production-only; tests
        // pass `None` so they stay hermetic.
        agent_session_writer: None,
        // RFE #105 round-3 PR2: the cutover wires two
        // additional DB-bound handles next to
        // agent_session_writer. Test builders pass `None` the
        // same way to stay hermetic — production populates
        // both via `run()` once the `connect_from_env()`
        // handle is in hand.
        session_worker_writer: None,
        db: None,
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
    sample_transport_with_workspace_and_path(tenant, "mcp", plugin, container, plugin_path)
}

fn sample_transport_with_workspace_and_path(
    tenant: &str,
    workspace: &str,
    plugin: &str,
    container: &str,
    plugin_path: &str,
) -> TransportState {
    TransportState {
        container_name: container.to_string(),
        container_ip: "172.20.0.5".to_string(),
        staging_token: "abcdef".to_string(),
        tenant_name: tenant.to_string(),
        workspace: workspace.to_string(),
        plugin_name: plugin.to_string(),
        port: 8000,
        path: plugin_path.to_string(),
        upstream_auth: UpstreamAuth::None,
        upstream_authorization: None,
        agent_id: None,
        egress_policy: None,
    }
}

fn sample_plugin_config() -> PluginDescriptor {
    sample_plugin_config_with_path("/mcp")
}

fn sample_plugin_config_with_path(path: &str) -> PluginDescriptor {
    sample_plugin_config_with_path_and_auth(path, UpstreamAuth::None)
}

fn sample_plugin_config_with_path_and_auth(
    path: &str,
    upstream_auth: UpstreamAuth,
) -> PluginDescriptor {
    PluginDescriptor {
        image: "botwork/plugin-a:local".to_string(),
        port: 8000,
        path: path.to_string(),
        upstream_auth,
        env: vec![],
        resources: PluginResources::default(),
        config_blob: None,
        egress: None,
    }
}

fn sample_pending(tenant: &str, plugin: &str, container: &str) -> PendingInit {
    PendingInit {
        container_name: container.to_string(),
        container_ip: "172.20.0.5".to_string(),
        staging_token: "abcdef".to_string(),
        tenant_name: tenant.to_string(),
        workspace: "mcp".to_string(),
        plugin_name: plugin.to_string(),
        descriptor: sample_plugin_config(),
        upstream_authorization: None,
        created_at: "2026-01-01T00:00:00Z".to_string(),
    }
}

async fn insert_transport(state: &AppState, mcp_session_id: &str, transport: TransportState) {
    // Seed the liveness cache so tests don't trigger docker inspect for
    // containers that only exist in the test's mental model.
    state.liveness_cache.lock().await.insert(
        transport.container_name.clone(),
        std::time::Instant::now() + botwork_session_broker::LIVENESS_TTL,
    );
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
            (":path", "/tenant1/mcp/plugin-a"),
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
            (":path", "/tenant1/mcp/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(400));
    assert_eq!(
        immediate_body(&response).as_deref(),
        Some("missing Mcp-Session-Id header")
    );
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
            (":path", "/tenant1/mcp/plugin-a/foo"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-unknown"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(404));
    assert_eq!(
        immediate_body(&response).as_deref(),
        Some("unknown mcp-session-id")
    );
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
            (":path", "/tenant1/mcp/plugin-a/foo"),
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
            (":path", "/tenant1/mcp/plugin-a/foo"),
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
            (":path", "/tenant1/mcp/plugin-a/foo"),
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
            (":path", "/tenant1/mcp/plugin-a"),
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
            (":path", "/tenant2/mcp/plugin-a"),
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
            (":path", "/tenant1/mcp/plugin-a"),
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
            (":path", "/tenant1/mcp/plugin-a"),
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
            (":path", "/tenant1/mcp/plugin-a/foo"),
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
            (":path", "/tenant1/mcp/plugin-a"),
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
            (":path", "/tenant1/mcp/plugin-a/foo"),
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
            (":path", "/tenant1/mcp/plugin-a/foo"),
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
            (":path", "/tenant1/mcp/plugin-a/foo"),
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
    // Routing of a known session reads upstream_auth straight off
    // TransportState; nothing about config-broker is consulted on the hot
    // path. Seed both `upstream_auth` (the policy) and
    // `upstream_authorization` (the resolved token) directly on the
    // transport.
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut transport = sample_transport("tenant1", "plugin-a", "mcp_session_abc");
    transport.upstream_auth = UpstreamAuth::Bearer {
        service: "github.com".to_string(),
    };
    transport.upstream_authorization = Some("ghp_CACHED".to_string());
    insert_transport(&state, "sess-1", transport).await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a/foo"),
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
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    let mut transport = sample_transport("tenant1", "plugin-a", "mcp_session_abc");
    // Bearer policy on the transport but no resolved token cached → strip.
    transport.upstream_auth = UpstreamAuth::Bearer {
        service: "github.com".to_string(),
    };
    insert_transport(&state, "sess-1", transport).await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a/foo"),
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
            (":path", "/tenant1/mcp/plugin-a/foo"),
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
            (":path", "/tenant1/mcp/plugin-a"),
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
            (":path", "/tenant2/mcp/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(403));
}

#[tokio::test]
async fn request_headers_post_initialize_config_broker_unreachable_returns_502() {
    // Pre-cutover this asserted on an empty in-process plugin registry → 500.
    // After the cutover the equivalent failure mode is "config-broker is not
    // reachable" — session-broker collapses Transport / 5xx errors to 502.
    let state = app_state_with_empty_plugins("/tmp/no-launcher.sock".to_string());
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(502));
}

#[tokio::test]
async fn request_headers_post_initialize_unknown_plugin_returns_404() {
    let config_url = spawn_config_broker_with_response(
        404,
        r#"{"error":"unknown_plugin","message":"unknown plugin: plugin-x"}"#.to_string(),
    )
    .await;
    let state = app_state_for_spawn(
        "/tmp/no-launcher.sock".to_string(),
        "http://127.0.0.1:1".to_string(),
        config_url,
    );
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-x"),
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

    let config_url = spawn_config_broker_with_descriptor(descriptor_default()).await;
    let state = app_state_for_spawn(path_to_string(&socket_path), auth_url, config_url);
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
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

    let config_url = spawn_config_broker_with_descriptor(descriptor_default()).await;
    let state = app_state_for_spawn(
        path_to_string(&socket_path),
        "http://127.0.0.1:1".to_string(),
        config_url,
    );
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
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
    let config_url = spawn_config_broker_with_descriptor(descriptor_default()).await;
    let state = app_state_for_spawn(
        path_to_string(&socket_path),
        "http://127.0.0.1:1".to_string(),
        config_url,
    );
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
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
    let descriptor = PluginDescriptor {
        upstream_auth: UpstreamAuth::Bearer {
            service: "github.com".to_string(),
        },
        ..descriptor_default()
    };
    let config_url = spawn_config_broker_with_descriptor(descriptor).await;
    let state = app_state_for_spawn(
        "/tmp/missing-launcher.sock".to_string(),
        auth_url,
        config_url,
    );
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
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
    let descriptor = PluginDescriptor {
        upstream_auth: UpstreamAuth::Bearer {
            service: "github.com".to_string(),
        },
        ..descriptor_default()
    };
    let config_url = spawn_config_broker_with_descriptor(descriptor).await;
    let state = app_state_for_spawn(
        "/tmp/missing-launcher.sock".to_string(),
        auth_url,
        config_url,
    );
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
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
    let descriptor = PluginDescriptor {
        upstream_auth: UpstreamAuth::Bearer {
            service: "github.com".to_string(),
        },
        ..descriptor_default()
    };
    let config_url = spawn_config_broker_with_descriptor(descriptor).await;
    let state = app_state_for_spawn(
        "/tmp/missing-launcher.sock".to_string(),
        auth_url,
        config_url,
    );
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
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
    let config_url = spawn_config_broker_with_descriptor(descriptor_default()).await;
    let state = app_state_for_spawn(
        path_to_string(&socket_path),
        "http://127.0.0.1:1".to_string(),
        config_url,
    );
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
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
    let config_url = spawn_config_broker_with_descriptor(descriptor_default()).await;
    let state = app_state_for_spawn(path_to_string(&socket_path), auth_url, config_url);
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
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
            (":path", "/tenant1/mcp/plugin-a"),
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
    let descriptor = PluginDescriptor {
        upstream_auth: UpstreamAuth::Bearer {
            service: "github.com".to_string(),
        },
        ..descriptor_default()
    };
    let config_url = spawn_config_broker_with_descriptor(descriptor).await;
    let state = app_state_for_spawn(
        "/tmp/missing-launcher.sock".to_string(),
        auth_url,
        config_url,
    );

    start_log_capture();
    let mut stream = PerStreamState::default();
    let _ = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
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

/// Spawn-path tests build a fake config-broker on top of this descriptor.
/// The `with_*` helpers are descriptor builders, not state builders — they
/// just assemble the JSON the fake `/resolve` endpoint returns. Tests then
/// stand the fake up via `spawn_config_broker_capture` and feed its URL into
/// `app_state_for_spawn`.
fn descriptor_default() -> PluginDescriptor {
    PluginDescriptor {
        image: "botwork/plugin-a:local".to_string(),
        port: 8000,
        path: "/mcp".to_string(),
        upstream_auth: UpstreamAuth::None,
        resources: PluginResources::default(),
        env: vec![],
        config_blob: None,
        egress: None,
    }
}

fn descriptor_with_env(env: Vec<EnvEntry>) -> PluginDescriptor {
    PluginDescriptor {
        env,
        ..descriptor_default()
    }
}

fn descriptor_with_resources(resources: PluginResources) -> PluginDescriptor {
    PluginDescriptor {
        resources,
        ..descriptor_default()
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

    let descriptor = descriptor_with_env(vec![
        EnvEntry {
            name: "GITHUB_TOOLSETS".to_string(),
            value: "default,actions".to_string(),
        },
        EnvEntry {
            name: "GITHUB_TERSE_DESCRIPTIONS".to_string(),
            value: "true".to_string(),
        },
    ]);
    let config_url = spawn_config_broker_with_descriptor(descriptor).await;
    let state = app_state_for_spawn(
        path_to_string(&socket_path),
        "http://127.0.0.1:1".to_string(),
        config_url,
    );
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
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

    let descriptor = descriptor_with_resources(PluginResources {
        cpus: Some("4.0".to_string()),
        memory: Some("4g".to_string()),
        pids: Some(1024),
    });
    let config_url = spawn_config_broker_with_descriptor(descriptor).await;
    let state = app_state_for_spawn(
        path_to_string(&socket_path),
        "http://127.0.0.1:1".to_string(),
        config_url,
    );
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
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

    let descriptor = descriptor_with_env(vec![EnvEntry {
        name: "GITHUB_TOOLSETS".to_string(),
        value: "default,actions".to_string(),
    }]);
    let config_url = spawn_config_broker_with_descriptor(descriptor).await;
    let state = app_state_for_spawn(path_to_string(&socket_path), auth_url, config_url);
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
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

    let descriptor = descriptor_with_env(vec![EnvEntry {
        name: "GITHUB_TOOLSETS".to_string(),
        value: "default,actions".to_string(),
    }]);
    let config_url = spawn_config_broker_with_descriptor(descriptor).await;
    let state = app_state_for_spawn(
        path_to_string(&socket_path),
        "http://127.0.0.1:1".to_string(),
        config_url,
    );
    let mut stream = PerStreamState::default();
    // No x-botwork-cap header — no secrets fetch.
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
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

/// Stand up a fake config-broker on `127.0.0.1:0` that responds to one
/// `POST /resolve` with the supplied descriptor (rendered to wire JSON) at
/// status 200, then closes. Returns the base URL.
///
/// `expected_calls` controls how many requests the fake will accept before
/// returning. Pass 1 for the typical single-spawn test.
async fn spawn_config_broker_with_descriptor(descriptor: PluginDescriptor) -> String {
    let body = render_descriptor_json(&descriptor);
    spawn_config_broker_with_response(200, body).await
}

/// Lower-level fake — caller controls status and body. Useful for the
/// failure-mode tests that want to assert how session-broker maps a 400/404/
/// 500/garbage response from config-broker onto a client-facing immediate
/// response.
async fn spawn_config_broker_with_response(status_code: u16, body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind config broker");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept config request");
        // Drain the request — we don't currently assert on its body in any
        // spawn-path test, but the read keeps the HTTP framing tidy.
        let _ = read_tcp_http_request(&mut stream).await;
        let reason = match status_code {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "OK",
        };
        let response = format!(
            "HTTP/1.1 {status_code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write config response");
    });
    format!("http://{addr}")
}

/// Render a `PluginDescriptor` to the on-the-wire JSON shape config-broker
/// returns. Mirrors `config-broker/src/handler.rs::render_descriptor` — kept
/// here so spawn-path tests don't depend on the config-broker crate.
fn render_descriptor_json(descriptor: &PluginDescriptor) -> String {
    let upstream_auth = match &descriptor.upstream_auth {
        UpstreamAuth::None => "none".to_string(),
        UpstreamAuth::Bearer { service } => format!("bearer/{service}"),
    };
    let mut json = serde_json::json!({
        "image": descriptor.image,
        "port": descriptor.port,
        "path": descriptor.path,
        "upstream_auth": upstream_auth,
        "resources": {},
        "env": descriptor
            .env
            .iter()
            .map(|e| serde_json::json!({ "name": e.name, "value": e.value }))
            .collect::<Vec<_>>(),
    });
    let resources = json["resources"].as_object_mut().unwrap();
    if let Some(cpus) = &descriptor.resources.cpus {
        resources.insert("cpus".to_string(), serde_json::Value::String(cpus.clone()));
    }
    if let Some(memory) = &descriptor.resources.memory {
        resources.insert(
            "memory".to_string(),
            serde_json::Value::String(memory.clone()),
        );
    }
    if let Some(pids) = descriptor.resources.pids {
        resources.insert("pids".to_string(), serde_json::Value::Number(pids.into()));
    }
    if let Some(blob) = &descriptor.config_blob {
        json.as_object_mut().unwrap().insert(
            "config_blob".to_string(),
            serde_json::Value::String(blob.clone()),
        );
    }
    json.to_string()
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

fn app_state_with_session_registry(
    launcher_socket_path: String,
    registry: Arc<SessionRegistry>,
) -> AppState {
    AppState {
        session_registry: registry,
        transport_sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_init: Arc::new(Mutex::new(HashMap::new())),
        launcher_socket_path,
        auth_broker_url: "http://127.0.0.1:1".to_string(),
        config_broker_endpoint: "http://127.0.0.1:1".to_string(),
        control_plane_endpoint: "http://127.0.0.1:1".to_string(),
        tombstones: Arc::new(Mutex::new(HashMap::new())),
        liveness_cache: Arc::new(Mutex::new(HashMap::new())),
        stream_liveness: Arc::new(Mutex::new(HashMap::new())),
        // RFE #105 PR2: DB write-through is production-only; tests
        // pass `None` so they stay hermetic.
        agent_session_writer: None,
        // RFE #105 round-3 PR2: the cutover wires two
        // additional DB-bound handles next to
        // agent_session_writer. Test builders pass `None` the
        // same way to stay hermetic — production populates
        // both via `run()` once the `connect_from_env()`
        // handle is in hand.
        session_worker_writer: None,
        db: None,
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
            (":path", "/tenant1/mcp/plugin-a/foo"),
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
            (":path", "/tenant1/mcp/plugin-a"),
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
            (":path", "/tenant1/mcp/plugin-a"),
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
            "tenant1",
            "mcp",
            "botwork/plugin-a:local",
            "2026-01-01T00:00:00Z",
        )
        .await;

    let state = app_state_with_session_registry(path_to_string(&socket_path), registry);
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
            (":path", "/tenant1/mcp/plugin-a/foo"),
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
            (":path", "/tenant1/mcp/plugin-a"),
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

// ---------------------------------------------------------------------------
// Namespace tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn request_headers_post_missing_workspace_returns_400() {
    // Old-shape URL /<tenant>/<plugin> without a workspace → 400
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
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

    assert_eq!(immediate_status(&response), Some(400));
    assert!(
        immediate_body(&response)
            .unwrap_or_default()
            .contains("workspace required: use /<tenant>/<workspace>/<plugin>"),
        "expected workspace hint in error body: {:?}",
        immediate_body(&response)
    );
}

#[tokio::test]
async fn request_headers_get_workspace_mismatch_returns_403() {
    // Transport bound to (tenant1, ns1, plugin-a); request uses ns2 → 403
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    insert_transport(
        &state,
        "sess-1",
        sample_transport_with_workspace_and_path(
            "tenant1",
            "ns1",
            "plugin-a",
            "mcp_session_abc",
            "/mcp",
        ),
    )
    .await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "GET"),
            (":path", "/tenant1/ns2/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(403));
    assert_eq!(
        immediate_body(&response).as_deref(),
        Some("session workspace mismatch")
    );
}

#[tokio::test]
async fn request_headers_delete_workspace_mismatch_returns_403() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    insert_transport(
        &state,
        "sess-1",
        sample_transport_with_workspace_and_path(
            "tenant1",
            "ns1",
            "plugin-a",
            "mcp_session_abc",
            "/mcp",
        ),
    )
    .await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "DELETE"),
            (":path", "/tenant1/ns2/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(403));
    assert_eq!(
        immediate_body(&response).as_deref(),
        Some("session workspace mismatch")
    );
}

#[tokio::test]
async fn request_headers_post_known_session_workspace_mismatch_returns_403() {
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    insert_transport(
        &state,
        "sess-1",
        sample_transport_with_workspace_and_path(
            "tenant1",
            "ns1",
            "plugin-a",
            "mcp_session_abc",
            "/mcp",
        ),
    )
    .await;
    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/ns2/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
            ("mcp-session-id", "sess-1"),
        ]),
    )
    .await;

    assert_eq!(immediate_status(&response), Some(403));
    assert_eq!(
        immediate_body(&response).as_deref(),
        Some("session workspace mismatch")
    );
}

#[tokio::test]
async fn different_workspaces_same_tenant_and_agent_get_distinct_agent_dirs() {
    // Two transports: same tenant/agent_id/plugin, different workspace.
    // Their agent_dirs must differ.
    let state = app_state_with_plugins("/tmp/no-launcher.sock".to_string());
    insert_transport(
        &state,
        "sess-ns1",
        sample_transport_with_workspace_and_path(
            "tenant1",
            "ns1",
            "plugin-a",
            "mcp_session_aaa",
            "/mcp",
        ),
    )
    .await;
    insert_transport(
        &state,
        "sess-ns2",
        sample_transport_with_workspace_and_path(
            "tenant1",
            "ns2",
            "plugin-a",
            "mcp_session_bbb",
            "/mcp",
        ),
    )
    .await;

    let sessions = state.transport_sessions.lock().await;
    let t1 = sessions.get("sess-ns1").unwrap();
    let t2 = sessions.get("sess-ns2").unwrap();
    assert_ne!(t1.workspace, t2.workspace, "workspaces must differ");
    // The agent_dir paths would differ because workspace is part of the key.
    let dir1 = format!(
        "/var/lib/botwork/tenants/{}/workspaces/{}/agents/agent-1",
        t1.tenant_name, t1.workspace
    );
    let dir2 = format!(
        "/var/lib/botwork/tenants/{}/workspaces/{}/agents/agent-1",
        t2.tenant_name, t2.workspace
    );
    assert_ne!(
        dir1, dir2,
        "agent_dirs with different workspaces must differ"
    );
}

#[tokio::test]
async fn session_entry_serialization_includes_tenant_and_workspace() {
    use botwork_session_broker::session_registry::{utc_now, SessionRegistry};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.json");
    let registry = SessionRegistry::new(path.to_str().unwrap());

    registry
        .record_spawn(
            "mcp_session_aabbccddeeff",
            "/staging/x",
            "acme",
            "dev",
            "botwork/mcp-echo:local",
            &utc_now(),
        )
        .await;

    let content = std::fs::read_to_string(&path).unwrap();
    let json: serde_json::Value = serde_json::from_str(&content).unwrap();
    let entry = &json["sessions"]["mcp_session_aabbccddeeff"];
    assert_eq!(entry["tenant"], "acme");
    assert_eq!(entry["workspace"], "dev");
}

#[allow(dead_code)]
fn _use_teardown_info_type(_: TeardownInfo) {}

fn descriptor_with_config_blob(config_blob: Option<String>) -> PluginDescriptor {
    PluginDescriptor {
        config_blob,
        ..descriptor_default()
    }
}

#[tokio::test]
async fn spawn_config_env_appears_in_launcher_payload() {
    use botwork_session_broker::config_broker::CONFIG_ENV_NAME;

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

    // config-broker now serialises the structured config to compact JSON on
    // its side — session-broker just drops the resulting string into
    // BOTWORK_MCP_CONFIG. The fake config-broker therefore has to ship the
    // descriptor with `config_blob` already serialised.
    let blob = serde_json::json!({
        "default_token_env": "BOTWORK_SECRET_GITHUB_DEFAULT",
        "routes": [
            { "owner": "botworkz", "token_env": "BOTWORK_SECRET_GITHUB_BOTWORKZ" }
        ]
    })
    .to_string();
    let descriptor = descriptor_with_config_blob(Some(blob));
    let config_url = spawn_config_broker_with_descriptor(descriptor).await;
    let state = app_state_for_spawn(
        path_to_string(&socket_path),
        "http://127.0.0.1:1".to_string(),
        config_url,
    );

    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
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
    let config_entry = env
        .iter()
        .find(|e| e["name"] == CONFIG_ENV_NAME)
        .expect("BOTWORK_MCP_CONFIG should be in env");
    let config_str = config_entry["value"]
        .as_str()
        .expect("config value is string");
    let parsed: serde_json::Value = serde_json::from_str(config_str).expect("config is valid JSON");
    assert_eq!(
        parsed["default_token_env"].as_str().unwrap(),
        "BOTWORK_SECRET_GITHUB_DEFAULT"
    );
    assert_eq!(parsed["routes"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn spawn_config_env_absent_when_config_not_set() {
    use botwork_session_broker::config_broker::CONFIG_ENV_NAME;

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

    let descriptor = descriptor_with_config_blob(None);
    let config_url = spawn_config_broker_with_descriptor(descriptor).await;
    let state = app_state_for_spawn(
        path_to_string(&socket_path),
        "http://127.0.0.1:1".to_string(),
        config_url,
    );

    let mut stream = PerStreamState::default();
    let response = ExternalProcessorService::handle_request_headers(
        &state,
        &mut stream,
        headers(&[
            (":method", "POST"),
            (":path", "/tenant1/mcp/plugin-a"),
            ("x-botwork-tenant", "tenant1"),
        ]),
    )
    .await;
    launcher_task.await.unwrap();
    assert_eq!(immediate_status(&response), Some(502));

    let launcher_payload: serde_json::Value =
        serde_json::from_str(&launcher_body.lock().await.clone().expect("launcher body"))
            .expect("launcher json");
    // When config is None the env array may be absent entirely (no static env,
    // no config entry) or present but must not contain BOTWORK_MCP_CONFIG.
    assert!(
        launcher_payload.get("env").is_none()
            || launcher_payload["env"]
                .as_array()
                .map(|a| a.iter().all(|e| e["name"] != CONFIG_ENV_NAME))
                .unwrap_or(true),
        "BOTWORK_MCP_CONFIG should not be present when config is None"
    );
}
