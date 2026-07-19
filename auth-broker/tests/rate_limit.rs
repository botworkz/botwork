//! `rate_limit` — integration test for the per-`(tenant, IP)`
//! token-bucket rate limiter on the four OPAQUE auth endpoints.
//!
//! ## What this pins
//!
//! - Exceeding the configured burst limit returns `429` with a
//!   `Retry-After` header and the structured error envelope.
//! - Requests within the burst limit succeed (existing behaviour
//!   is unaffected when the limit is not exceeded).
//! - The rate-limit response shape is identical for known-tenant
//!   and unknown-tenant paths (enumeration safety).
//!
//! ## No Docker required
//!
//! The server is started with mock stores (no postgres). The
//! register/login endpoints 404 or 400 on mock stores rather than
//! succeeding, but the 429 is returned *before* the store is ever
//! reached, so the test does not need a real database.

mod common;

use axum::http::StatusCode;
use botwork_auth_broker::{auth::RateLimitConfig, build_router, AppState};
use reqwest::Client;
use serde_json::json;
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use common::offline_auth_state;

// ---------------------------------------------------------------------------
// Test-server setup
// ---------------------------------------------------------------------------

/// Start a broker with a very tight rate limit (1 req/s, burst 2) so
/// the test can exhaust the bucket quickly without needing real time.
async fn spawn_with_tight_limit() -> (String, JoinHandle<()>) {
    let auth = offline_auth_state()
        .await
        .with_rate_limiter(RateLimitConfig {
            rate_per_second: 1,
            burst: 2,
            disabled: false,
        });
    let dir = tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);
    let state = AppState::with_auth(path, auth);
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

/// Start a broker with the default (disabled) rate limiter to confirm
/// normal requests are unaffected.
async fn spawn_with_disabled_limit() -> (String, JoinHandle<()>) {
    let auth = offline_auth_state().await; // rate limiting disabled by default
    let dir = tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);
    let state = AppState::with_auth(path, auth);
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

// ---------------------------------------------------------------------------
// /auth/login/start: 429 after burst
// ---------------------------------------------------------------------------

/// Fire `burst + 1` requests at `/auth/login/start`; the last one must
/// return `429` with a `Retry-After` header.
#[tokio::test]
async fn login_start_returns_429_after_burst_exceeded() {
    let (base, _handle) = spawn_with_tight_limit().await;
    let client = Client::new();

    // Minimal valid base64 for the login_request field (won't pass OPAQUE
    // validation but the rate check fires *before* OPAQUE processing).
    let fake_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, b"fake");

    let body = json!({
        "tenant": "acme",
        "credential_identifier": "alice",
        "login_request": fake_b64,
    });

    // First `burst` (2) requests should NOT return 429.
    for i in 0..2 {
        let resp = client
            .post(format!("{base}/auth/login/start"))
            .json(&body)
            .send()
            .await
            .expect("request");
        assert_ne!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "request {i} should not be rate-limited (within burst)"
        );
    }

    // Request `burst + 1` must be rate-limited.
    let resp = client
        .post(format!("{base}/auth/login/start"))
        .json(&body)
        .send()
        .await
        .expect("request");

    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "request beyond burst must return 429"
    );

    // Must carry a Retry-After header with a positive integer value.
    let retry_after = resp
        .headers()
        .get("retry-after")
        .expect("Retry-After header must be present")
        .to_str()
        .expect("Retry-After must be ASCII")
        .parse::<u64>()
        .expect("Retry-After must be a positive integer");
    assert!(
        retry_after >= 1,
        "Retry-After must be >= 1, got {retry_after}"
    );

    // Body must use the structured error envelope.
    let json_body: serde_json::Value = resp.json().await.expect("JSON body");
    assert_eq!(
        json_body["error"]["code"].as_str(),
        Some("rate_limited"),
        "error code must be 'rate_limited'"
    );
}

// ---------------------------------------------------------------------------
// /auth/register/start: 429 after burst
// ---------------------------------------------------------------------------

#[tokio::test]
async fn register_start_returns_429_after_burst_exceeded() {
    let (base, _handle) = spawn_with_tight_limit().await;
    let client = Client::new();

    let fake_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, b"fake");
    let body = json!({
        "tenant": "acme",
        "credential_identifier": "alice",
        "registration_request": fake_b64,
    });

    // Consume the burst.
    for _ in 0..2 {
        let _ = client
            .post(format!("{base}/auth/register/start"))
            .json(&body)
            .send()
            .await
            .expect("request");
    }

    let resp = client
        .post(format!("{base}/auth/register/start"))
        .json(&body)
        .send()
        .await
        .expect("request");

    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(resp.headers().contains_key("retry-after"));
}

// ---------------------------------------------------------------------------
// Enumeration safety: same 429 shape for known and unknown tenants
// ---------------------------------------------------------------------------

/// The 429 response for an unknown tenant must be identical in shape
/// to the one for a known-but-rate-limited tenant. The limiter keys
/// on the *requested* tenant string and never consults the store.
#[tokio::test]
async fn rate_limit_response_identical_for_unknown_vs_known_tenant() {
    let (base, _handle) = spawn_with_tight_limit().await;
    let client = Client::new();

    let fake_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, b"fake");

    let hit_limit = |tenant: &str| {
        let base = base.clone();
        let client = client.clone();
        let body = json!({
            "tenant": tenant,
            "credential_identifier": "alice",
            "login_request": fake_b64.clone(),
        });
        async move {
            // Consume burst.
            for _ in 0..2u32 {
                let _ = client
                    .post(format!("{base}/auth/login/start"))
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
            }
            // The next request must be 429.
            client
                .post(format!("{base}/auth/login/start"))
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    };

    let resp_unknown = hit_limit("no-such-tenant-xyz").await;
    let resp_known = hit_limit("some-known-tenant-abc").await;

    // Both must be 429.
    assert_eq!(resp_unknown.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(resp_known.status(), StatusCode::TOO_MANY_REQUESTS);

    // Both must carry Retry-After.
    assert!(resp_unknown.headers().contains_key("retry-after"));
    assert!(resp_known.headers().contains_key("retry-after"));

    // Both must use the same error code.
    let body_unknown: serde_json::Value = resp_unknown.json().await.unwrap();
    let body_known: serde_json::Value = resp_known.json().await.unwrap();

    assert_eq!(
        body_unknown["error"]["code"], body_known["error"]["code"],
        "error code must be identical for unknown vs known tenant"
    );
}

// ---------------------------------------------------------------------------
// Below-limit sequence still succeeds (disabled limiter)
// ---------------------------------------------------------------------------

/// When the limiter is disabled (the default for tests), requests never
/// get a 429 and the broker processes them normally.
#[tokio::test]
async fn below_limit_sequence_with_disabled_limiter_does_not_429() {
    let (base, _handle) = spawn_with_disabled_limit().await;
    let client = Client::new();

    let fake_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, b"fake");
    let body = json!({
        "tenant": "acme",
        "credential_identifier": "alice",
        "login_request": fake_b64,
    });

    // Fire more requests than any burst would normally allow and confirm
    // none of them are 429.
    for i in 0..10 {
        let resp = client
            .post(format!("{base}/auth/login/start"))
            .json(&body)
            .send()
            .await
            .expect("request");
        assert_ne!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "request {i} must not be rate-limited when limiter is disabled"
        );
    }
}
