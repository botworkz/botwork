//! `secrets_write` — offline coverage for the internal secret-store write
//! endpoints: `POST /secrets` and `DELETE /secrets/:service/:name`.
//!
//! Gate: `required-features = ["test-support"]`
//!
//! All tests run without Docker or a live Postgres connection. The DB
//! surface is exercised via the `MockLeaseStore` / `MockTenantStore` pair;
//! the vault surface uses a real on-disk vault created by `Vault::create`
//! inside a `tempfile::tempdir()`.
//!
//! Coverage map
//! ------------
//!
//! `resolve_active_lease` branches:
//!   - Tenant not found → 503 `no_active_lease`
//!   - Tenant lookup DB error → 500 `internal_error`
//!   - No active lease row → 503 `no_active_lease`
//!   - Lease lookup DB error → 500 `internal_error`
//!   - Export key absent from in-memory cache → 503 `no_active_lease`
//!
//! `POST /secrets` branches:
//!   - Invalid service name → 400
//!   - Invalid secret name → 400
//!   - Unknown kind string → 400
//!   - Invalid base64 in `value_b64` → 400
//!   - Secret already exists, `overwrite: false` → 409
//!   - New secret → 200 `{"stored": "svc/name", "created": true}`
//!   - Overwrite existing secret → 200 `{"created": false}`
//!   - Vault unlock error → 500 (vault file removed)
//!   - `put_secret` → `Conflict` → 503 `vault_conflict` (gen-file race)
//!   - `put_secret` → generic vault Err → 500 `internal_error` (gen-file corruption)
//!
//! Lines 159-161 (`has_secret` Err) and 177 (`put_secret` InvalidComponent)
//! are defensive arms unreachable from the HTTP layer without production
//! seams and are deferred to the tarpaulin-exclude follow-up PR.
//!
//! `DELETE /secrets/:service/:name` branches:
//!   - Invalid service name → 400
//!   - Invalid secret name → 400
//!   - Secret not found → 404
//!   - Delete success → 204
//!   - Vault unlock error → 500 (vault file removed)
//!   - `delete_secret` → `Conflict` → 503 `vault_conflict` (gen-file race)
//!   - `delete_secret` → generic vault Err → 500 `internal_error` (gen-file corruption)

mod common;

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::Request;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use botwork_auth_broker::auth::lease::LeaseRow;
use botwork_auth_broker::auth::AuthState;
use botwork_auth_broker::secrets::build_secrets_router;
use botwork_auth_broker::store::mock::{
    ActiveLeaseOutcome, MockLeaseStore, MockPasswordFileStore, MockTenantStore,
};
use botwork_auth_broker::AppState;
use botwork_vault::Vault;
use chrono::{Duration, Utc};
use http::StatusCode;
use rand::Rng;
use serde::Deserialize;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Test harness helpers
// ---------------------------------------------------------------------------

struct SecretsWriteFixture {
    /// The built axum router for the internal secrets API.
    app: axum::Router,
    /// The `AppState` underlying the router (held to keep the vault root alive).
    state: AppState,
    /// Direct reference to the lease store so tests can push outcomes.
    lease_store: Arc<MockLeaseStore>,
    tenant_id: Uuid,
    lease_id: Uuid,
    export_key: [u8; 64],
    _dir: tempfile::TempDir,
}

