//! HTTP handlers for the embedded ui bundle.
//!
//! # Routes (Phase 2 reshape — botworkz/space#311)
//!
//! * `GET /healthz` — liveness probe. Returns `{ "status": "ok" }`.
//! * `GET /login` and `GET /login/` — serve the SPA shell from the
//!   embedded `dist/index.html`. The SPA client-side router handles
//!   the login page at this path.
//! * `GET /static/*` — serve named assets from the embedded `dist/`
//!   with the correct `Content-Type`. This prefix is exclusively for
//!   static assets; the ingress envoy routes it here directly.
//! * `GET /{tenant}` and `GET /{tenant}/` — serve the SPA shell.
//!   The tenant name is a client-side router parameter; the server
//!   does not validate it (envoy's ext_authz is the gate).
//! * `GET /{tenant}/*` — deep links inside a tenant's SPA shell
//!   fall back to `index.html` so a hard reload keeps working.
//!
//! `/admin/*` is **not** served here — that URL space is retired in
//! Phase 2. The ingress envoy no longer routes it to this service.
//!
//! The bundle is pulled from `ui/wasm/dist/` at compile time via
//! [`include_dir!`]. If `dist/` is missing or empty at compile time
//! the build still succeeds but the resulting binary serves only
//! `/healthz` — every SPA path 404s. The integration test asserts
//! that `index.html` is actually present so this failure mode is loud
//! in CI, not a runtime mystery.

use axum::body::Body;
use axum::extract::Path;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use include_dir::{include_dir, Dir};
use serde::Serialize;
use tracing::debug;

const PREFIX: &str = "[ui]";

/// The trunk-built bundle. `CARGO_MANIFEST_DIR` resolves to
/// `ui/server/`; up one level and across into `wasm/dist/`.
///
/// If this path doesn't exist at compile time the macro produces an
/// empty `Dir`, and only `/healthz` will respond.
static BUNDLE: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../wasm/dist");

#[derive(Debug, Serialize)]
struct HealthBody {
    status: &'static str,
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(HealthBody { status: "ok" }))
}

/// Serve `index.html` for the login page and tenant SPA shells.
async fn index() -> Response {
    serve_path("index.html")
}

/// Serve named assets from the embedded `dist/` bundle.
///
/// Path comes in as the part after `/static/`. Falls back to
/// `index.html` for unknown paths (for future static-routed SPA
/// deep links, though in practice `/static/*` should only ever
/// receive real asset requests).
async fn static_asset(Path(rest): Path<String>) -> Response {
    let trimmed = rest.trim_start_matches('/');
    if trimmed.is_empty() {
        return serve_path("index.html");
    }
    if BUNDLE.get_file(trimmed).is_some() {
        serve_path(trimmed)
    } else {
        debug!("{PREFIX} static fallback to index.html for {trimmed}");
        serve_path("index.html")
    }
}

/// Serve any file from the embedded `dist/` bundle for tenant-scoped
/// deep links.
///
/// Unknown paths fall back to `index.html` so a deep-linked
/// client-side route reload (`/{tenant}/workspaces/abc` → browser
/// requests that path from the server, server has no such file,
/// returns the SPA shell, SPA router takes over) works without
/// server-side knowledge of the route table.
async fn tenant_asset(Path((_tenant, rest)): Path<(String, String)>) -> Response {
    let trimmed = rest.trim_start_matches('/');
    if trimmed.is_empty() {
        return serve_path("index.html");
    }
    if BUNDLE.get_file(trimmed).is_some() {
        serve_path(trimmed)
    } else {
        debug!("{PREFIX} fallback to index.html for {trimmed}");
        serve_path("index.html")
    }
}

/// Build a Response for a single embedded file. Returns 404 when
/// the file isn't present in `dist/`.
fn serve_path(rel: &str) -> Response {
    let Some(file) = BUNDLE.get_file(rel) else {
        return (
            StatusCode::NOT_FOUND,
            format!("{PREFIX} asset {rel} not in bundle"),
        )
            .into_response();
    };
    let mime = mime_guess::from_path(rel).first_or_octet_stream();
    let mut response = Response::new(Body::from(file.contents()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.essence_str())
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    response
}

/// Construct the router. Exposed so the integration test can mount
/// the same routes against `127.0.0.1:0`.
pub fn build_router() -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        // Login page — serves SPA shell; client-side router handles login UI.
        .route("/login", get(index))
        .route("/login/", get(index))
        // Static assets under /static/*.
        .route("/static/*rest", get(static_asset))
        // Tenant SPA shell: /{tenant} and /{tenant}/ serve index.html.
        // /{tenant}/* deep links also fall back to index.html via tenant_asset.
        .route("/:tenant", get(index))
        .route("/:tenant/", get(index))
        .route("/:tenant/*rest", get(tenant_asset))
}
