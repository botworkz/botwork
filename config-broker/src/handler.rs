//! HTTP handler for `POST /resolve`.
//!
//! Wire contract documented in `README.md`. In short:
//!
//! Request:
//!     `{ "tenant": "<tenant>", "namespace": "<ns>", "plugin": "<name>" }`
//!
//! Response 200:
//!     `{ "image", "port", "path", "upstream_auth",
//!        "resources": { "cpus"?, "memory"?, "pids"? },
//!        "env": [ { "name", "value" }, … ],
//!        "config_blob"?: "<compact JSON string>" }`
//!
//! Errors share a single envelope:
//!     `{ "error": "<machine code>", "message": "<human detail>" }`

use std::sync::Arc;
use std::sync::OnceLock;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::registry::{PluginEntry, PluginRegistry};

const PREFIX: &str = "[config-broker]";

/// Tenant name regex — matches session-broker's TENANT_RE.
/// Validated for shape only; v0 does not key resolution on tenant.
const TENANT_RE: &str = r"^[a-z][a-z0-9-]{0,30}$";

/// Namespace regex — matches session-broker's NAMESPACE_RE.
const NAMESPACE_RE: &str = r"^[a-z][a-z0-9-]{0,30}$";

/// Plugin name regex — matches the rule used at `plugins.yaml` parse time.
const PLUGIN_NAME_RE: &str = r"^[a-z][a-z0-9-]{0,30}$";

fn tenant_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(TENANT_RE).expect("valid tenant regex"))
}

fn namespace_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(NAMESPACE_RE).expect("valid namespace regex"))
}

fn plugin_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(PLUGIN_NAME_RE).expect("valid plugin name regex"))
}

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<PluginRegistry>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResolveRequest {
    tenant: Option<String>,
    namespace: Option<String>,
    plugin: Option<String>,
}

#[derive(Debug, Serialize)]
struct EnvEntry {
    name: String,
    value: String,
}

#[derive(Debug, Serialize, Default)]
struct ResourcesView {
    #[serde(skip_serializing_if = "Option::is_none")]
    cpus: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pids: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ResolveResponse {
    image: String,
    port: u16,
    path: String,
    upstream_auth: String,
    resources: ResourcesView,
    env: Vec<EnvEntry>,
    /// Already-serialised compact JSON. Omitted (rather than `""` or `{}`)
    /// when the operator did not set `config:` on this plugin.
    #[serde(skip_serializing_if = "Option::is_none")]
    config_blob: Option<String>,
    /// `egress:` block from `plugins.yaml`, validated by config-broker
    /// (one of: `{ "mode": "all" }`, `{ "mode": "none" }`, or
    /// `{ "allow": [ { "host", "ports": [...] }, ... ] }`) and shipped
    /// verbatim. session-broker forwards it straight through to
    /// control-plane (botwork #81) as the `egress_policy` of a
    /// `SessionRecord`. The xDS materialiser owns the schema; config-
    /// broker just refuses to ship anything that does not match one of
    /// the three forms. Always present (the field is required at the
    /// `plugins.yaml` level as of 0.1.9).
    egress: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
}

fn error_response(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    let body = ErrorBody {
        error: code,
        message: message.into(),
    };
    (status, Json(body)).into_response()
}

fn render_descriptor(entry: &PluginEntry) -> ResolveResponse {
    ResolveResponse {
        image: entry.image.clone(),
        port: entry.port,
        path: entry.path.clone(),
        upstream_auth: entry.upstream_auth.to_wire(),
        resources: ResourcesView {
            cpus: entry.resources.cpus.clone(),
            memory: entry.resources.memory.clone(),
            pids: entry.resources.pids,
        },
        env: entry
            .env
            .iter()
            .map(|(name, value)| EnvEntry {
                name: name.clone(),
                value: value.clone(),
            })
            .collect(),
        config_blob: entry.config.as_ref().map(|v| {
            serde_json::to_string(v)
                .expect("config Value re-serialises; validated at registry load")
        }),
        egress: entry.egress.clone(),
    }
}

pub(crate) async fn resolve(
    State(state): State<AppState>,
    body: Option<Json<ResolveRequest>>,
) -> Response {
    let Some(Json(payload)) = body else {
        warn!("{PREFIX} resolve: invalid_request — missing or unparseable JSON body");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "request body must be a JSON object",
        );
    };

