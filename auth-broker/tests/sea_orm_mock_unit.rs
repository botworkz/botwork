//! `sea_orm_mock_unit` — offline unit tests for the `SeaOrm*Store`
//! implementations driven by `sea_orm::MockDatabase`.
//!
//! Gate: `required-features = ["test-support"]`
//!
//! These tests exercise the **control flow and error mapping** of the DB
//! layer (`auth/lease.rs`, `auth/opaque.rs`, and the thin delegation
//! wrappers in `store/sea_orm_impl.rs`) without a live Postgres connection
//! or a Docker daemon.  `MockDatabase::new(DatabaseBackend::Postgres)` with
//! `append_query_results` / `append_query_errors` / `append_exec_results` /
//! `append_exec_errors` replays canned rows and errors so every branch
//! (`Ok(Some)`, `Ok(None)`, `Err(Db(_))`, `Err(Conflict)`, etc.) is
//! reachable offline.
//!
//! ## What is NOT tested here
//!
//! SQL correctness (JOINs, unique-constraint enforcement, FK cascades,
//! transaction isolation) remains in the docker-gated integration tier
//! (`opaque_e2e`, `remote_secrets`).  `MockDatabase` is a fixture, not a
//! database.
//!
//! ## Relation to the hand-rolled mock stores
//!
//! The hand-rolled `MockLeaseStore` / `MockTenantStore` /
//! `MockPasswordFileStore` in `store/mock.rs` and the `seed_synthetic_lease`
//! harness in `tests/common/mod.rs` remain in place and unchanged.  This
//! file is purely additive: it adds coverage of the real `SeaOrm*Store` →
//! free-function path that the hand-rolled mocks bypass entirely.

use botwork_auth_broker::auth::lease::{Bearer, LeaseRow, ValidationError};
use botwork_auth_broker::auth::opaque::UpsertError;
use botwork_auth_broker::store::sea_orm_impl::{
    SeaOrmLeaseStore, SeaOrmPasswordFileStore, SeaOrmTenantStore,
};
use botwork_auth_broker::store::{LeaseStore, PasswordFileStore, TenantStore};
use botwork_auth_broker::wrap_session_key;
use botwork_entity::{lease, opaque_password_file, tenant};
use botwork_opaque_handshake::PasswordFile;
use chrono::{Duration, Utc};
use sea_orm::{DatabaseBackend, DbErr, MockDatabase, MockExecResult};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Generate a real `PasswordFile` via an in-process OPAQUE round-trip.
/// Build a real `PasswordFile` value by running a minimal offline OPAQUE
/// registration exchange with a randomly-generated client password.  The bytes
/// are then used both as the canned DB value and as the input to
/// `upsert_password_file`, ensuring `PasswordFile::from_bytes` succeeds on the
/// re-read path.
fn make_password_file() -> PasswordFile {
    let mut rng = rand::rng();
    let setup = botwork_opaque_handshake::ServerSetup::generate(&mut rng);
    // Use a freshly-generated random password for the fixture so static
    // analysis tools don't flag a hard-coded credential (this value is never
    // stored or transmitted anywhere outside the unit test).
    let mut pw = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rng, &mut pw);
    let cr = botwork_opaque_handshake::client::registration_start(&mut rng, &pw)
        .expect("client registration_start");
    let sr = botwork_opaque_handshake::server::registration_start(&setup, cr.request, b"tenant-id")
        .expect("server registration_start");
    let cf =
        botwork_opaque_handshake::client::registration_finish(&mut rng, cr.state, &pw, sr.response)
            .expect("client registration_finish");
    botwork_opaque_handshake::server::registration_finish(cf.upload)
}

fn now() -> chrono::DateTime<Utc> {
    Utc::now()
}

fn future() -> chrono::DateTime<Utc> {
    Utc::now() + Duration::days(7)
}

fn past() -> chrono::DateTime<Utc> {
    Utc::now() - Duration::hours(1)
}

