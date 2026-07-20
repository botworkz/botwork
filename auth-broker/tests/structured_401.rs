//! Issue #125: structured 401 response contract — round-1b port.
//!
//! This file is the round-1b port of the original
//! `tests/structured_401.rs` (deleted, then ported back per
//! PR #147 review). Every assertion the pre-cutover file made
//! against the legacy bearer-as-vault-password path now runs
//! against the OPAQUE lease-only path. The wire contract is the
//! same:
//!
//! - HTTP status = 401
//! - `Content-Type: application/json`
//! - `WWW-Authenticate: Bearer realm="botspace/…", error="<code>",
//!   error_description="…"` per RFC 6750 §3
//! - JSON body shape `{ "error": { "code", "message",
//!   "remediation": { command, docs_url } } }`
//!
//! Round-1b shape changes (vs the deleted pre-cutover file):
//!
//! - `build_app_state(root, false)` is gone. The broker now
//!   requires an `AuthState`; tests construct one via
//!   `common::offline_auth_state()` which lazy-connects to an
//!   unreachable postgres so any test path that actually queries
//!   the DB hangs (deliberately — the 401 paths under test all
//!   reject *upstream* of the lease lookup).
//! - The `wrong_password_emits_invalid_bearer_with_tenant_scope`
//!   case used to seed a vault with `Vault::create(root, b"pw",
//!   KdfProfile::FastTesting)` and assert that a wrong password
//!   401s. Post-cutover the equivalent is "any bearer that the
//!   lease lookup doesn't recognise 401s as invalid_bearer", which
//!   is the property the `legacy_path_is_gone_unknown_bearer_returns_401`
//!   test in `opaque_e2e` already pins end-to-end against a real
//!   lease DB; we keep the upstream-of-DB version here so the wire
//!   shape is exercised without docker. The path-shaped bearer
//!   that gets rejected for length here is the round-1b stand-in.
//! - The `fetch_with_*` cases work unchanged — `/secrets/fetch`
//!   401s before reading the cap up against the cache, so no DB
//!   plumbing is needed.

mod common;

use axum::body::{to_bytes, Body};
use axum::http::Request;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use botwork_auth_broker::handler::BOTWORK_CAP_COOKIE_NAME;
use botwork_auth_broker::{build_router, AppState, DOCS_URL};
use http::StatusCode;
use serde::Deserialize;
use tempfile::tempdir;
use tower::ServiceExt;

use common::{bearer, offline_auth_state};

