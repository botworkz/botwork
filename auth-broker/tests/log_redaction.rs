//! `log_redaction` — round-1b port of the deleted
//! `tests/log_redaction.rs`.
//!
//! Pre-cutover this file pinned that bearer tokens, cap values,
//! and raw secret bytes never appear in the broker's log stream.
//! Round 1b doesn't loosen any of those: the bearer is still
//! `Zeroizing<String>`, caps still `redact_token()` in every log
//! line, and the per-secret unlock keeps plaintext bytes out of
//! the cache so they can't accidentally land in a `?cache`
//! debug print.
//!
//! Round-1b shape changes (vs the deleted pre-cutover file):
//!
//! - `build_app_state(root, false)` is gone. Tests use
//!   `common::offline_auth_state()` for the 401 / mint paths and
//!   `common::seed_synthetic_lease` for the fetch path.
//! - The pre-cutover happy-path tests went through `/auth/check`
//!   to mint a cap; today the synthetic-lease seed produces the
//!   same `(cache_entry, cap_entry)` pair without driving
//!   `/auth/check`, which keeps the test docker-free.
//! - The pre-cutover `unlock-cache miss/hit` log lines tested an
//!   internal log call that lived inside the legacy
//!   bearer-as-vault-password path. That path is gone; the
//!   equivalent round-1b log surface is the
//!   `auth/check: lease validated` line, which is exercised
//!   end-to-end in `opaque_e2e`. We don't duplicate that here
//!   because the offline harness can't drive the lease lookup.

mod common;

use std::io;
use std::sync::{Arc, Mutex, Once, OnceLock};

use axum::body::{to_bytes, Body};
use axum::http::Request;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use botwork_auth_broker::caps::decode_cap;
use botwork_auth_broker::{build_router, AppState};
use botwork_vault::SecretKind;
use reqwest::StatusCode;
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant};
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

use common::{bearer, build_offline_app_state, seed_synthetic_lease, SeedSecret};

#[derive(Clone)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

struct SharedWriterGuard(Arc<Mutex<Vec<u8>>>);

impl<'a> MakeWriter<'a> for SharedWriter {
    type Writer = SharedWriterGuard;

    fn make_writer(&'a self) -> Self::Writer {
        SharedWriterGuard(self.0.clone())
    }
}

impl io::Write for SharedWriterGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn log_buffer() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| Arc::new(Mutex::new(Vec::new())))
        .clone()
}

fn init_logs() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let writer = SharedWriter(log_buffer());
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("info"))
            .with_writer(writer)
            .with_target(false)
            .without_time()
            .with_ansi(false)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

fn clear_logs() {
    log_buffer().lock().unwrap().clear();
}

fn collected_logs() -> String {
    String::from_utf8_lossy(&log_buffer().lock().unwrap()).into_owned()
}

