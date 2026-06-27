// SPDX-License-Identifier: Apache-2.0

//! Thin typed HTTP client over `fetch`, talking to api on the
//! same origin.
//!
//! # Wire envelopes
//!
//! api uses two stable wire shapes that every consumer needs to
//! handle:
//!
//! 1. **List responses** — `{ "items": [...], "total": N }`. Wrapping
//!    is deliberate so pagination can land as a pure-additive change
//!    (`?limit=&offset=&next_cursor=`). Decoded into [`ListResponse`].
//!
//! 2. **Error responses** — `{ "error": "<code>", "message": "<text>",
//!    "dependents"?: [...] }`. The `error` field is machine-readable
//!    (`not_found` / `bad_request` / `validation_failed` /
//!    `has_dependents` / `stale_write` / `already_exists` /
//!    `unavailable` / `internal`), the `message` is operator-facing
//!    text, and `dependents` is present only on the 409
//!    `has_dependents` shape. Decoded into [`ApiError`].
//!
//! # Optimistic locking
//!
//! Mutating endpoints (PUT) require an `if_unmodified_since` field in
//! the request body, holding the `updated_at` value the client last
//! read for the row. api compares the submitted value with the
//! current row at write time and returns `409 stale_write` if they
//! differ. UI flow:
//!
//! 1. GET the row, remember `updated_at`,
//! 2. user edits, hits Save,
//! 3. PUT with `if_unmodified_since = remembered_updated_at`,
//! 4. on `Stale` re-fetch and show a "this changed under you" prompt.
//!
//! The client surfaces `Stale` as a distinct variant of [`ApiError`]
//! so handlers don't have to string-match on the `error` field.
//!
//! # Operator identity
//!
//! Every mutation sends `x-botwork-admin: <operator>`. api logs
//! it on the audit event but does not validate it (the docker
//! network is the trust boundary in v0). The UI sets it to a
//! placeholder ("ui") until ext_authz starts asserting an
//! identity at the ingress envoy.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Headers, Request, RequestInit, Response};

/// Operator identity sent on every mutation. Placeholder for v0; will
/// be replaced by the asserted identity from envoy ext_authz once
/// auth lands.
pub const OPERATOR: &str = "ui";

/// Base path the ui talks to. Same-origin by construction:
/// the ui bundle is served from `/admin/` and the ingress
/// envoy routes `/api/*` to api (Phase 2 reshape — botworkz/space#311).
pub const API_BASE: &str = "/api";

/// Wire-shape for every list endpoint. `items` is serialised verbatim
/// from the entity model; `total` is the row count after filtering
/// (NOT the unfiltered table size).
#[derive(Debug, Clone, Deserialize)]
pub struct ListResponse<T> {
    pub items: Vec<T>,
    pub total: usize,
}

/// Structured representation of api's error envelope.
///
/// `Stale`, `HasDependents`, and `ValidationFailed` are pulled out as
/// their own variants because the UI renders them specially.
/// Everything else falls into `Other`.
#[derive(Debug, Clone)]
pub enum ApiError {
    /// `404 not_found`.
    NotFound { message: String },
    /// `400 bad_request`.
    BadRequest { message: String },
    /// `422 validation_failed`.
    ValidationFailed { message: String },
    /// `409 stale_write` — caller's `if_unmodified_since` token is
    /// stale. Re-fetch the row and retry.
    Stale { message: String },
    /// `409 already_exists` — typically a name uniqueness violation.
    AlreadyExists { message: String },
    /// `409 has_dependents` — delete blocked by FK references.
    /// `dependents` is the verbatim JSON payload (api's
    /// shape is `[{kind, id, name}]` but we keep it loose for
    /// forward-compat).
    HasDependents {
        message: String,
        dependents: JsonValue,
    },
    /// `503 unavailable` — coordination layer is down (typically
    /// control-plane unreachable for a binding mutation). Retryable.
    Unavailable { message: String },
    /// Any other api error, or a transport-layer failure.
    Other { status: u16, message: String },
}

