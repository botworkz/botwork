//! `lease_store_unit` — unit tests for the store traits and mock
//! implementations.
//!
//! Tests all branches for:
//! - [`LeaseStore::validate_and_extend`]: valid, expired, revoked, miss, db-error
//! - [`LeaseStore::insert_lease`]: records the call, returns a `LeaseRow`
//! - [`LeaseStore::revoke`]: records the bearer hash
//! - [`TenantStore`]: lookup by name and by id, miss, db-error
//! - [`PasswordFileStore`]: load, upsert (fresh, idempotent, conflict, db-error)
//!
//! No Docker, no Postgres — all assertions use in-memory state.

use std::sync::Arc;

use botwork_auth_broker::auth::lease::{
    Bearer, BearerHash, LeaseRow, ValidationError, WrappedExportKey,
};
use botwork_auth_broker::auth::opaque::UpsertError;
use botwork_auth_broker::store::mock::{
    LeaseOutcome, MockLeaseStore, MockPasswordFileStore, MockTenantStore,
};
use botwork_auth_broker::store::{LeaseStore, PasswordFileStore, TenantStore};
use botwork_opaque_handshake::PasswordFile;
use chrono::Utc;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fake_bearer() -> Bearer {
    Bearer::generate()
}

fn fake_bearer_hash() -> BearerHash {
    let b = fake_bearer();
    b.hash()
}

fn fake_wrapped_export_key() -> WrappedExportKey {
    WrappedExportKey(vec![0u8; 64])
}

fn lease_row_for(tenant_id: Uuid) -> LeaseRow {
    let now = Utc::now();
    let expires = now + chrono::Duration::days(7);
    LeaseRow {
        id: Uuid::new_v4(),
        tenant_id,
        issued_at: now,
        expires_at: expires,
        idle_extends_to: expires,
        revoked_at: None,
    }
}

// ---------------------------------------------------------------------------
// LeaseStore: validate_and_extend
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lease_store_validate_valid_returns_some() {
    let store = MockLeaseStore::new();
    let tenant_id = Uuid::new_v4();
    let row = lease_row_for(tenant_id);
    store.push_outcome(LeaseOutcome::Valid(row.clone()));

    let result = store.validate_and_extend(&fake_bearer(), Utc::now()).await;
    let validated = result.expect("should be Ok").expect("should be Some");
    assert_eq!(validated.lease.tenant_id, tenant_id);
    assert_eq!(validated.lease.id, row.id);
    // export_key is the synthetic 0x42 fill the mock uses
    assert_eq!(validated.export_key.len(), 32);
    assert!(validated.export_key.iter().all(|&b| b == 0x42));
}

#[tokio::test]
async fn lease_store_validate_miss_returns_none() {
    let store = MockLeaseStore::new();
    store.push_outcome(LeaseOutcome::Miss);

    let result = store.validate_and_extend(&fake_bearer(), Utc::now()).await;
    assert!(matches!(result, Ok(None)));
}

#[tokio::test]
async fn lease_store_validate_expired_returns_err() {
    let store = MockLeaseStore::new();
    store.push_outcome(LeaseOutcome::Expired);

    let result = store.validate_and_extend(&fake_bearer(), Utc::now()).await;
    assert!(matches!(result, Err(ValidationError::Expired)));
}

#[tokio::test]
async fn lease_store_validate_revoked_returns_err() {
    let store = MockLeaseStore::new();
    store.push_outcome(LeaseOutcome::Revoked);

    let result = store.validate_and_extend(&fake_bearer(), Utc::now()).await;
    assert!(matches!(result, Err(ValidationError::Revoked)));
}

#[tokio::test]
async fn lease_store_validate_db_error_returns_err() {
    let store = MockLeaseStore::new();
    store.push_outcome(LeaseOutcome::DbError("boom".to_string()));

    let result = store.validate_and_extend(&fake_bearer(), Utc::now()).await;
    assert!(matches!(result, Err(ValidationError::Db(_))));
}

// ---------------------------------------------------------------------------
// LeaseStore: insert_lease
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lease_store_insert_records_call_and_returns_row() {
    let store = MockLeaseStore::new();
    let tenant_id = Uuid::new_v4();
    let hash = fake_bearer_hash();
    let wek = fake_wrapped_export_key();
    let now = Utc::now();

    let row = store
        .insert_lease(tenant_id, &hash, &wek, 3600, now)
        .await
        .expect("insert should succeed");

    assert_eq!(row.tenant_id, tenant_id);
    assert!(row.expires_at > now);

    let inserts = store.drain_inserts();
    assert_eq!(inserts.len(), 1);
    assert_eq!(inserts[0].0, tenant_id);
    assert_eq!(inserts[0].1, hash.to_vec());
}

// ---------------------------------------------------------------------------
// LeaseStore: revoke
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lease_store_revoke_records_bearer_hash() {
    let store = MockLeaseStore::new();
    let hash = fake_bearer_hash();

    let affected = store.revoke(&hash, Utc::now()).await.expect("revoke ok");
    assert_eq!(affected, 1);

    let revokes = store.drain_revokes();
    assert_eq!(revokes.len(), 1);
    assert_eq!(revokes[0], hash.to_vec());
}

