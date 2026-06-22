// SPDX-License-Identifier: Apache-2.0

//! Thin typed HTTP client over `fetch`, talking to admin-api on the
//! same origin.
//!
//! # Wire envelopes
//!
//! admin-api uses two stable wire shapes that every consumer needs to
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
//! read for the row. admin-api compares the submitted value with the
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
//! Every mutation sends `x-botwork-admin: <operator>`. admin-api logs
//! it on the audit event but does not validate it (the docker
//! network is the trust boundary in v0). The UI sets it to a
//! placeholder ("admin-ui") until ext_authz starts asserting an
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
pub const OPERATOR: &str = "admin-ui";

/// Base path the admin-ui talks to. Same-origin by construction:
/// the admin-ui bundle is served from `/admin/` and the ingress
/// envoy routes `/admin/api/*` to admin-api.
pub const API_BASE: &str = "/admin/api/v1";

/// Wire-shape for every list endpoint. `items` is serialised verbatim
/// from the entity model; `total` is the row count after filtering
/// (NOT the unfiltered table size).
#[derive(Debug, Clone, Deserialize)]
pub struct ListResponse<T> {
    pub items: Vec<T>,
    pub total: usize,
}

/// Structured representation of admin-api's error envelope.
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
    /// `dependents` is the verbatim JSON payload (admin-api's
    /// shape is `[{kind, id, name}]` but we keep it loose for
    /// forward-compat).
    HasDependents {
        message: String,
        dependents: JsonValue,
    },
    /// `503 unavailable` — coordination layer is down (typically
    /// control-plane unreachable for a binding mutation). Retryable.
    Unavailable { message: String },
    /// Any other admin-api error, or a transport-layer failure.
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

/// Wire shape of admin-api's error envelope. Used internally to
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
    let url = format!("{API_BASE}/tenants/{id}");
    // DELETE has no body, but we still route through `send_json` for
    // the operator-header + error-envelope handling. The `()` payload
    // serialises to `null`, which admin-api treats as "no body".
    let _: serde_json::Value = send_json::<(), serde_json::Value>("DELETE", &url, None).await?;
    Ok(())
}

// ── transport ───────────────────────────────────────────────────

async fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, ApiError> {
    let request = build_request("GET", url, &Headers::new().map_err(transport)?)?;
    fetch_and_decode(request).await
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