impl ApiError {
    /// Operator-facing summary string. Useful for the catch-all error
    /// renderer; specific error variants should be matched on by the
    /// caller for richer rendering.
    pub fn message(&self) -> &str {
        match self {
            ApiError::NotFound { message }
            | ApiError::BadRequest { message }
            | ApiError::ValidationFailed { message }
            | ApiError::Stale { message }
            | ApiError::AlreadyExists { message }
            | ApiError::HasDependents { message, .. }
            | ApiError::Unavailable { message }
            | ApiError::Other { message, .. } => message,
        }
    }
}

/// Wire shape of api's error envelope. Used internally to
/// decode 4xx/5xx responses.
#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: String,
    message: String,
    #[serde(default)]
    dependents: Option<JsonValue>,
}

impl ErrorEnvelope {
    fn into_api_error(self, status: u16) -> ApiError {
        match self.error.as_str() {
            "not_found" => ApiError::NotFound {
                message: self.message,
            },
            "bad_request" => ApiError::BadRequest {
                message: self.message,
            },
            "validation_failed" => ApiError::ValidationFailed {
                message: self.message,
            },
            "stale_write" => ApiError::Stale {
                message: self.message,
            },
            "already_exists" => ApiError::AlreadyExists {
                message: self.message,
            },
            "has_dependents" => ApiError::HasDependents {
                message: self.message,
                dependents: self.dependents.unwrap_or(JsonValue::Null),
            },
            "unavailable" => ApiError::Unavailable {
                message: self.message,
            },
            _ => ApiError::Other {
                status,
                message: self.message,
            },
        }
    }
}

// ── tenant ──────────────────────────────────────────────────────────

/// Tenant model — mirror of `botwork_entity::tenant::Model`.
///
/// Hand-rolled (no shared types crate yet) because the wasm crate
/// can't depend on `botwork-entity` (sea-orm doesn't compile to
/// `wasm32-unknown-unknown`). When the entity surface stabilises
/// we'll consider a shared `botwork-admin-types` crate; until then
/// the wire contract is the source of truth and this struct mirrors
/// it.
#[derive(Debug, Clone, Deserialize)]
pub struct Tenant {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
}

/// POST body for tenant create.
#[derive(Debug, Serialize)]
pub struct TenantCreate {
    pub name: String,
}

/// PUT body for tenant update. Carries the optimistic-lock token in
/// `if_unmodified_since` per the wire contract.
#[derive(Debug, Serialize)]
pub struct TenantUpdate {
    pub name: String,
    pub if_unmodified_since: String,
}

/// Issue `GET ${API_BASE}/tenants` and decode the list envelope.
pub async fn list_tenants() -> Result<ListResponse<Tenant>, ApiError> {
    get_json(&format!("{API_BASE}/tenants")).await
}

/// Issue `GET ${API_BASE}/tenants/{id}` and decode the row. Returns
/// [`ApiError::NotFound`] for unknown IDs and [`ApiError::BadRequest`]
/// for malformed UUIDs.
pub async fn get_tenant(id: &str) -> Result<Tenant, ApiError> {
    get_json(&format!("{API_BASE}/tenants/{id}")).await
}

/// Issue `POST ${API_BASE}/tenants` with `{ "name": ... }`. Surfaces
/// `409 already_exists` on duplicate name and `422 validation_failed`
/// on invalid name shape.
pub async fn create_tenant(body: &TenantCreate) -> Result<Tenant, ApiError> {
    send_json("POST", &format!("{API_BASE}/tenants"), Some(body)).await
}

/// Issue `PUT ${API_BASE}/tenants/{id}` with the new name + lock
/// token. Surfaces `409 stale_write` when the token doesn't match.
pub async fn update_tenant(id: &str, body: &TenantUpdate) -> Result<Tenant, ApiError> {
    send_json("PUT", &format!("{API_BASE}/tenants/{id}"), Some(body)).await
}

/// Issue `DELETE ${API_BASE}/tenants/{id}`. Surfaces
/// `409 has_dependents` (with the dependent list) when the tenant
/// owns workspaces.
pub async fn delete_tenant(id: &str) -> Result<(), ApiError> {
    send_no_content(&format!("{API_BASE}/tenants/{id}")).await
}