impl SecretsWriteFixture {
    /// Build the fixture with `MockTenantStore::with_tenant` and a seeded
    /// vault on disk. Calling this method does NOT push any
    /// `ActiveLeaseOutcome`; individual tests enqueue outcomes themselves via
    /// [`Self::push_active_lease`].
    async fn new(tenant: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let vault_root = dir.path().to_path_buf();

        let tenant_id = Uuid::new_v4();
        let lease_id = Uuid::new_v4();
        let mut export_key = [0u8; 64];
        rand::rng().fill_bytes(&mut export_key);

        // Create the vault on disk so unlock_master succeeds.
        let tenant_vault_root = vault_root.join(tenant);
        let suite_version = botwork_opaque_handshake::SUITE_VERSION;
        Vault::create(&tenant_vault_root, &export_key, suite_version).expect("create vault");

        let tenant_store = Arc::new(MockTenantStore::with_tenant(tenant, tenant_id));
        let lease_store = Arc::new(MockLeaseStore::new());
        let pf_store = Arc::new(MockPasswordFileStore::new());
        let setup = botwork_opaque_handshake::ServerSetup::generate(&mut rand::rng());
        let auth = AuthState::from_stores(
            Arc::clone(&lease_store)
                as Arc<dyn botwork_auth_broker::store::LeaseStore + Send + Sync>,
            tenant_store,
            pf_store,
            setup,
        );
        let state = AppState::with_auth(vault_root.clone(), auth);
        let app = build_secrets_router(state.clone());

        SecretsWriteFixture {
            app,
            state,
            lease_store,
            tenant_id,
            lease_id,
            export_key,
            _dir: dir,
        }
    }

    /// Push a `Found` outcome so the next `resolve_active_lease` call
    /// succeeds, and register the export key in the in-memory cache so
    /// `lease_export_key` returns it.
    async fn push_active_lease(&self) {
        let now = Utc::now();
        let expires_at = now + Duration::hours(1);
        let row = LeaseRow {
            id: self.lease_id,
            tenant_id: self.tenant_id,
            issued_at: now,
            expires_at,
            idle_extends_to: expires_at,
            revoked_at: None,
        };
        self.lease_store
            .push_active_lease_outcome(ActiveLeaseOutcome::Found(row));
        self.state
            .auth
            .remember_lease_export_key(self.lease_id, &self.export_key)
            .await;
    }

    /// Push a DB error outcome for `find_active_lease_for_tenant`.
    fn push_lease_db_error(&self, msg: &str) {
        self.lease_store
            .push_active_lease_outcome(ActiveLeaseOutcome::DbError(msg.to_string()));
    }

    // Convenience request senders -----------------------------------------

    async fn post_secret(&self, body: Value) -> axum::http::Response<Body> {
        self.app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/secrets")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn delete_secret(
        &self,
        service: &str,
        name: &str,
        tenant: &str,
    ) -> axum::http::Response<Body> {
        let uri = format!("/secrets/{service}/{name}?tenant={tenant}");
        self.app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }
}

/// Deserialise the response body as JSON; panics if it isn't valid JSON.
async fn body_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).expect("response body is not valid JSON")
}

fn assert_error_code(json: &Value, expected_code: &str) {
    assert_eq!(
        json["error"]["code"].as_str().unwrap_or(""),
        expected_code,
        "unexpected error code in: {json}"
    );
}

/// A minimal store request body that is valid for all fields.
fn store_body(tenant: &str) -> Value {
    json!({
        "tenant": tenant,
        "service": "mysvc",
        "name": "mykey",
        "kind": "api-key",
        "value_b64": STANDARD.encode(b"s3cr3t"),
    })
}

// ---------------------------------------------------------------------------
// resolve_active_lease: tenant not found
// ---------------------------------------------------------------------------

#[tokio::test]
async fn store_tenant_not_found_is_503() {
    let fx = SecretsWriteFixture::new("acme").await;
    // tenant_store has "acme" → tenant_id; use a different name to get
    // a `Ok(None)` from `lookup_tenant_id_by_name`.
    let body = json!({
        "tenant": "unknown-tenant",
        "service": "svc",
        "name": "key",
        "kind": "api-key",
        "value_b64": STANDARD.encode(b"x"),
    });
    let resp = fx.post_secret(body).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = body_json(resp).await;
    assert_error_code(&json, "no_active_lease");
}