fn test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn spawn(state: AppState) -> (String, JoinHandle<()>) {
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

async fn request(base: &str, auth: Option<&str>, original_path: Option<&str>) -> reqwest::Response {
    let client = reqwest::Client::new();
    let mut req = client.post(format!("{base}/auth/check"));
    if let Some(value) = auth {
        req = req.header("authorization", value);
    }
    if let Some(value) = original_path {
        req = req.header("x-envoy-original-path", value);
    }
    req.send().await.unwrap()
}

async fn send_fetch(
    app: &axum::Router,
    cap: Option<&str>,
) -> axum::http::Response<axum::body::Body> {
    let mut builder = Request::builder().method("POST").uri("/secrets/fetch");
    if let Some(cap) = cap {
        builder = builder.header("x-botwork-cap", cap);
    }
    app.clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn assert_single_rejection(logs: &str, expected_fragment: &str) {
    assert_eq!(
        logs.matches("secrets/fetch: rejected").count(),
        1,
        "expected exactly one rejection line: {logs}"
    );
    assert!(
        logs.contains(expected_fragment),
        "missing rejection reason {expected_fragment}: {logs}"
    );
}

#[tokio::test]
async fn raw_bearer_does_not_leak_in_unauthorized_response_or_logs() {
    let _guard = test_lock().lock().await;
    init_logs();
    clear_logs();

    let (state, _) = build_offline_app_state().await;
    let (base, handle) = spawn(state).await;

    let raw_bearer = "supersecret-bearer-value";
    let response = request(&base, Some(&bearer(raw_bearer)), Some("/tenant/ns/plugin")).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let body_text = response.text().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    assert!(
        matches!(
            parsed["error"]["code"].as_str(),
            Some("missing_bearer" | "invalid_bearer")
        ),
        "unexpected body: {body_text}"
    );
    assert!(
        !body_text.contains(raw_bearer),
        "raw bearer leaked in response body: {body_text}"
    );

    let logs = collected_logs();
    assert!(
        !logs.contains(raw_bearer),
        "raw bearer leaked in logs: {logs}"
    );
    assert!(
        logs.contains("supers…"),
        "expected redacted token in logs: {logs}"
    );

    handle.abort();
}

#[tokio::test]
async fn secrets_fetch_ok_logs_counts_and_tuple_without_leaking_values() {
    let _guard = test_lock().lock().await;
    init_logs();
    clear_logs();

    let dir = tempdir().unwrap();
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let (state, _) = build_offline_app_state().await;
    let synth = seed_synthetic_lease(
        &state,
        &vault_root,
        "tenant",
        "ns",
        "plugin-allow",
        "bearer-redaction-aaaaaaaaaaaaaaaaaaaaaaa",
        vec![
            SeedSecret::new("github.com", "pat", SecretKind::ApiKey, b"ghp_xxx")
                .allowed_for(&["plugin-allow"]),
            SeedSecret::new(
                "other.service",
                "token",
                SecretKind::ApiKey,
                b"other_secret",
            )
            .allowed_for(&["other-plugin"]),
        ],
    )
    .await;
    let app = build_router(state);

    let response = send_fetch(&app, Some(&synth.cap_value)).await;
    assert_eq!(response.status(), StatusCode::OK);

    let logs = collected_logs();
    assert!(logs.contains("secrets/fetch: ok tenant=tenant namespace=ns plugin=plugin-allow"));
    assert!(logs.contains("vault_secrets=2 visible_to_plugin=1"));
    assert!(logs.contains("returned=[(github.com,pat,api-key)]"));
    assert!(
        !logs.contains("ghp_xxx"),
        "raw secret leaked in logs: {logs}"
    );
    assert!(
        !logs.contains("Z2hwX3h4eA=="),
        "base64 secret leaked in logs: {logs}"
    );
    assert!(
        !logs.contains(&synth.cap_value),
        "raw cap leaked in logs: {logs}"
    );
}

#[tokio::test]
async fn secrets_fetch_rejection_missing_cap_logs_once_and_redacts() {
    let _guard = test_lock().lock().await;
    init_logs();
    clear_logs();

    let (state, _) = build_offline_app_state().await;
    let app = build_router(state);
    let response = send_fetch(&app, None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let logs = collected_logs();
    assert_single_rejection(&logs, "secrets/fetch: rejected — missing x-botwork-cap");
}

#[tokio::test]
async fn secrets_fetch_rejection_malformed_cap_logs_once_and_redacts() {
    let _guard = test_lock().lock().await;
    init_logs();
    clear_logs();

    let (state, _) = build_offline_app_state().await;
    let app = build_router(state);
    let raw_cap = "%%%";
    let response = send_fetch(&app, Some(raw_cap)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let logs = collected_logs();
    assert_single_rejection(
        &logs,
        "secrets/fetch: rejected — malformed cap (base64url decode failed)",
    );
    assert!(!logs.contains(raw_cap), "raw cap leaked in logs: {logs}");
}

#[tokio::test]
async fn secrets_fetch_rejection_unknown_cap_logs_once_and_redacts() {
    let _guard = test_lock().lock().await;
    init_logs();
    clear_logs();

    let (state, _) = build_offline_app_state().await;
    let app = build_router(state);
    let unknown_cap = URL_SAFE_NO_PAD.encode([9u8; 32]);
    let response = send_fetch(&app, Some(&unknown_cap)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let logs = collected_logs();
    assert_single_rejection(
        &logs,
        "secrets/fetch: rejected — unknown cap (not in cap cache)",
    );
    assert!(
        !logs.contains(&unknown_cap),
        "raw cap leaked in logs: {logs}"
    );
}

#[tokio::test]
async fn secrets_fetch_rejection_expired_cap_logs_once_and_redacts() {
    let _guard = test_lock().lock().await;
    init_logs();
    clear_logs();

    let dir = tempdir().unwrap();
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let (state, _) = build_offline_app_state().await;
    let synth = seed_synthetic_lease(
        &state,
        &vault_root,
        "tenant",
        "ns",
        "plugin-allow",
        "bearer-expired-aaaaaaaaaaaaaaaaaaaaaaaaa",
        vec![
            SeedSecret::new("service", "name", SecretKind::ApiKey, b"value")
                .allowed_for(&["plugin-allow"]),
        ],
    )
    .await;
    let app = build_router(state.clone());

    let cap_id = decode_cap(&synth.cap_value).unwrap();
    {
        let mut caps = state.caps.lock().await;
        let entry = caps.get_mut(&cap_id).unwrap();
        entry.expires_at = Instant::now() - Duration::from_secs(1);
    }

    let response = send_fetch(&app, Some(&synth.cap_value)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let logs = collected_logs();
    assert_single_rejection(&logs, "secrets/fetch: rejected — expired cap age=");
    assert!(logs.contains("ttl=60s"));
    assert!(
        !logs.contains(&synth.cap_value),
        "raw cap leaked in logs: {logs}"
    );
}

#[tokio::test]
async fn secrets_fetch_rejection_orphaned_cap_logs_once_and_redacts() {
    let _guard = test_lock().lock().await;
    init_logs();
    clear_logs();

    let dir = tempdir().unwrap();
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let (state, _) = build_offline_app_state().await;
    let synth = seed_synthetic_lease(
        &state,
        &vault_root,
        "tenant",
        "ns",
        "plugin-allow",
        "bearer-orphaned-aaaaaaaaaaaaaaaaaaaaaaaa",
        vec![
            SeedSecret::new("service", "name", SecretKind::ApiKey, b"value")
                .allowed_for(&["plugin-allow"]),
        ],
    )
    .await;
    let app = build_router(state.clone());

    state.cache.lock().await.clear();
    let response = send_fetch(&app, Some(&synth.cap_value)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();

    let logs = collected_logs();
    assert_single_rejection(
        &logs,
        "secrets/fetch: rejected — orphaned cap (underlying cache entry evicted)",
    );
    assert!(
        !logs.contains(&synth.cap_value),
        "raw cap leaked in logs: {logs}"
    );
}

#[tokio::test]
async fn secrets_fetch_logs_vault_count_distinct_from_visible_count() {
    let _guard = test_lock().lock().await;
    init_logs();
    clear_logs();

    let dir = tempdir().unwrap();
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let (state, _) = build_offline_app_state().await;
    let synth = seed_synthetic_lease(
        &state,
        &vault_root,
        "tenant",
        "ns",
        "plugin-filtered",
        "bearer-vc-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        vec![
            SeedSecret::new("a", "1", SecretKind::ApiKey, b"x").allowed_for(&["other-plugin"]),
            SeedSecret::new("b", "2", SecretKind::ApiKey, b"y").allowed_for(&["other-plugin"]),
        ],
    )
    .await;
    let app = build_router(state);

    let response = send_fetch(&app, Some(&synth.cap_value)).await;
    assert_eq!(response.status(), StatusCode::OK);

    let logs = collected_logs();
    assert!(logs.contains("secrets/fetch: ok tenant=tenant namespace=ns plugin=plugin-filtered"));
    assert!(logs.contains("vault_secrets=2 visible_to_plugin=0"));
    assert!(logs.contains("returned=[]"));
    assert!(
        !logs.contains(&synth.cap_value),
        "raw cap leaked in logs: {logs}"
    );
}