/// Assert that a `DbErr` is the `Custom` variant (the shape most mock-injected
/// errors take).  Extracted to avoid repeating the same assertion across tests.
fn assert_custom_db_err(err: DbErr) {
    assert!(
        matches!(err, DbErr::Custom(_)),
        "expected DbErr::Custom, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// TenantStore: lookup_tenant_id_by_name
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tenant_lookup_by_name_hit() {
    let tenant_id = Uuid::new_v4();
    let n = now();
    let model = tenant::Model {
        id: tenant_id,
        name: "acme".to_string(),
        created_at: n,
        updated_at: n,
    };
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![model]])
        .into_connection();
    let store = SeaOrmTenantStore::new(db);

    let result = store.lookup_tenant_id_by_name("acme").await.unwrap();
    assert_eq!(result, Some(tenant_id));
}

#[tokio::test]
async fn tenant_lookup_by_name_miss() {
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![Vec::<tenant::Model>::new()])
        .into_connection();
    let store = SeaOrmTenantStore::new(db);

    let result = store.lookup_tenant_id_by_name("nobody").await.unwrap();
    assert_eq!(result, None);
}

#[tokio::test]
async fn tenant_lookup_by_name_db_error() {
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_errors(vec![DbErr::Custom("db boom".to_string())])
        .into_connection();
    let store = SeaOrmTenantStore::new(db);

    let err = store.lookup_tenant_id_by_name("x").await.unwrap_err();
    assert_custom_db_err(err);
}

// ---------------------------------------------------------------------------
// TenantStore: lookup_tenant_name_by_id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tenant_lookup_by_id_hit() {
    let tenant_id = Uuid::new_v4();
    let n = now();
    let model = tenant::Model {
        id: tenant_id,
        name: "acme".to_string(),
        created_at: n,
        updated_at: n,
    };
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![model]])
        .into_connection();
    let store = SeaOrmTenantStore::new(db);

    let result = store.lookup_tenant_name_by_id(tenant_id).await.unwrap();
    assert_eq!(result.as_deref(), Some("acme"));
}

#[tokio::test]
async fn tenant_lookup_by_id_miss() {
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![Vec::<tenant::Model>::new()])
        .into_connection();
    let store = SeaOrmTenantStore::new(db);

    let result = store
        .lookup_tenant_name_by_id(Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(result, None);
}

#[tokio::test]
async fn tenant_lookup_by_id_db_error() {
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_errors(vec![DbErr::Custom("connection reset".to_string())])
        .into_connection();
    let store = SeaOrmTenantStore::new(db);

    let err = store
        .lookup_tenant_name_by_id(Uuid::new_v4())
        .await
        .unwrap_err();
    assert_custom_db_err(err);
}

// ---------------------------------------------------------------------------
// PasswordFileStore: load_password_file
// ---------------------------------------------------------------------------

#[tokio::test]
async fn load_password_file_hit() {
    let tenant_id = Uuid::new_v4();
    let pf = make_password_file();
    let n = now();
    let model = opaque_password_file::Model {
        id: Uuid::new_v4(),
        tenant_id,
        password_file: pf.as_bytes().to_vec(),
        suite_version: 1,
        created_at: n,
        updated_at: n,
    };
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![model]])
        .into_connection();
    let store = SeaOrmPasswordFileStore::new(db);

    let result = store.load_password_file(tenant_id).await.unwrap();
    assert!(result.is_some());
    assert_eq!(result.unwrap().as_bytes(), pf.as_bytes());
}

#[tokio::test]
async fn load_password_file_miss() {
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![Vec::<opaque_password_file::Model>::new()])
        .into_connection();
    let store = SeaOrmPasswordFileStore::new(db);

    let result = store.load_password_file(Uuid::new_v4()).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn load_password_file_db_error() {
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_errors(vec![DbErr::Custom("query failed".to_string())])
        .into_connection();
    let store = SeaOrmPasswordFileStore::new(db);

    let err = store.load_password_file(Uuid::new_v4()).await.unwrap_err();
    assert_custom_db_err(err);
}