#[derive(Debug, Deserialize)]
struct Remediation {
    command: String,
    docs_url: String,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
    remediation: Remediation,
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

struct Captured {
    status: StatusCode,
    content_type: Option<String>,
    www_authenticate: Option<String>,
    body: ErrorEnvelope,
    raw_body: String,
}

async fn send_auth(
    app: &axum::Router,
    authorization: Option<&str>,
    original_path: Option<&str>,
) -> Captured {
    let mut builder = Request::builder().method("POST").uri("/auth/check");
    if let Some(value) = authorization {
        builder = builder.header("authorization", value);
    }
    if let Some(value) = original_path {
        builder = builder.header("x-envoy-original-path", value);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    capture(response).await
}

async fn send_fetch(app: &axum::Router, cap: Option<&str>) -> Captured {
    let mut builder = Request::builder().method("POST").uri("/secrets/fetch");
    if let Some(cap) = cap {
        builder = builder.header("x-botwork-cap", cap);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    capture(response).await
}

async fn send_whoami(
    app: &axum::Router,
    authorization: Option<&str>,
    cookie: Option<&str>,
) -> Captured {
    let mut builder = Request::builder().method("GET").uri("/api/auth/whoami");
    if let Some(value) = authorization {
        builder = builder.header("authorization", value);
    }
    if let Some(value) = cookie {
        builder = builder.header("cookie", value);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    capture(response).await
}

async fn send_logout(
    app: &axum::Router,
    authorization: Option<&str>,
    cookie: Option<&str>,
) -> axum::http::Response<Body> {
    let mut builder = Request::builder().method("POST").uri("/api/auth/logout");
    if let Some(value) = authorization {
        builder = builder.header("authorization", value);
    }
    if let Some(value) = cookie {
        builder = builder.header("cookie", value);
    }
    app.clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn capture(response: axum::http::Response<Body>) -> Captured {
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let www_authenticate = response
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let raw_body = String::from_utf8(bytes.to_vec()).expect("UTF-8 body");
    let body: ErrorEnvelope =
        serde_json::from_str(&raw_body).expect("401 body must be ErrorEnvelope JSON");
    Captured {
        status,
        content_type,
        www_authenticate,
        body,
        raw_body,
    }
}

fn assert_envelope(captured: &Captured, expected_code: &str) {
    assert_eq!(captured.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        captured.content_type.as_deref(),
        Some("application/json"),
        "Content-Type must be application/json, body={}",
        captured.raw_body
    );
    let header = captured
        .www_authenticate
        .as_deref()
        .expect("WWW-Authenticate must be set");
    assert!(
        header.starts_with("Bearer realm=\"botspace/"),
        "WWW-Authenticate must start with Bearer + botspace realm, got {header}"
    );
    assert!(
        header.contains(&format!("error=\"{expected_code}\"")),
        "WWW-Authenticate must include error={expected_code}, got {header}"
    );
    assert!(
        header.contains("error_description=\""),
        "WWW-Authenticate must include error_description, got {header}"
    );

    assert_eq!(captured.body.error.code, expected_code);
    assert!(!captured.body.error.message.is_empty());
    assert!(!captured.body.error.remediation.command.is_empty());
    assert_eq!(captured.body.error.remediation.docs_url, DOCS_URL);
}

async fn build_offline_app() -> axum::Router {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let state = AppState::with_auth(root, offline_auth_state().await);
    build_router(state)
}

#[tokio::test]
async fn missing_authorization_with_valid_path_emits_missing_bearer() {
    let app = build_offline_app().await;
    let captured = send_auth(&app, None, Some("/phlax/ns/exec-bash")).await;
    assert_envelope(&captured, "missing_bearer");
    assert!(
        captured.body.error.message.contains("phlax"),
        "tenant-scoped 401 must mention tenant, body={}",
        captured.raw_body
    );
    assert_eq!(captured.body.error.remediation.command, "bw --tenant phlax");
    assert!(
        captured
            .www_authenticate
            .as_deref()
            .unwrap()
            .starts_with("Bearer realm=\"botspace/phlax\""),
        "realm must carry tenant when known"
    );
}

#[tokio::test]
async fn non_bearer_authorization_emits_invalid_bearer() {
    let app = build_offline_app().await;
    let captured = send_auth(&app, Some("Banana foo"), Some("/phlax/ns/exec-bash")).await;
    assert_envelope(&captured, "invalid_bearer");
}

#[tokio::test]
async fn bad_path_emits_invalid_bearer_with_unscoped_remediation() {
    let app = build_offline_app().await;
    let captured = send_auth(&app, Some(&bearer("pw")), Some("/tenant/plugin")).await;
    assert_envelope(&captured, "invalid_bearer");
    assert_eq!(captured.body.error.remediation.command, "bw");
    assert!(
        !captured.body.error.message.contains("tenant '"),
        "no tenant should be named when the path is malformed, body={}",
        captured.raw_body
    );
    let header = captured.www_authenticate.as_deref().unwrap();
    assert!(
        header.starts_with("Bearer realm=\"botspace/\","),
        "realm must drop tenant when unknown, got {header}"
    );
}

#[tokio::test]
async fn bearer_not_a_lease_shape_emits_invalid_bearer_with_tenant_scope() {
    // Round-1b replacement for the pre-cutover
    // `wrong_password_emits_invalid_bearer_with_tenant_scope` test.
    //
    // Pre-cutover: seed a vault under `<root>/phlax/` with a known
    // password, hit `/auth/check` with the wrong password, assert
    // 401 with `invalid_bearer` and the `--tenant phlax` remediation.
    //
    // Post-cutover: the broker no longer treats the bearer as a
    // vault password. It treats it as a 32-byte base64 lease bearer.
    // A garbage non-base64 bearer is rejected by the *length* check
    // in `try_lease_path` BEFORE the lease lookup fires — the path
    // 401s without touching the DB. That's the property we pin here:
    // the structured 401 still carries the tenant from the path, and
    // the wire shape is unchanged from #125.
    //
    // The semantic "wrong password" case (well-formed bearer, no
    // matching lease row) is exercised end-to-end in
    // `opaque_e2e::legacy_path_is_gone_unknown_bearer_returns_401`
    // which carries the docker gate.
    let app = build_offline_app().await;
    let captured = send_auth(
        &app,
        Some(&bearer("definitely-not-a-base64-lease-bearer")),
        Some("/phlax/ns/exec-bash"),
    )
    .await;
    assert_envelope(&captured, "invalid_bearer");
    assert_eq!(captured.body.error.remediation.command, "bw --tenant phlax");
}

#[tokio::test]
async fn fetch_with_no_cap_emits_missing_bearer() {
    let app = build_offline_app().await;
    let captured = send_fetch(&app, None).await;
    assert_envelope(&captured, "missing_bearer");
    assert_eq!(captured.body.error.remediation.command, "bw");
}

#[tokio::test]
async fn fetch_with_malformed_cap_emits_invalid_bearer() {
    let app = build_offline_app().await;
    let captured = send_fetch(&app, Some("%%%")).await;
    assert_envelope(&captured, "invalid_bearer");
}

#[tokio::test]
async fn fetch_with_unknown_cap_emits_invalid_bearer() {
    let app = build_offline_app().await;
    let unknown = URL_SAFE_NO_PAD.encode([1u8; 32]);
    let captured = send_fetch(&app, Some(&unknown)).await;
    assert_envelope(&captured, "invalid_bearer");
}

#[tokio::test]
async fn realm_drops_tenant_on_fetch_404s() {
    let app = build_offline_app().await;
    let captured = send_fetch(&app, Some("%%%")).await;
    let header = captured.www_authenticate.as_deref().unwrap();
    assert!(
        header.starts_with("Bearer realm=\"botspace/\","),
        "fetch 401 realm must not carry tenant, got {header}"
    );
    assert_eq!(captured.body.error.remediation.command, "bw");
}

#[tokio::test]
async fn api_auth_whoami_without_bearer_emits_missing_bearer() {
    let app = build_offline_app().await;
    let captured = send_whoami(&app, None, None).await;
    assert_envelope(&captured, "missing_bearer");
    assert_eq!(captured.body.error.remediation.command, "bw");
    let header = captured.www_authenticate.as_deref().unwrap();
    assert!(
        header.contains(concat!("realm=\"", "botspace/\",")),
        "whoami 401 realm must not carry tenant, got {header}"
    );
}

#[tokio::test]
async fn api_auth_whoami_with_malformed_bearer_emits_invalid_bearer() {
    let app = build_offline_app().await;
    let captured = send_whoami(
        &app,
        Some(concat!("Bearer ", "obviously-not-a-real-bearer-zzz")),
        None,
    )
    .await;
    assert_envelope(&captured, "invalid_bearer");
}

#[tokio::test]
async fn api_auth_whoami_with_non_bearer_authorization_emits_invalid_bearer() {
    let app = build_offline_app().await;
    let captured = send_whoami(&app, Some("Banana foo"), None).await;
    assert_envelope(&captured, "invalid_bearer");
}

#[tokio::test]
async fn api_auth_whoami_with_empty_bearer_after_scheme_emits_invalid_bearer() {
    let app = build_offline_app().await;
    let captured = send_whoami(&app, Some("Bearer "), None).await;
    assert_envelope(&captured, "invalid_bearer");
}

#[tokio::test]
async fn api_auth_logout_without_bearer_returns_204_and_clears_cookie() {
    let app = build_offline_app().await;
    let response = send_logout(&app, None, None).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        response.headers().get("www-authenticate").is_none(),
        "logout without bearer must not emit WWW-Authenticate"
    );
    assert!(
        response.headers().get("content-type").is_none(),
        "logout without bearer must not emit a JSON content type"
    );
    let set_cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .expect("logout without bearer must clear auth cookie");
    assert!(
        set_cookie.starts_with(&format!(
            "{BOTWORK_CAP_COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0;"
        )),
        "logout must clear the auth cookie, got {set_cookie}"
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(
        body.is_empty(),
        "logout without bearer must not emit a 401 JSON body, got {}",
        String::from_utf8_lossy(&body)
    );
}
