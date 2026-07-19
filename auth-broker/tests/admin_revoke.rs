//! `admin_revoke` — pin the acceptance criteria for the admin
//! `DELETE /admin/api/v1/leases/:id` endpoint.
//!
//! Test coverage per the issue acceptance criteria:
//!
//! 1. **Negative-auth: no bearer** — unauthenticated request returns 401.
//! 2. **Negative-auth: wrong bearer** — wrong admin key returns 401.
//! 3. **Negative-auth: no admin key configured** — when
//!    `admin_api_key` is `None` on `AppState`, every call returns 401
//!    (the surface is disabled by default).
//! 4. **Successful revocation** — correct bearer → 200, postgres
//!    `revoke_by_id` is called with the right lease UUID.
//! 5. **Cap cohort eviction** — caps for the revoked lease are evicted
//!    from the in-memory map; caps for other leases survive.

mod common;

use std::sync::Arc;

use botwork_auth_broker::caps::CapEntry;
use botwork_auth_broker::store::mock::{MockLeaseStore, MockPasswordFileStore, MockTenantStore};
use botwork_auth_broker::{build_router, AppState};
use reqwest::StatusCode;
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use uuid::Uuid;

use common::build_offline_app_state;

const ADMIN_KEY: &str = "test-admin-key-do-not-use-in-production";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build an AppState with the admin key configured.
async fn build_state_with_admin_key() -> (AppState, std::path::PathBuf) {
    let (state, path) = build_offline_app_state().await;
    let state = state.with_admin_api_key(ADMIN_KEY);
    (state, path)
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

async fn delete_lease(base: &str, auth: Option<&str>, lease_id: Uuid) -> reqwest::Response {
    let client = reqwest::Client::new();
    let mut req = client.delete(format!("{base}/admin/api/v1/leases/{lease_id}"));
    if let Some(value) = auth {
        req = req.header("authorization", value);
    }
    req.send().await.unwrap()
}

fn admin_bearer() -> String {
    common::bearer(ADMIN_KEY)
}

// ---------------------------------------------------------------------------
// Negative-auth: no admin key configured (surface disabled)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_revoke_disabled_when_no_key_configured() {
    // `build_offline_app_state` does NOT call `with_admin_api_key`, so the
    // surface should be disabled and every call returns 401.
    let (state, _path) = build_offline_app_state().await;
    let (base, handle) = spawn(state).await;

    let lease_id = Uuid::new_v4();
    let resp = delete_lease(&base, Some(&admin_bearer()), lease_id).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "admin surface must be disabled when no key is configured"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// Negative-auth: no Authorization header
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_revoke_no_bearer_returns_401() {
    let (state, _path) = build_state_with_admin_key().await;
    let (base, handle) = spawn(state).await;

    let lease_id = Uuid::new_v4();
    let resp = delete_lease(&base, None, lease_id).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "missing authorization header must yield 401"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// Negative-auth: wrong admin key
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_revoke_wrong_bearer_returns_401() {
    let (state, _path) = build_state_with_admin_key().await;
    let (base, handle) = spawn(state).await;

    let lease_id = Uuid::new_v4();
    let bearer_wrong_key = common::bearer("wrong-key");
    let resp = delete_lease(&base, Some(&bearer_wrong_key), lease_id).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "wrong bearer must yield 401"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// Successful revocation: correct bearer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_revoke_correct_bearer_calls_revoke_by_id() {
    // Wire up the mock lease store so we can inspect the revoke_by_id call.
    let setup = botwork_opaque_handshake::ServerSetup::generate(&mut rand::thread_rng());
    let lease_store = Arc::new(MockLeaseStore::new());
    let tenant_store = Arc::new(MockTenantStore::new());
    let pf_store = Arc::new(MockPasswordFileStore::new());
    let auth = botwork_auth_broker::auth::AuthState::from_stores(
        lease_store.clone(),
        tenant_store,
        pf_store,
        setup,
    );

    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let state = AppState::with_auth(root, auth).with_admin_api_key(ADMIN_KEY);
    let (base, handle) = spawn(state).await;

    let lease_id = Uuid::new_v4();
    let resp = delete_lease(&base, Some(&admin_bearer()), lease_id).await;

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "valid admin call must return 200"
    );

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["revoked"], 1,
        "mock always returns 1 row affected; JSON must reflect that"
    );
    assert_eq!(
        body["caps_evicted"], 0,
        "no caps were seeded, so eviction count must be 0"
    );

    // Verify the mock recorded the correct lease_id.
    let recorded = lease_store.drain_revokes_by_id();
    assert_eq!(
        recorded,
        vec![lease_id],
        "revoke_by_id must be called with the requested lease_id"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// Cap cohort eviction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_revoke_evicts_matching_cap_cohort() {
    use botwork_auth_broker::cache_key;
    use botwork_auth_broker::CAP_TTL;

    let setup = botwork_opaque_handshake::ServerSetup::generate(&mut rand::thread_rng());
    let lease_store = Arc::new(MockLeaseStore::new());
    let tenant_store = Arc::new(MockTenantStore::new());
    let pf_store = Arc::new(MockPasswordFileStore::new());
    let auth = botwork_auth_broker::auth::AuthState::from_stores(
        lease_store.clone(),
        tenant_store,
        pf_store,
        setup,
    );

    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let state = AppState::with_auth(root, auth).with_admin_api_key(ADMIN_KEY);

    // Seed two caps: one for the lease we'll revoke, one for a different lease.
    let target_lease = Uuid::new_v4();
    let other_lease = Uuid::new_v4();
    let ck = cache_key("tenant-a", "some-bearer");

    use botwork_auth_broker::caps::mint_cap_id;
    let cap_id_target = mint_cap_id();
    let cap_id_other = mint_cap_id();

    let now = Instant::now();
    state
        .insert_cap_for_test(
            cap_id_target,
            CapEntry {
                cache_key: ck,
                namespace: "ns".to_string(),
                plugin: "plugin".to_string(),
                expires_at: now + CAP_TTL,
                lease_id: target_lease,
            },
        )
        .await;
    state
        .insert_cap_for_test(
            cap_id_other,
            CapEntry {
                cache_key: ck,
                namespace: "ns".to_string(),
                plugin: "plugin".to_string(),
                expires_at: now + CAP_TTL,
                lease_id: other_lease,
            },
        )
        .await;

    assert_eq!(
        state.caps_len().await,
        2,
        "both caps must be present before revocation"
    );

    let (base, handle) = spawn(state.clone()).await;

    let resp = delete_lease(&base, Some(&admin_bearer()), target_lease).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["caps_evicted"], 1,
        "exactly the target-lease cap must be evicted"
    );

    // The other lease's cap must still be present in the cap map.
    let remaining = state.caps_len().await;
    assert_eq!(remaining, 1, "the non-target cap must survive");

    handle.abort();
}

