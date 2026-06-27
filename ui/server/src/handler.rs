//! HTTP handlers for the embedded ui bundle.
//!
//! v0 surface: `/healthz` and `/admin/*`. See `lib.rs` for the
//! full route table + deploy posture.
//!
//! The bundle is pulled from `ui/wasm/dist/` at compile time
//! via [`include_dir!`]. The path is workspace-relative *to this
//! crate's manifest*, so the build flow is:
//!
//! 1. `trunk build --release` in `ui/wasm/` populates
//!    `ui/wasm/dist/` with `index.html`, the wasm-bindgen JS
//!    loader, and the `.wasm` blob.
//! 2. `cargo build --release -p botwork-ui-server` inlines
//!    that directory into the binary.
//! 3. The runtime container has no filesystem dependency.
//!
//! If `dist/` is missing or empty at compile time the build still
//! succeeds (include_dir is silent about empty inputs), but the
//! resulting binary serves only `/healthz` — every `/admin/*` path
//! 404s. The integration test `tests/integration.rs` asserts that
//! `index.html` is actually present so this failure mode is loud
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
/// empty `Dir`, and only `/healthz` will respond. The CI image build
/// runs `trunk build --release` before `cargo build`, so the
/// production binary always has a populated bundle. Local dev should
/// use `trunk serve` (loopback :8080) rather than this server.
static BUNDLE: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../wasm/dist");

#[derive(Debug, Serialize)]
struct HealthBody {
    status: &'static str,
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(HealthBody { status: "ok" }))
}

/// Serve `/admin/` and `/admin/index.html` from the embedded
/// `dist/index.html`.
async fn index() -> Response {
    serve_path("index.html")
}

/// Serve any other file from the embedded `dist/`.
///
/// Unknown paths fall back to `index.html` so a deep-linked
/// client-side route reload (`/admin/tenants/abc` → browser
/// requests `/admin/tenants/abc` from the server, server has no
/// such file, returns the SPA shell, SPA router takes over) works
/// without server-side knowledge of the route table.
async fn asset(Path(rest): Path<String>) -> Response {
    // Strip a leading slash so `BUNDLE.get_file("foo/bar")` (no
    // leading slash) matches what trunk emits.
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
/// the file isn't present in `dist/` — only used from `asset` for
/// files the asset handler already verified, and from `index` for
/// the well-known `index.html` (so a 404 here means trunk hasn't
/// been run, and we want the loud failure).
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
        // mime is `&'static` in the common case but always parses
        // back from its own string form.
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
        .route("/admin/", get(index))
        .route("/admin/index.html", get(index))
        .route("/admin/*rest", get(asset))
}