// ── workspace ──────────────────────────────────────────────────────

/// Workspace model — mirror of `botwork_entity::workspace::Model`.
#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceCreate {
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceUpdate {
    pub name: String,
    pub if_unmodified_since: String,
}

/// List workspaces, optionally filtered by parent tenant.
pub async fn list_workspaces(tenant: &str) -> Result<ListResponse<Workspace>, ApiError> {
    let url = format!("{API_BASE}/tenant/{tenant}/workspaces");
    get_json(&url).await
}

pub async fn get_workspace(tenant: &str, id: &str) -> Result<Workspace, ApiError> {
    get_json(&format!("{API_BASE}/tenant/{tenant}/workspaces/{id}")).await
}

pub async fn create_workspace(tenant: &str, body: &WorkspaceCreate) -> Result<Workspace, ApiError> {
    send_json(
        "POST",
        &format!("{API_BASE}/tenant/{tenant}/workspaces"),
        Some(body),
    )
    .await
}

pub async fn update_workspace(
    tenant: &str,
    id: &str,
    body: &WorkspaceUpdate,
) -> Result<Workspace, ApiError> {
    send_json(
        "PUT",
        &format!("{API_BASE}/tenant/{tenant}/workspaces/{id}"),
        Some(body),
    )
    .await
}

/// Workspace delete CASCADEs to bindings + agent_sessions; if the
/// live-state gate against control-plane is reachable, also
/// terminates any live sessions before committing. Failure modes:
/// `503 unavailable` (control-plane unreachable mid-cascade) — the
/// DB is rolled back, UI should retry.
pub async fn delete_workspace(tenant: &str, id: &str) -> Result<(), ApiError> {
    send_no_content(&format!("{API_BASE}/tenant/{tenant}/workspaces/{id}")).await
}

// ── plugin ─────────────────────────────────────────────────────────

/// Plugin model — mirror of `botwork_entity::plugin::Model`.
///
/// `env`, `resources`, and `egress` are kept as raw `JsonValue` so
/// the UI can render them in a JSON textarea without losing fidelity.
/// api validates these on write via `botwork-api-core` and
/// returns `422 validation_failed` with a precise message if they
/// don't match the schema.
#[derive(Debug, Clone, Deserialize)]
pub struct Plugin {
    pub id: String,
    pub name: String,
    pub image: String,
    pub port: u64,
    pub path: String,
    pub upstream_auth: String,
    pub env: JsonValue,
    pub resources: Option<JsonValue>,
    pub egress: JsonValue,
    pub created_at: String,
    pub updated_at: String,
}

/// POST body for plugin create. Optional fields use the same wire
/// shape as `PluginBody` on the api side — `None` means "use
/// the validator default", not "set to null".
#[derive(Debug, Serialize)]
pub struct PluginCreate {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_auth: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub egress: Option<JsonValue>,
}

/// PUT body for plugin update. Same shape as `PluginCreate` plus the
/// optimistic-lock token. The form sends back the full row (so an
/// unedited field stays at its current value) rather than a sparse
/// patch.
#[derive(Debug, Serialize)]
pub struct PluginUpdate {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_auth: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub egress: Option<JsonValue>,
    pub if_unmodified_since: String,
}

pub async fn list_plugins() -> Result<ListResponse<Plugin>, ApiError> {
    get_json(&format!("{API_BASE}/plugins")).await
}

pub async fn get_plugin(id: &str) -> Result<Plugin, ApiError> {
    get_json(&format!("{API_BASE}/plugins/{id}")).await
}

pub async fn create_plugin(body: &PluginCreate) -> Result<Plugin, ApiError> {
    send_json("POST", &format!("{API_BASE}/plugins"), Some(body)).await
}

pub async fn update_plugin(id: &str, body: &PluginUpdate) -> Result<Plugin, ApiError> {
    send_json("PUT", &format!("{API_BASE}/plugins/{id}"), Some(body)).await
}