// ---------------------------------------------------------------------------
// PasswordFileStore: upsert_password_file
// ---------------------------------------------------------------------------

/// Fresh insert: the INSERT exec succeeds with rows_affected = 1 → `Ok(())`.
#[tokio::test]
async fn upsert_password_file_fresh_insert() {
    let tenant_id = Uuid::new_v4();
    let pf = make_password_file();
    // INSERT ... ON CONFLICT DO NOTHING uses exec path.
    // rows_affected = 1 means the row was inserted.
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_exec_results(vec![MockExecResult {
            last_insert_id: 0,
            rows_affected: 1,
        }])
        .into_connection();
    let store = SeaOrmPasswordFileStore::new(db);

    store
        .upsert_password_file(tenant_id, &pf, 1)
        .await
        .expect("fresh upsert should succeed");
}

/// Conflict + idempotent: INSERT returns RecordNotInserted (rows_affected = 0),
/// the re-read SELECT returns the same bytes → `Ok(())`.
#[tokio::test]
async fn upsert_password_file_idempotent_conflict() {
    let tenant_id = Uuid::new_v4();
    let pf = make_password_file();
    let n = now();
    // Same bytes stored — idempotent re-register.
    let stored_model = opaque_password_file::Model {
        id: Uuid::new_v4(),
        tenant_id,
        password_file: pf.as_bytes().to_vec(),
        suite_version: 1,
        created_at: n,
        updated_at: n,
    };
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        // rows_affected = 0 → sea-orm raises RecordNotInserted internally
        .append_exec_results(vec![MockExecResult {
            last_insert_id: 0,
            rows_affected: 0,
        }])
        // re-read SELECT returns the stored row (same bytes)
        .append_query_results(vec![vec![stored_model]])
        .into_connection();
    let store = SeaOrmPasswordFileStore::new(db);

    store
        .upsert_password_file(tenant_id, &pf, 1)
        .await
        .expect("idempotent re-register should be Ok");
}

/// Conflict + different bytes: INSERT returns RecordNotInserted, the re-read
/// SELECT returns different bytes → `Err(UpsertError::Conflict)`.
#[tokio::test]
async fn upsert_password_file_conflict_different_bytes() {
    let tenant_id = Uuid::new_v4();
    let pf_new = make_password_file();
    let pf_old = make_password_file(); // different bytes
    let n = now();
    let stored_model = opaque_password_file::Model {
        id: Uuid::new_v4(),
        tenant_id,
        password_file: pf_old.as_bytes().to_vec(),
        suite_version: 1,
        created_at: n,
        updated_at: n,
    };
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_exec_results(vec![MockExecResult {
            last_insert_id: 0,
            rows_affected: 0,
        }])
        .append_query_results(vec![vec![stored_model]])
        .into_connection();
    let store = SeaOrmPasswordFileStore::new(db);

    let err = store
        .upsert_password_file(tenant_id, &pf_new, 1)
        .await
        .unwrap_err();
    assert!(
        matches!(err, UpsertError::Conflict),
        "expected Conflict, got {err:?}"
    );
}

/// Real DB error on the INSERT exec → `Err(UpsertError::Db(_))`.
#[tokio::test]
async fn upsert_password_file_db_error() {
    let pf = make_password_file();
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_exec_errors(vec![DbErr::Custom("write failed".to_string())])
        .into_connection();
    let store = SeaOrmPasswordFileStore::new(db);

    let err = store
        .upsert_password_file(Uuid::new_v4(), &pf, 1)
        .await
        .unwrap_err();
    assert!(
        matches!(err, UpsertError::Db(_)),
        "expected Db(_), got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// LeaseStore: insert_lease
// ---------------------------------------------------------------------------

/// `model.insert()` for Postgres uses INSERT…RETURNING * (query path) so we
/// queue the expected returned row in `append_query_results`.
#[tokio::test]
async fn insert_lease_success() {
    let tenant_id = Uuid::new_v4();
    let n = now();
    let exp = future();
    let expected_model = lease::Model {
        id: Uuid::new_v4(),
        tenant_id,
        bearer_hash: vec![0u8; 32],
        wrapped_export_key: vec![0u8; 64],
        issued_at: n,
        expires_at: exp,
        idle_extends_to: exp,
        revoked_at: None,
    };
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![expected_model.clone()]])
        .into_connection();
    let store = SeaOrmLeaseStore::new(db);

    use botwork_auth_broker::auth::lease::WrappedExportKey;
    let bearer = Bearer::generate();
    let hash = bearer.hash();
    let wek = WrappedExportKey(vec![0u8; 64]);

    let row: LeaseRow = store
        .insert_lease(tenant_id, &hash, &wek, 3600, n)
        .await
        .expect("insert_lease should succeed");

    assert_eq!(row.tenant_id, tenant_id);
    assert_eq!(row.id, expected_model.id);
    assert!(row.expires_at > n);
}