// ---------------------------------------------------------------------------
// resolve_active_lease: tenant lookup DB error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn store_tenant_db_error_is_500() {
    let dir = tempfile::tempdir().expect("tempdir");
    let vault_root = dir.path().to_path_buf();

    let tenant_store = Arc::new(MockTenantStore::always_error("db down"));
    let lease_store = Arc::new(MockLeaseStore::new());
    let pf_store = Arc::new(MockPasswordFileStore::new());
    let setup = botwork_opaque_handshake::ServerSetup::generate(&mut rand::rng());
    let auth = AuthState::from_stores(lease_store, tenant_store, pf_store, setup);
    let state = AppState::with_auth(vault_root, auth);
    let app = build_secrets_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/secrets")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&store_body("acme")).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let json = body_json(resp).await;
    assert_error_code(&json, "internal_error");
}

// ---------------------------------------------------------------------------
// resolve_active_lease: no active lease row
// ---------------------------------------------------------------------------

#[tokio::test]
async fn store_no_active_lease_is_503() {
    let fx = SecretsWriteFixture::new("acme").await;
    // Do NOT push an active lease outcome → MockLeaseStore falls back to Ok(None).
    let resp = fx.post_secret(store_body("acme")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = body_json(resp).await;
    assert_error_code(&json, "no_active_lease");
}

// ---------------------------------------------------------------------------
// resolve_active_lease: lease lookup DB error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn store_lease_db_error_is_500() {
    let fx = SecretsWriteFixture::new("acme").await;
    fx.push_lease_db_error("lease table offline");
    let resp = fx.post_secret(store_body("acme")).await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let json = body_json(resp).await;
    assert_error_code(&json, "internal_error");
}

// ---------------------------------------------------------------------------
// resolve_active_lease: export key absent from in-memory cache
// ---------------------------------------------------------------------------

#[tokio::test]
async fn store_export_key_missing_is_503() {
    let fx = SecretsWriteFixture::new("acme").await;
    // Push a Found lease but do NOT call remember_lease_export_key.
    let now = Utc::now();
    let expires_at = now + Duration::hours(1);
    let row = LeaseRow {
        id: fx.lease_id,
        tenant_id: fx.tenant_id,
        issued_at: now,
        expires_at,
        idle_extends_to: expires_at,
        revoked_at: None,
    };
    fx.lease_store
        .push_active_lease_outcome(ActiveLeaseOutcome::Found(row));
    // No remember_lease_export_key call → lease_export_key returns None.

    let resp = fx.post_secret(store_body("acme")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = body_json(resp).await;
    assert_error_code(&json, "no_active_lease");
}

// ---------------------------------------------------------------------------
// POST /secrets: validation errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn store_invalid_service_name_is_400() {
    let fx = SecretsWriteFixture::new("acme").await;
    fx.push_active_lease().await;
    let body = json!({
        "tenant": "acme",
        "service": "bad service!", // contains space + exclamation mark
        "name": "key",
        "kind": "api-key",
        "value_b64": STANDARD.encode(b"x"),
    });
    let resp = fx.post_secret(body).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_error_code(&json, "bad_request");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("invalid service"));
}

#[tokio::test]
async fn store_invalid_secret_name_is_400() {
    let fx = SecretsWriteFixture::new("acme").await;
    fx.push_active_lease().await;
    let body = json!({
        "tenant": "acme",
        "service": "svc",
        "name": "bad name!", // contains space + exclamation mark
        "kind": "api-key",
        "value_b64": STANDARD.encode(b"x"),
    });
    let resp = fx.post_secret(body).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_error_code(&json, "bad_request");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("invalid name"));
}

#[tokio::test]
async fn store_unknown_kind_is_400() {
    let fx = SecretsWriteFixture::new("acme").await;
    fx.push_active_lease().await;
    let body = json!({
        "tenant": "acme",
        "service": "svc",
        "name": "key",
        "kind": "not-a-real-kind",
        "value_b64": STANDARD.encode(b"x"),
    });
    let resp = fx.post_secret(body).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_error_code(&json, "bad_request");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("unknown kind"));
}