/// Plugin delete is RESTRICTed by FK from `workspace_plugin` — a
/// plugin with live bindings returns `409 has_dependents` with the
/// blocking bindings named.
pub async fn delete_plugin(id: &str) -> Result<(), ApiError> {
    send_no_content(&format!("{API_BASE}/plugins/{id}")).await
}

// ── workspace_plugin ──────────────────────────────────────────────

/// Workspace_plugin binding — composite PK on `(workspace_id,
/// plugin_id)`. Mirror of `botwork_entity::workspace_plugin::Model`.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspacePlugin {
    pub workspace_id: String,
    pub plugin_id: String,
    pub config: Option<JsonValue>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize)]
pub struct WorkspacePluginCreate {
    pub workspace_id: String,
    pub plugin_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<JsonValue>,
}

#[derive(Debug, Serialize)]
pub struct WorkspacePluginUpdate {
    /// Wire contract: `Some(JsonValue::Null)` clears the config;
    /// `None` leaves it unchanged. The serializer below maps that:
    /// `None` ⇒ field absent (skip_serializing_if), explicit null ⇒
    /// `"config": null`.
    pub config: Option<JsonValue>,
    pub if_unmodified_since: String,
}

/// List bindings, optionally filtered by workspace + plugin.
pub async fn list_workspace_plugins(
    tenant: &str,
    workspace_id: Option<&str>,
    plugin_id: Option<&str>,
) -> Result<ListResponse<WorkspacePlugin>, ApiError> {
    let mut url = format!("{API_BASE}/tenant/{tenant}/workspace_plugins");
    let mut sep = '?';
    if let Some(wid) = workspace_id {
        url.push(sep);
        url.push_str(&format!("workspace_id={wid}"));
        sep = '&';
    }
    if let Some(pid) = plugin_id {
        url.push(sep);
        url.push_str(&format!("plugin_id={pid}"));
    }
    get_json(&url).await
}

pub async fn get_workspace_plugin(
    tenant: &str,
    workspace_id: &str,
    plugin_id: &str,
) -> Result<WorkspacePlugin, ApiError> {
    get_json(&format!(
        "{API_BASE}/tenant/{tenant}/workspace_plugins/{workspace_id}/{plugin_id}"
    ))
    .await
}

pub async fn create_workspace_plugin(
    tenant: &str,
    body: &WorkspacePluginCreate,
) -> Result<WorkspacePlugin, ApiError> {
    send_json(
        "POST",
        &format!("{API_BASE}/tenant/{tenant}/workspace_plugins"),
        Some(body),
    )
    .await
}

pub async fn update_workspace_plugin(
    tenant: &str,
    workspace_id: &str,
    plugin_id: &str,
    body: &WorkspacePluginUpdate,
) -> Result<WorkspacePlugin, ApiError> {
    send_json(
        "PUT",
        &format!("{API_BASE}/tenant/{tenant}/workspace_plugins/{workspace_id}/{plugin_id}"),
        Some(body),
    )
    .await
}

/// Binding delete goes through the live-state gate against
/// control-plane. Failure modes: `503 unavailable` if control-plane
/// is unreachable (the DB is rolled back, UI should retry).
pub async fn delete_workspace_plugin(
    tenant: &str,
    workspace_id: &str,
    plugin_id: &str,
) -> Result<(), ApiError> {
    send_no_content(&format!(
        "{API_BASE}/tenant/{tenant}/workspace_plugins/{workspace_id}/{plugin_id}"
    ))
    .await
}

// ── agent_session (read-only) ─────────────────────────────────────

/// agent_session model — mirror of `botwork_entity::agent_session::Model`.
///
/// api exposes read-only access; session-broker owns all writes
/// (state transitions, last_active_at bumps, reactivation_count
/// increments). See `ui/wasm/src/pages/sessions.rs` for the
/// rationale on why CUD doesn't surface here.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentSession {
    pub id: String,
    pub tenant_id: String,
    pub workspace_id: String,
    pub agent_session_id: String,
    pub state: String,
    pub created_at: String,
    pub last_active_at: String,
    pub reactivation_count: i64,
}