/// DB error during INSERT → `Err(DbErr::Custom(_))`.
#[tokio::test]
async fn insert_lease_db_error() {
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_errors(vec![DbErr::Custom("disk full".to_string())])
        .into_connection();
    let store = SeaOrmLeaseStore::new(db);

    use botwork_auth_broker::auth::lease::WrappedExportKey;
    let bearer = Bearer::generate();
    let hash = bearer.hash();
    let wek = WrappedExportKey(vec![0u8; 64]);

    let err = store
        .insert_lease(Uuid::new_v4(), &hash, &wek, 3600, now())
        .await
        .unwrap_err();
    assert_custom_db_err(err);
}

// ---------------------------------------------------------------------------
// LeaseStore: validate_and_extend
// ---------------------------------------------------------------------------
//
// `validate_and_extend` wraps the inner function in a transaction. Both the
// SELECT (find by bearer_hash) and the UPDATE RETURNING (idle extension) go
// through the query path under Postgres, so they both consume from
// `append_query_results`.  The exec path is NOT involved here.

fn live_lease_model(tenant_id: Uuid, bearer_hash_bytes: Vec<u8>, wek: Vec<u8>) -> lease::Model {
    let n = now();
    lease::Model {
        id: Uuid::new_v4(),
        tenant_id,
        bearer_hash: bearer_hash_bytes,
        wrapped_export_key: wek,
        issued_at: n,
        expires_at: future(),
        idle_extends_to: future(),
        revoked_at: None,
    }
}

/// DB error on the initial SELECT → `Err(ValidationError::Db(_))`.
#[tokio::test]
async fn validate_and_extend_db_error() {
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_errors(vec![DbErr::Custom("db down".to_string())])
        .into_connection();
    let store = SeaOrmLeaseStore::new(db);

    let result = store.validate_and_extend(&Bearer::generate(), now()).await;
    assert!(
        matches!(result, Err(ValidationError::Db(_))),
        "expected Db(_)"
    );
}

/// No row found for bearer_hash → `Ok(None)`.
#[tokio::test]
async fn validate_and_extend_miss() {
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![Vec::<lease::Model>::new()])
        .into_connection();
    let store = SeaOrmLeaseStore::new(db);

    let result = store
        .validate_and_extend(&Bearer::generate(), now())
        .await
        .unwrap();
    assert!(result.is_none(), "expected None for miss");
}

/// Row found with `revoked_at` set → `Err(ValidationError::Revoked)`.
#[tokio::test]
async fn validate_and_extend_revoked() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let n = now();
    let mut model = live_lease_model(tenant_id, bearer.hash().to_vec(), vec![0u8; 64]);
    model.revoked_at = Some(n);

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![model]])
        .into_connection();
    let store = SeaOrmLeaseStore::new(db);

    let result = store.validate_and_extend(&bearer, n).await;
    assert!(
        matches!(result, Err(ValidationError::Revoked)),
        "expected Revoked"
    );
}