    let Some(tenant) = payload.tenant.as_deref() else {
        warn!("{PREFIX} resolve: invalid_request — missing 'tenant'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing required field 'tenant'",
        );
    };
    let Some(namespace) = payload.namespace.as_deref() else {
        warn!("{PREFIX} resolve: invalid_request — missing 'namespace'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing required field 'namespace'",
        );
    };
    let Some(plugin) = payload.plugin.as_deref() else {
        warn!("{PREFIX} resolve: invalid_request — missing 'plugin'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing required field 'plugin'",
        );
    };

    if !tenant_re().is_match(tenant) {
        warn!("{PREFIX} resolve: invalid_request — bad tenant '{tenant}'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("invalid tenant '{tenant}': must match ^[a-z][a-z0-9-]{{0,30}}$"),
        );
    }
    if !namespace_re().is_match(namespace) {
        warn!("{PREFIX} resolve: invalid_namespace — bad namespace '{namespace}'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_namespace",
            format!("invalid namespace '{namespace}': must match ^[a-z][a-z0-9-]{{0,30}}$"),
        );
    }
    if !plugin_name_re().is_match(plugin) {
        warn!("{PREFIX} resolve: invalid_request — bad plugin name '{plugin}'");
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("invalid plugin '{plugin}': must match ^[a-z][a-z0-9-]{{0,30}}$"),
        );
    }

    match state.registry.get(plugin) {
        Some(entry) => {
            info!("{PREFIX} resolve: ok tenant={tenant} namespace={namespace} plugin={plugin}");
            (StatusCode::OK, Json(render_descriptor(entry))).into_response()
        }
        None => {
            warn!(
                "{PREFIX} resolve: unknown_plugin tenant={tenant} namespace={namespace} plugin={plugin}"
            );
            error_response(
                StatusCode::NOT_FOUND,
                "unknown_plugin",
                format!("unknown plugin: {plugin}"),
            )
        }
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/resolve", post(resolve))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{PluginEntry, PluginResources, UpstreamAuth};
    use std::collections::HashMap;

    fn entry_for_test() -> PluginEntry {
        // Tests pick `egress: { mode: "all" }` -- the wire shape of
        // `egress: all` from plugins.yaml -- because it is the smallest
        // value that satisfies the required-field schema without
        // implying any policy structure.
        PluginEntry {
            image: "botwork/mcp-test:local".to_string(),
            port: 8000,
            path: "/".to_string(),
            upstream_auth: UpstreamAuth::None,
            env: vec![("FOO".to_string(), "bar".to_string())],
            resources: PluginResources::default(),
            config: None,
            egress: serde_json::json!({ "mode": "all" }),
        }
    }

    #[test]
    fn render_descriptor_emits_wire_shape_with_no_config() {
        let descriptor = render_descriptor(&entry_for_test());
        assert_eq!(descriptor.image, "botwork/mcp-test:local");
        assert_eq!(descriptor.port, 8000);
        assert_eq!(descriptor.path, "/");
        assert_eq!(descriptor.upstream_auth, "none");
        assert!(descriptor.config_blob.is_none());
        assert_eq!(descriptor.env.len(), 1);
        assert_eq!(descriptor.env[0].name, "FOO");
        assert_eq!(descriptor.env[0].value, "bar");
    }

    #[test]
    fn render_descriptor_serialises_config_blob_compactly() {
        let mut entry = entry_for_test();
        entry.config = Some(serde_json::json!({"routes": [{"owner": "botworkz"}]}));
        let descriptor = render_descriptor(&entry);
        let blob = descriptor.config_blob.expect("blob set");
        assert_eq!(blob, r#"{"routes":[{"owner":"botworkz"}]}"#);
    }

    #[test]
    fn render_descriptor_emits_bearer_upstream_auth() {
        let mut entry = entry_for_test();
        entry.upstream_auth = UpstreamAuth::Bearer {
            service: "github.com".to_string(),
        };
        let descriptor = render_descriptor(&entry);
        assert_eq!(descriptor.upstream_auth, "bearer/github.com");
    }

    #[test]
    fn render_descriptor_omits_resources_when_default() {
        let descriptor = render_descriptor(&entry_for_test());
        let json = serde_json::to_value(&descriptor).expect("ser");
        // ResourcesView is always present, but its inner fields are omitted.
        let resources = json.get("resources").expect("resources present");
        assert!(
            resources.as_object().expect("object").is_empty(),
            "resources object should be empty when all fields are None: {resources}"
        );
    }

    #[test]
    fn render_descriptor_includes_partial_resources() {
        let mut entry = entry_for_test();
        entry.resources = PluginResources {
            cpus: None,
            memory: Some("4g".to_string()),
            pids: Some(1024),
        };
        let descriptor = render_descriptor(&entry);
        let json = serde_json::to_value(&descriptor).expect("ser");
        let resources = json.get("resources").expect("resources present");
        assert_eq!(resources["memory"], "4g");
        assert_eq!(resources["pids"], 1024);
        assert!(resources.get("cpus").is_none());
    }

    #[test]
    fn render_descriptor_emits_all_keyword_as_mode_object() {
        // `egress: all` in plugins.yaml is normalised by the registry to
        // `{ mode: "all" }` and round-trips verbatim through the wire.
        let descriptor = render_descriptor(&entry_for_test());
        let json = serde_json::to_value(&descriptor).expect("ser");
        assert_eq!(json["egress"], serde_json::json!({ "mode": "all" }));
    }

    #[test]
    fn render_descriptor_emits_none_keyword_as_mode_object() {
        // Same shape as `mode: all` but the wire value is `none`. The
        // materialiser (xDS feeder) is responsible for translating
        // `mode: none` into "no upstream clusters for this session".
        let mut entry = entry_for_test();
        entry.egress = serde_json::json!({ "mode": "none" });
        let descriptor = render_descriptor(&entry);
        let json = serde_json::to_value(&descriptor).expect("ser");
        assert_eq!(json["egress"], serde_json::json!({ "mode": "none" }));
    }

    #[test]
    fn render_descriptor_emits_full_allow_list_verbatim() {
        // config-broker doesn't interpret the schema -- shape is the
        // contract, content is the materialiser's problem. This test
        // pins that any validated mapping payload round-trips byte-
        // for-byte through the wire so downstream consumers always
        // see exactly what the operator typed.
        let mut entry = entry_for_test();
        entry.egress = serde_json::json!({
            "allow": [
                { "host": "api.github.com", "ports": [443] },
                { "host": "codeload.github.com", "ports": [443, 80] },
            ],
        });
        let descriptor = render_descriptor(&entry);
        let json = serde_json::to_value(&descriptor).expect("ser");
        assert_eq!(json["egress"]["allow"][0]["host"], "api.github.com");
        assert_eq!(json["egress"]["allow"][0]["ports"][0], 443);
        assert_eq!(json["egress"]["allow"][1]["ports"][1], 80);
    }

    fn state_with(plugin: &str) -> AppState {
        let mut registry = HashMap::new();
        registry.insert(plugin.to_string(), entry_for_test());
        AppState {
            registry: Arc::new(registry),
        }
    }

    async fn body_status(response: Response) -> (StatusCode, serde_json::Value) {
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, 64 * 1024)
            .await
            .expect("read body");
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (parts.status, value)
    }

    fn payload(tenant: &str, namespace: &str, plugin: &str) -> Json<ResolveRequest> {
        Json(ResolveRequest {
            tenant: Some(tenant.to_string()),
            namespace: Some(namespace.to_string()),
            plugin: Some(plugin.to_string()),
        })
    }

    #[tokio::test]
    async fn resolve_returns_200_for_known_plugin() {
        let state = state_with("test");
        let response = resolve(State(state), Some(payload("phlax", "mcp", "test"))).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["image"], "botwork/mcp-test:local");
        assert_eq!(body["upstream_auth"], "none");
    }

    #[tokio::test]
    async fn resolve_returns_404_for_unknown_plugin() {
        let state = state_with("test");
        let response = resolve(State(state), Some(payload("phlax", "mcp", "missing"))).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "unknown_plugin");
    }

    #[tokio::test]
    async fn resolve_returns_400_for_bad_namespace() {
        let state = state_with("test");
        let response = resolve(State(state), Some(payload("phlax", "BAD", "test"))).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_namespace");
    }

    #[tokio::test]
    async fn resolve_returns_400_for_bad_tenant() {
        let state = state_with("test");
        let response = resolve(State(state), Some(payload("BAD", "mcp", "test"))).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_request");
    }

    #[tokio::test]
    async fn resolve_returns_400_for_bad_plugin_name() {
        let state = state_with("test");
        let response = resolve(State(state), Some(payload("phlax", "mcp", "BAD"))).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_request");
    }

    #[tokio::test]
    async fn resolve_returns_400_when_body_missing() {
        let state = state_with("test");
        let response = resolve(State(state), None).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_request");
    }

    #[tokio::test]
    async fn resolve_returns_400_when_field_missing() {
        let state = state_with("test");
        let bad = Json(ResolveRequest {
            tenant: Some("phlax".to_string()),
            namespace: Some("mcp".to_string()),
            plugin: None,
        });
        let response = resolve(State(state), Some(bad)).await;
        let (status, body) = body_status(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_request");
        assert!(
            body["message"].as_str().unwrap_or("").contains("'plugin'"),
            "message should mention the missing field: {body}"
        );
    }
}