/// List agent_sessions with optional filters. `state` matches the
/// `agent_session::state` module constants (active / grace /
/// inactive / teardown_requested / purged) verbatim.
pub async fn list_agent_sessions(
    tenant: &str,
    workspace_id: Option<&str>,
    state: Option<&str>,
) -> Result<ListResponse<AgentSession>, ApiError> {
    let mut url = format!("{API_BASE}/tenant/{tenant}/agent_sessions");
    let mut sep = '?';
    if let Some(wid) = workspace_id {
        url.push(sep);
        url.push_str(&format!("workspace_id={wid}"));
        sep = '&';
    }
    if let Some(st) = state {
        url.push(sep);
        url.push_str(&format!("state={st}"));
    }
    get_json(&url).await
}

pub async fn get_agent_session(tenant: &str, id: &str) -> Result<AgentSession, ApiError> {
    get_json(&format!("{API_BASE}/tenant/{tenant}/agent_sessions/{id}")).await
}

// ── session_worker (read-only) ────────────────────────────────────

/// session_worker model — mirror of
/// `botwork_entity::session_worker::Model`.
///
/// `agent_session_id` is nullable (spawn-to-first-bind window).
/// `reaped_at` is nullable (NULL = live; non-NULL = teardown
/// scheduled / done). api exposes read-only access for the
/// same reasons as agent_session.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionWorker {
    pub id: String,
    pub agent_session_id: Option<String>,
    pub plugin_id: String,
    pub container_name: String,
    pub container_ip: String,
    pub mcp_session_id: String,
    pub spawned_at: String,
    pub reaped_at: Option<String>,
}

/// List session_workers with optional filters. `live=Some(true)`
/// filters to `reaped_at IS NULL`, `Some(false)` to `IS NOT NULL`,
/// `None` doesn't constrain.
pub async fn list_session_workers(
    tenant: &str,
    agent_session_id: Option<&str>,
    plugin_id: Option<&str>,
    live: Option<bool>,
) -> Result<ListResponse<SessionWorker>, ApiError> {
    let mut url = format!("{API_BASE}/tenant/{tenant}/session_workers");
    let mut sep = '?';
    if let Some(aid) = agent_session_id {
        url.push(sep);
        url.push_str(&format!("agent_session_id={aid}"));
        sep = '&';
    }
    if let Some(pid) = plugin_id {
        url.push(sep);
        url.push_str(&format!("plugin_id={pid}"));
        sep = '&';
    }
    if let Some(live) = live {
        url.push(sep);
        url.push_str(&format!("live={live}"));
    }
    get_json(&url).await
}

pub async fn get_session_worker(tenant: &str, id: &str) -> Result<SessionWorker, ApiError> {
    get_json(&format!("{API_BASE}/tenant/{tenant}/session_workers/{id}")).await
}

// ── auth ─────────────────────────────────────────────────────────

/// Response body from `GET /api/auth/whoami` (auth-broker contract).
///
/// Returns `None` if the request returns 401 (unauthenticated).
/// Other errors are silently treated as unauthenticated for the
/// purposes of the boot-time redirect check.
#[derive(Debug, Deserialize)]
struct WhoamiResponse {
    pub tenant: String,
}

/// Probe `GET /api/auth/whoami` and return the tenant name if the
/// request has a valid active cap (cookie or bearer).  Returns `None`
/// on 401 or any transport error so callers can treat it as "not
/// authenticated".
pub async fn whoami() -> Option<String> {
    get_json::<WhoamiResponse>("/api/auth/whoami")
        .await
        .ok()
        .map(|r| r.tenant)
}

/// Issue `POST /api/auth/logout` to evict the active cap + clear the
/// `botwork_cap` cookie.  Returns `Ok(())` on 200/204 or any
/// successful response; caller should redirect to `/login` regardless
/// of the return value (the cookie is cleared server-side).
pub async fn logout() -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    let opts = web_sys::RequestInit::new();
    opts.set_method("POST");
    let req = web_sys::Request::new_with_str_and_init("/api/auth/logout", &opts)
        .map_err(|e| format!("{e:?}"))?;
    let resp: web_sys::Response = JsFuture::from(window.fetch_with_request(&req))
        .await
        .map_err(|e| format!("{e:?}"))?
        .dyn_into()
        .map_err(|e| format!("{e:?}"))?;
    if resp.ok() {
        Ok(())
    } else {
        Err(format!("logout HTTP {}", resp.status()))
    }
}