/// Row found but `expires_at` is in the past → `Err(ValidationError::Expired)`.
#[tokio::test]
async fn validate_and_extend_expired_by_expires_at() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let n = now();
    let mut model = live_lease_model(tenant_id, bearer.hash().to_vec(), vec![0u8; 64]);
    model.expires_at = past(); // in the past
    model.idle_extends_to = future();

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![model]])
        .into_connection();
    let store = SeaOrmLeaseStore::new(db);

    let result = store.validate_and_extend(&bearer, n).await;
    assert!(
        matches!(result, Err(ValidationError::Expired)),
        "expected Expired (expires_at)"
    );
}

/// Row found but `idle_extends_to` is in the past → `Err(ValidationError::Expired)`.
#[tokio::test]
async fn validate_and_extend_expired_by_idle() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let n = now();
    let mut model = live_lease_model(tenant_id, bearer.hash().to_vec(), vec![0u8; 64]);
    model.expires_at = future();
    model.idle_extends_to = past(); // in the past

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![model]])
        .into_connection();
    let store = SeaOrmLeaseStore::new(db);

    let result = store.validate_and_extend(&bearer, n).await;
    assert!(
        matches!(result, Err(ValidationError::Expired)),
        "expected Expired (idle)"
    );
}

/// Row found and live, but `wrapped_export_key` is garbage (AEAD decrypt
/// will fail) → `Ok(None)` (same as InvalidBearer).
#[tokio::test]
async fn validate_and_extend_bad_wrapped_key() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let n = now();
    // A well-formed but incorrect wrapped key (right length, wrong bytes) so
    // the TooShort guard passes but the AEAD tag check fails.
    let garbage_wek = vec![0u8; 64];
    let model = live_lease_model(tenant_id, bearer.hash().to_vec(), garbage_wek);

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![model]])
        .into_connection();
    let store = SeaOrmLeaseStore::new(db);

    let result = store.validate_and_extend(&bearer, n).await.unwrap();
    assert!(
        result.is_none(),
        "bad wrapped_export_key should return Ok(None)"
    );
}

/// Full success path: live row with a properly wrapped export key → the key
/// is unwrapped and `Ok(Some(ValidatedLease))` is returned.
///
/// The UPDATE RETURNING also goes through the query path (Postgres), so we
/// queue two results: the SELECT and the UPDATE-RETURNING.
#[tokio::test]
async fn validate_and_extend_success() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let session_key = vec![0xABu8; 64];
    // Build a correctly-wrapped export key for this bearer.
    let wek_bytes = wrap_session_key(bearer.as_bytes(), &session_key);

    let n = now();
    let exp = future();
    let model = lease::Model {
        id: Uuid::new_v4(),
        tenant_id,
        bearer_hash: bearer.hash().to_vec(),
        wrapped_export_key: wek_bytes.clone(),
        issued_at: n,
        expires_at: exp,
        idle_extends_to: exp,
        revoked_at: None,
    };
    // For the UPDATE RETURNING we return the same model (mock doesn't care
    // about SQL semantics; we only need a non-empty result so the update
    // path doesn't surface RecordNotUpdated).
    let updated_model = lease::Model {
        idle_extends_to: n + Duration::hours(1),
        ..model.clone()
    };

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        // query #0: SELECT by bearer_hash
        // query #1: UPDATE … RETURNING * (idle extension)
        .append_query_results(vec![vec![model.clone()], vec![updated_model]])
        .into_connection();
    let store = SeaOrmLeaseStore::new(db);

    let result = store
        .validate_and_extend(&bearer, n)
        .await
        .expect("validate_and_extend should succeed");
    let validated = result.expect("should be Some");

    assert_eq!(validated.lease.tenant_id, tenant_id);
    assert_eq!(validated.lease.id, model.id);
    // The unwrapped key must match the original session key.
    assert_eq!(validated.export_key.as_slice(), session_key.as_slice());
}

// ---------------------------------------------------------------------------
// LeaseStore: revoke
// ---------------------------------------------------------------------------