#[tokio::test]
async fn store_invalid_base64_is_400() {
    let fx = SecretsWriteFixture::new("acme").await;
    fx.push_active_lease().await;
    let body = json!({
        "tenant": "acme",
        "service": "svc",
        "name": "key",
        "kind": "api-key",
        "value_b64": "not!!valid==base64$$",
    });
    let resp = fx.post_secret(body).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_error_code(&json, "bad_request");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("invalid base64"));
}

// ---------------------------------------------------------------------------
// POST /secrets: conflict (already exists without overwrite)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn store_conflict_without_overwrite_is_409() {
    let fx = SecretsWriteFixture::new("acme").await;

    // First write: creates the secret.
    fx.push_active_lease().await;
    let resp = fx.post_secret(store_body("acme")).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Second write: same key, overwrite omitted (defaults to false) → 409.
    fx.push_active_lease().await;
    let resp = fx.post_secret(store_body("acme")).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let json = body_json(resp).await;
    assert_error_code(&json, "already_exists");
}

// ---------------------------------------------------------------------------
// POST /secrets: success — new secret
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct StoreResponse {
    stored: String,
    created: bool,
}

#[tokio::test]
async fn store_new_secret_returns_200_created_true() {
    let fx = SecretsWriteFixture::new("acme").await;
    fx.push_active_lease().await;

    let resp = fx.post_secret(store_body("acme")).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let sr: StoreResponse = serde_json::from_slice(&bytes).expect("StoreResponse JSON");
    assert_eq!(sr.stored, "mysvc/mykey");
    assert!(sr.created, "expected created: true for a new secret");
}

// ---------------------------------------------------------------------------
// POST /secrets: success — overwrite existing secret
// ---------------------------------------------------------------------------

#[tokio::test]
async fn store_overwrite_returns_200_created_false() {
    let fx = SecretsWriteFixture::new("acme").await;

    // First write: create.
    fx.push_active_lease().await;
    let resp = fx.post_secret(store_body("acme")).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Second write: overwrite.
    fx.push_active_lease().await;
    let mut body = store_body("acme");
    body["overwrite"] = json!(true);
    let resp = fx.post_secret(body).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let sr: StoreResponse = serde_json::from_slice(&bytes).expect("StoreResponse JSON");
    assert_eq!(sr.stored, "mysvc/mykey");
    assert!(!sr.created, "expected created: false for an overwrite");
}

// ---------------------------------------------------------------------------
// DELETE /secrets: resolve_active_lease errors (same DB surface)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_no_active_lease_is_503() {
    let fx = SecretsWriteFixture::new("acme").await;
    // No outcome pushed → Ok(None) from find_active_lease_for_tenant.
    let resp = fx.delete_secret("svc", "key", "acme").await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = body_json(resp).await;
    assert_error_code(&json, "no_active_lease");
}

#[tokio::test]
async fn delete_lease_db_error_is_500() {
    let fx = SecretsWriteFixture::new("acme").await;
    fx.push_lease_db_error("db down");
    let resp = fx.delete_secret("svc", "key", "acme").await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let json = body_json(resp).await;
    assert_error_code(&json, "internal_error");
}

// ---------------------------------------------------------------------------
// DELETE /secrets: validation errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_invalid_service_name_is_400() {
    let fx = SecretsWriteFixture::new("acme").await;
    fx.push_active_lease().await;
    // ".." is rejected by validate_service (reserved component) but is a valid URI path segment.
    let resp = fx.delete_secret("..", "key", "acme").await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_error_code(&json, "bad_request");
}

#[tokio::test]
async fn delete_invalid_secret_name_is_400() {
    let fx = SecretsWriteFixture::new("acme").await;
    fx.push_active_lease().await;
    // ".." is rejected by validate_name (reserved component) but is a valid URI path segment.
    let resp = fx.delete_secret("svc", "..", "acme").await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_error_code(&json, "bad_request");
}