// ---------------------------------------------------------------------------
// TenantStore
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tenant_store_lookup_by_name_hit() {
    let tenant_id = Uuid::new_v4();
    let store = MockTenantStore::with_tenant("acme", tenant_id);

    let result = store.lookup_tenant_id_by_name("acme").await.unwrap();
    assert_eq!(result, Some(tenant_id));
}

#[tokio::test]
async fn tenant_store_lookup_by_name_miss() {
    let store = MockTenantStore::new();

    let result = store.lookup_tenant_id_by_name("nope").await.unwrap();
    assert_eq!(result, None);
}

#[tokio::test]
async fn tenant_store_lookup_by_id_hit() {
    let tenant_id = Uuid::new_v4();
    let store = MockTenantStore::with_tenant("acme", tenant_id);

    let result = store.lookup_tenant_name_by_id(tenant_id).await.unwrap();
    assert_eq!(result.as_deref(), Some("acme"));
}

#[tokio::test]
async fn tenant_store_lookup_by_id_miss() {
    let store = MockTenantStore::new();

    let result = store
        .lookup_tenant_name_by_id(Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(result, None);
}

#[tokio::test]
async fn tenant_store_db_error() {
    let store = MockTenantStore::always_error("db down");

    let err = store.lookup_tenant_id_by_name("x").await.unwrap_err();
    assert!(err.to_string().contains("db down"));
}

// ---------------------------------------------------------------------------
// PasswordFileStore
// ---------------------------------------------------------------------------

fn make_password_file() -> PasswordFile {
    // Generate a real OPAQUE PasswordFile via a full registration round-trip.
    let setup = botwork_opaque_handshake::ServerSetup::generate(&mut rand::rng());
    let mut rng = rand::rng();
    let cr = botwork_opaque_handshake::client::registration_start(&mut rng, b"password")
        .expect("client registration_start");
    let sr = botwork_opaque_handshake::server::registration_start(&setup, cr.request, b"id")
        .expect("server registration_start");
    let cf = botwork_opaque_handshake::client::registration_finish(
        &mut rng,
        cr.state,
        b"password",
        sr.response,
    )
    .expect("client registration_finish");
    botwork_opaque_handshake::server::registration_finish(cf.upload)
}

#[tokio::test]
async fn password_file_store_load_miss() {
    let store = MockPasswordFileStore::new();
    let result = store.load_password_file(Uuid::new_v4()).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn password_file_store_load_hit() {
    let tenant_id = Uuid::new_v4();
    let pf = make_password_file();
    let store = MockPasswordFileStore::with_file(tenant_id, &pf);

    let result = store.load_password_file(tenant_id).await.unwrap();
    assert!(result.is_some());
    // Round-trip: bytes should be identical.
    assert_eq!(result.unwrap().as_bytes(), pf.as_bytes());
}

#[tokio::test]
async fn password_file_store_upsert_fresh() {
    let store = MockPasswordFileStore::new();
    let tenant_id = Uuid::new_v4();
    let pf = make_password_file();

    store
        .upsert_password_file(tenant_id, &pf, 1)
        .await
        .expect("fresh insert should succeed");

    // Immediately loadable.
    let loaded = store.load_password_file(tenant_id).await.unwrap().unwrap();
    assert_eq!(loaded.as_bytes(), pf.as_bytes());
}

#[tokio::test]
async fn password_file_store_upsert_idempotent() {
    let store = MockPasswordFileStore::new();
    let tenant_id = Uuid::new_v4();
    let pf = make_password_file();

    store.upsert_password_file(tenant_id, &pf, 1).await.unwrap();
    // Same bytes — idempotent Ok(()).
    store
        .upsert_password_file(tenant_id, &pf, 1)
        .await
        .expect("second identical upsert should be idempotent");
}

#[tokio::test]
async fn password_file_store_upsert_conflict() {
    let store = MockPasswordFileStore::new();
    let tenant_id = Uuid::new_v4();
    let pf1 = make_password_file();
    let pf2 = make_password_file();

    store
        .upsert_password_file(tenant_id, &pf1, 1)
        .await
        .unwrap();
    // Different bytes — conflict.
    let err = store
        .upsert_password_file(tenant_id, &pf2, 1)
        .await
        .unwrap_err();
    assert!(matches!(err, UpsertError::Conflict));
}

#[tokio::test]
async fn password_file_store_db_error() {
    let store = MockPasswordFileStore::always_error("db down");
    let pf = make_password_file();

    let err = store
        .upsert_password_file(Uuid::new_v4(), &pf, 1)
        .await
        .unwrap_err();
    assert!(matches!(err, UpsertError::Db(_)));
}

// ---------------------------------------------------------------------------
// Arc<dyn Trait> round-trip — ensure the traits are object-safe and
// usable through Arc.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn traits_are_object_safe_through_arc() {
    let _: Arc<dyn LeaseStore + Send + Sync> = Arc::new(MockLeaseStore::new());
    let _: Arc<dyn TenantStore + Send + Sync> = Arc::new(MockTenantStore::new());
    let _: Arc<dyn PasswordFileStore + Send + Sync> = Arc::new(MockPasswordFileStore::new());
}