/// `update_many().exec()` uses the exec path (no RETURNING).  Rows affected
/// is the return value.
#[tokio::test]
async fn revoke_success() {
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_exec_results(vec![MockExecResult {
            last_insert_id: 0,
            rows_affected: 1,
        }])
        .into_connection();
    let store = SeaOrmLeaseStore::new(db);

    use botwork_auth_broker::auth::lease::BearerHash;
    let hash = BearerHash::from_bearer_bytes(b"some-bearer");
    let affected = store.revoke(&hash, now()).await.expect("revoke ok");
    assert_eq!(affected, 1);
}

/// DB error on UPDATE → `Err(DbErr::Custom(_))`.
#[tokio::test]
async fn revoke_db_error() {
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_exec_errors(vec![DbErr::Custom("connection lost".to_string())])
        .into_connection();
    let store = SeaOrmLeaseStore::new(db);

    use botwork_auth_broker::auth::lease::BearerHash;
    let hash = BearerHash::from_bearer_bytes(b"x");
    let err = store.revoke(&hash, now()).await.unwrap_err();
    assert_custom_db_err(err);
}

// ---------------------------------------------------------------------------
// LeaseStore: revoke_by_id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn revoke_by_id_success() {
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_exec_results(vec![MockExecResult {
            last_insert_id: 0,
            rows_affected: 1,
        }])
        .into_connection();
    let store = SeaOrmLeaseStore::new(db);

    let affected = store
        .revoke_by_id(Uuid::new_v4(), now())
        .await
        .expect("revoke_by_id ok");
    assert_eq!(affected, 1);
}

#[tokio::test]
async fn revoke_by_id_db_error() {
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_exec_errors(vec![DbErr::Custom("lock timeout".to_string())])
        .into_connection();
    let store = SeaOrmLeaseStore::new(db);

    let err = store.revoke_by_id(Uuid::new_v4(), now()).await.unwrap_err();
    assert_custom_db_err(err);
}

// ---------------------------------------------------------------------------
// insert_lease_ignoring_conflict (free function, via exec path)
// ---------------------------------------------------------------------------
//
// `INSERT … ON CONFLICT DO NOTHING` with a set primary key goes through
// the exec path.  rows_affected = 1 → Ok(()), rows_affected = 0 →
// RecordNotInserted propagates as Err (the "DO NOTHING" is at the SQL
// level; the caller decides whether to ignore RecordNotInserted).

#[tokio::test]
async fn insert_lease_ignoring_conflict_success() {
    let tenant_id = Uuid::new_v4();
    let n = now();
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_exec_results(vec![MockExecResult {
            last_insert_id: 0,
            rows_affected: 1,
        }])
        .into_connection();

    use botwork_auth_broker::auth::lease::{
        insert_lease_ignoring_conflict, BearerHash, WrappedExportKey,
    };
    let bearer = Bearer::generate();
    let hash = BearerHash::from_bearer_bytes(bearer.as_bytes());
    let wek = WrappedExportKey(vec![0u8; 64]);

    insert_lease_ignoring_conflict(&db, tenant_id, &hash, &wek, 3600, n)
        .await
        .expect("fresh insert_ignoring_conflict should succeed");
}

/// rows_affected = 0 causes sea-orm to surface RecordNotInserted.
/// The function propagates this (the caller handles the conflict-ignored path).
#[tokio::test]
async fn insert_lease_ignoring_conflict_conflict_path() {
    let tenant_id = Uuid::new_v4();
    let n = now();
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_exec_results(vec![MockExecResult {
            last_insert_id: 0,
            rows_affected: 0,
        }])
        .into_connection();

    use botwork_auth_broker::auth::lease::{
        insert_lease_ignoring_conflict, BearerHash, WrappedExportKey,
    };
    let bearer = Bearer::generate();
    let hash = BearerHash::from_bearer_bytes(bearer.as_bytes());
    let wek = WrappedExportKey(vec![0u8; 64]);

    let err = insert_lease_ignoring_conflict(&db, tenant_id, &hash, &wek, 3600, n)
        .await
        .unwrap_err();
    assert!(
        matches!(err, DbErr::RecordNotInserted),
        "conflict path should surface RecordNotInserted, got {err:?}"
    );
}