// ---------------------------------------------------------------------------
// DELETE /secrets: secret not found
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_missing_secret_is_404() {
    let fx = SecretsWriteFixture::new("acme").await;
    fx.push_active_lease().await;
    let resp = fx.delete_secret("svc", "nonexistent", "acme").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let json = body_json(resp).await;
    assert_error_code(&json, "not_found");
}

// ---------------------------------------------------------------------------
// DELETE /secrets: success
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_existing_secret_returns_204() {
    let fx = SecretsWriteFixture::new("acme").await;

    // First, store a secret.
    fx.push_active_lease().await;
    let resp = fx.post_secret(store_body("acme")).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Then delete it.
    fx.push_active_lease().await;
    let resp = fx.delete_secret("mysvc", "mykey", "acme").await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------------
// Vault unlock failures (500) — P6 secrets.rs top-up
// ---------------------------------------------------------------------------

/// Remove the vault file before the request so that `vault.unlock_master`
/// returns `VaultError::NotInitialized`.  Both `store` and `delete_secret`
/// must map vault errors to 500 `internal_error`.
#[tokio::test]
async fn store_vault_unlock_fails_is_500() {
    let fx = SecretsWriteFixture::new("acme").await;

    // Delete vault.botwork so unlock_master fails.
    let vault_file = fx._dir.path().join("acme").join("vault.botwork");
    std::fs::remove_file(&vault_file).expect("remove vault.botwork");

    fx.push_active_lease().await;
    let resp = fx.post_secret(store_body("acme")).await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let json = body_json(resp).await;
    assert_error_code(&json, "internal_error");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("vault error"),
        "message should mention vault error, got: {}",
        json["error"]["message"]
    );
}

#[tokio::test]
async fn delete_vault_unlock_fails_is_500() {
    let fx = SecretsWriteFixture::new("acme").await;

    // Delete vault.botwork so unlock_master fails.
    let vault_file = fx._dir.path().join("acme").join("vault.botwork");
    std::fs::remove_file(&vault_file).expect("remove vault.botwork");

    fx.push_active_lease().await;
    let resp = fx.delete_secret("svc", "key", "acme").await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let json = body_json(resp).await;
    assert_error_code(&json, "internal_error");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("vault error"),
        "message should mention vault error, got: {}",
        json["error"]["message"]
    );
}

// ---------------------------------------------------------------------------
// Vault error arms after successful unlock — Conflict and generic Err
// ---------------------------------------------------------------------------