// ── transport ───────────────────────────────────────────────────

async fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, ApiError> {
    let request = build_request("GET", url, &Headers::new().map_err(transport)?)?;
    fetch_and_decode(request).await
}

/// Issue a DELETE expecting `204 No Content`. Lifted out of the
/// per-entity callers to keep the operator-header + envelope
/// handling consistent across all 4 deletable entities.
async fn send_no_content(url: &str) -> Result<(), ApiError> {
    let _: serde_json::Value = send_json::<(), serde_json::Value>("DELETE", url, None).await?;
    Ok(())
}

async fn send_json<B: Serialize, T: serde::de::DeserializeOwned>(
    method: &str,
    url: &str,
    body: Option<&B>,
) -> Result<T, ApiError> {
    let headers = Headers::new().map_err(transport)?;
    headers
        .set("x-botwork-admin", OPERATOR)
        .map_err(transport)?;
    if body.is_some() {
        headers
            .set("content-type", "application/json")
            .map_err(transport)?;
    }
    headers
        .set("accept", "application/json")
        .map_err(transport)?;

    let mut request = build_request(method, url, &headers)?;

    if let Some(body) = body {
        let body_json = serde_json::to_string(body).map_err(|err| ApiError::Other {
            status: 0,
            message: format!("serialize body: {err}"),
        })?;
        let opts = RequestInit::new();
        opts.set_method(method);
        opts.set_headers(&headers);
        opts.set_body(&JsValue::from_str(&body_json));
        request = Request::new_with_str_and_init(url, &opts).map_err(transport)?;
    }

    fetch_and_decode(request).await
}

fn build_request(method: &str, url: &str, headers: &Headers) -> Result<Request, ApiError> {
    let opts = RequestInit::new();
    opts.set_method(method);
    opts.set_headers(headers);
    Request::new_with_str_and_init(url, &opts).map_err(transport)
}

async fn fetch_and_decode<T: serde::de::DeserializeOwned>(request: Request) -> Result<T, ApiError> {
    let window = web_sys::window().ok_or_else(|| ApiError::Other {
        status: 0,
        message: "no window".to_string(),
    })?;
    let resp_value = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(transport)?;
    let resp: Response = resp_value.dyn_into().map_err(|_| ApiError::Other {
        status: 0,
        message: "non-Response from fetch".to_string(),
    })?;
    let status = resp.status();
    let body_text = JsFuture::from(resp.text().map_err(transport)?)
        .await
        .map_err(transport)?
        .as_string()
        .unwrap_or_default();
    if !resp.ok() {
        // Try to decode the structured envelope; if the server
        // returned a non-JSON body just wrap the raw text in Other.
        if let Ok(env) = serde_json::from_str::<ErrorEnvelope>(&body_text) {
            return Err(env.into_api_error(status));
        }
        return Err(ApiError::Other {
            status,
            message: if body_text.is_empty() {
                format!("HTTP {status}")
            } else {
                body_text
            },
        });
    }
    if body_text.is_empty() {
        // 204 No Content path (DELETE). Decode null into the unit
        // type via serde_json — `T` will be `serde_json::Value::Null`
        // for the DELETE caller's `Value` placeholder.
        return serde_json::from_str("null").map_err(|err| ApiError::Other {
            status,
            message: format!("decode empty body as {}: {err}", std::any::type_name::<T>()),
        });
    }
    serde_json::from_str(&body_text).map_err(|err| ApiError::Other {
        status,
        message: format!("decode body as {}: {err}", std::any::type_name::<T>()),
    })
}

fn transport(err: JsValue) -> ApiError {
    ApiError::Other {
        status: 0,
        message: format!("transport: {err:?}"),
    }
}