// ---------------------------------------------------------------------------
// Error shape: JSON envelope on 401
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_revoke_401_has_json_envelope() {
    let (state, _path) = build_state_with_admin_key().await;
    let (base, handle) = spawn(state).await;

    let bearer_wrong_key = common::bearer("wrong-key");
    let resp = delete_lease(&base, Some(&bearer_wrong_key), Uuid::new_v4()).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"]["code"], "unauthorized",
        "401 response must carry structured JSON envelope"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// DB error: revoke_by_id fails → 500 with JSON envelope
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_revoke_db_error_returns_500() {
    let setup = botwork_opaque_handshake::ServerSetup::generate(&mut rand::thread_rng());
    let lease_store = Arc::new(MockLeaseStore::new());
    let tenant_store = Arc::new(MockTenantStore::new());
    let pf_store = Arc::new(MockPasswordFileStore::new());

    // Queue a DB error for the next revoke_by_id call.
    lease_store.push_revoke_by_id_error("simulated postgres error");

    let auth = botwork_auth_broker::auth::AuthState::from_stores(
        lease_store,
        tenant_store,
        pf_store,
        setup,
    );

    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let state = AppState::with_auth(root, auth).with_admin_api_key(ADMIN_KEY);
    let (base, handle) = spawn(state).await;

    let resp = delete_lease(&base, Some(&admin_bearer()), Uuid::new_v4()).await;

    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "DB error during revoke_by_id must yield 500"
    );

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"]["code"], "internal",
        "500 response must carry structured JSON envelope with code=internal"
    );

    handle.abort();
}