/// Spawn a background OS thread that:
///
/// 1. Opens `gen_path` (the vault generation-counter sidecar
///    `vault.botwork.gen`) and acquires an **exclusive** advisory flock.
/// 2. Sends `()` on the returned `Receiver` to signal that the flock is held.
/// 3. Sleeps 30 ms so the in-flight request's `unlock_master` can call
///    `peek_generation` (no flock needed there) and record `expected_gen`
///    from the current file content.
/// 4. Overwrites the gen file with `new_content` (still under flock).
/// 5. Releases the flock, unblocking `VaultLock::acquire` in `persist`.
///
/// Because the vault's `persist` calls `VaultLock::acquire` (exclusive
/// flock) *after* `unlock_master` records `expected_gen`, the exclusive
/// flock held here guarantees that `persist` blocks until step 5 and then
/// reads the corrupted content:
///
/// * `new_content = gen.to_le_bytes()` where `gen != expected_gen` →
///   `VaultError::Conflict`.
/// * `new_content = [1, 2, 3]` (3 bytes — neither 0 nor 8) →
///   `VaultError::Integrity` (truncated gen file), which is neither
///   `Conflict` nor `InvalidComponent` → the generic `Err(e)` arm.
///
/// The caller must:
/// 1. Call `rx.recv()` to wait for the lock to be acquired.
/// 2. Push the active lease and issue the request.
/// 3. Call `bg.join()` after the response to surface any bg-thread panic.
fn spawn_gen_file_corruptor(
    gen_path: std::path::PathBuf,
    new_content: Vec<u8>,
) -> (std::thread::JoinHandle<()>, std::sync::mpsc::Receiver<()>) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        use std::io::{Seek, SeekFrom, Write};

        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&gen_path)
            .unwrap_or_else(|e| panic!("corruptor: failed to open gen file {gen_path:?}: {e}"));
        // Acquire exclusive flock; blocks until no competing holder.
        f.lock()
            .unwrap_or_else(|e| panic!("corruptor: acquire exclusive flock on {gen_path:?}: {e}"));
        // Signal the main thread that the flock is held.
        tx.send(()).expect("corruptor: send lock-acquired signal");
        // Conservative sleep before writing.
        //
        // `peek_generation` (called inside `unlock_master`) reads the gen file
        // WITHOUT acquiring a flock — advisory locking does not prevent plain
        // reads.  It therefore completes in microseconds, long before this
        // sleep expires.  After `peek_generation` the handler proceeds
        // synchronously to `persist → VaultLock::acquire`, which blocks
        // because we hold the exclusive flock.  We write the corruption payload
        // *while the handler is already blocked*, then release; `VaultLock::acquire`
        // immediately unblocks and reads the new content.
        //
        // 100 ms is deliberately conservative: even on a heavily loaded CI
        // host, `unlock_master` (fast-KDF path) and the in-memory steps
        // between `peek_generation` and `VaultLock::acquire` complete in
        // well under 10 ms.  Because `VaultLock::acquire` is guaranteed to
        // block (the flock is still held when the handler reaches it), the
        // write always lands while the handler is blocked, making the outcome
        // deterministic.
        std::thread::sleep(std::time::Duration::from_millis(100));
        // Overwrite the gen file with the chosen content.
        f.set_len(new_content.len() as u64)
            .expect("corruptor: truncate gen file");
        f.seek(SeekFrom::Start(0))
            .expect("corruptor: seek gen file");
        f.write_all(&new_content)
            .expect("corruptor: write gen file");
        f.sync_all().expect("corruptor: sync gen file");
        // Release the flock — `VaultLock::acquire` immediately unblocks.
        f.unlock()
            .unwrap_or_else(|e| panic!("corruptor: release flock on {gen_path:?}: {e}"));
    });
    (handle, rx)
}

/// `POST /secrets`: vault write conflict → 503 `vault_conflict`.
///
/// The background thread holds an exclusive flock on `vault.botwork.gen`
/// before the request starts.  `unlock_master` calls `peek_generation`
/// without a flock (advisory locking; reads succeed) and records
/// `expected_gen = 0`.  When `put_secret → persist` calls
/// `VaultLock::acquire` it blocks on the exclusive flock.  The background
/// thread then writes `gen = 100` and releases, so `VaultLock::acquire`
/// reads `found = 100 ≠ expected = 0` and returns `VaultError::Conflict`.
#[tokio::test]
async fn store_put_secret_conflict_is_503() {
    let fx = SecretsWriteFixture::new("acme").await;
    let gen_path = fx._dir.path().join("acme").join("vault.botwork.gen");

    // gen=100 as 8-byte LE: expected=0, found=100 → Conflict.
    let (bg, rx) = spawn_gen_file_corruptor(gen_path, 100u64.to_le_bytes().to_vec());
    rx.recv().expect("bg corruptor should acquire flock");

    fx.push_active_lease().await;
    let resp = fx.post_secret(store_body("acme")).await;
    bg.join().expect("bg corruptor thread should not panic");

    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = body_json(resp).await;
    assert_error_code(&json, "vault_conflict");
}

/// `POST /secrets`: generic vault error from `put_secret` → 500 `internal_error`.
///
/// The background thread writes 3 bytes (neither 0 nor 8) to the gen file
/// while the request's `persist` is blocked on the exclusive flock.
/// `VaultLock::acquire → read_gen_from` returns `VaultError::Integrity`
/// (truncated gen file).  That is neither `Conflict` nor `InvalidComponent`
/// so the generic `Err(e)` arm fires → 500.
#[tokio::test]
async fn store_put_secret_generic_err_is_500() {
    let fx = SecretsWriteFixture::new("acme").await;
    let gen_path = fx._dir.path().join("acme").join("vault.botwork.gen");

    // 3 bytes → `VaultError::Integrity` (truncated gen file).
    let (bg, rx) = spawn_gen_file_corruptor(gen_path, vec![0x01, 0x02, 0x03]);
    rx.recv().expect("bg corruptor should acquire flock");

    fx.push_active_lease().await;
    let resp = fx.post_secret(store_body("acme")).await;
    bg.join().expect("bg corruptor thread should not panic");

    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let json = body_json(resp).await;
    assert_error_code(&json, "internal_error");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("vault error"),
        "message should mention vault error, got: {}",
        json["error"]["message"]
    );
}

/// `DELETE /secrets/:service/:name`: vault write conflict → 503 `vault_conflict`.
///
/// Same mechanism as `store_put_secret_conflict_is_503` but for the delete
/// path.  We create the secret first (gen 0 → 1); the bg thread then writes
/// `gen = 100` while `delete_secret → persist` is blocked on the flock, so
/// `VaultLock::acquire` sees `found = 100 ≠ expected = 1` → Conflict.
#[tokio::test]
async fn delete_secret_conflict_is_503() {
    let fx = SecretsWriteFixture::new("acme").await;

    // Pre-condition: create the secret so delete has something to remove
    // (and so the gen file advances to 1).
    fx.push_active_lease().await;
    let store_resp = fx.post_secret(store_body("acme")).await;
    assert_eq!(
        store_resp.status(),
        StatusCode::OK,
        "pre-condition: secret must be stored"
    );

    let gen_path = fx._dir.path().join("acme").join("vault.botwork.gen");

    // After one store gen=1 on disk; writing 100 guarantees mismatch.
    let (bg, rx) = spawn_gen_file_corruptor(gen_path, 100u64.to_le_bytes().to_vec());
    rx.recv().expect("bg corruptor should acquire flock");

    fx.push_active_lease().await;
    let resp = fx.delete_secret("mysvc", "mykey", "acme").await;
    bg.join().expect("bg corruptor thread should not panic");

    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = body_json(resp).await;
    assert_error_code(&json, "vault_conflict");
}

/// `DELETE /secrets/:service/:name`: generic vault error → 500 `internal_error`.
///
/// After one store (gen 0 → 1) the bg thread writes 3 invalid bytes to the
/// gen file while `delete_secret → persist` is blocked on the exclusive
/// flock.  `VaultLock::acquire → read_gen_from` returns
/// `VaultError::Integrity`, which is neither `Conflict` nor `SecretNotFound`
/// → the generic `Err(e)` arm fires → 500.
#[tokio::test]
async fn delete_secret_generic_err_is_500() {
    let fx = SecretsWriteFixture::new("acme").await;

    // Pre-condition: create the secret.
    fx.push_active_lease().await;
    let store_resp = fx.post_secret(store_body("acme")).await;
    assert_eq!(
        store_resp.status(),
        StatusCode::OK,
        "pre-condition: secret must be stored"
    );

    let gen_path = fx._dir.path().join("acme").join("vault.botwork.gen");

    let (bg, rx) = spawn_gen_file_corruptor(gen_path, vec![0x01, 0x02, 0x03]);
    rx.recv().expect("bg corruptor should acquire flock");

    fx.push_active_lease().await;
    let resp = fx.delete_secret("mysvc", "mykey", "acme").await;
    bg.join().expect("bg corruptor thread should not panic");

    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let json = body_json(resp).await;
    assert_error_code(&json, "internal_error");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("vault error"),
        "message should mention vault error, got: {}",
        json["error"]["message"]
    );
}
