//! In-memory mock implementations of the broker store traits.
//!
//! Gate: `cfg(any(test, feature = "test-support"))`
//!
//! These stubs let unit tests exercise every code path in the HTTP
//! handlers and validation logic without a Docker daemon or a live
//! Postgres connection.
//!
//! ## Usage
//!
//! ```rust,ignore
//! use botwork_auth_broker::store::mock::{
//!     MockLeaseStore, MockPasswordFileStore, MockTenantStore,
//! };
//! use botwork_auth_broker::auth::AuthState;
//!
//! let tenant_store = Arc::new(MockTenantStore::with_tenant("acme", tenant_id));
//! let pf_store = Arc::new(MockPasswordFileStore::new());
//! let lease_store = Arc::new(MockLeaseStore::new());
//! let state = AuthState::from_stores(lease_store, tenant_store, pf_store, setup);
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use botwork_opaque_handshake::PasswordFile;
use chrono::{DateTime, Utc};
use sea_orm::DbErr;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::auth::lease::{
    Bearer, BearerHash, LeaseRow, ValidatedLease, ValidationError, WrappedExportKey,
};
use crate::auth::opaque::UpsertError;
use crate::store::{LeaseStore, PasswordFileStore, TenantStore};

// ---------------------------------------------------------------------------
// LeaseStore mock
// ---------------------------------------------------------------------------

/// Canned outcome for a single `find_active_lease_for_tenant` call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActiveLeaseOutcome {
    /// Return `Ok(Some(lease_row))`.
    Found(LeaseRow),
    /// Return `Ok(None)`.
    NotFound,
    /// Return `Err(DbErr::Custom(msg))`.
    DbError(String),
}

/// Canned outcome for a single `validate_and_extend` call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseOutcome {
    /// Return `Ok(Some(ValidatedLease))` with the given `LeaseRow` and
    /// a synthetic non-empty export key.
    Valid(LeaseRow),
    /// Return `Ok(None)`.
    Miss,
    /// Return `Err(ValidationError::Expired)`.
    Expired,
    /// Return `Err(ValidationError::Revoked)`.
    Revoked,
    /// Return `Err(ValidationError::Db(DbErr::Custom(msg)))`.
    DbError(String),
}

/// Simple round-robin mock: each `validate_and_extend` call pops the
/// next outcome from the front of `outcomes`. Panics (test failure)
/// if the queue is empty.
///
/// `insert_lease` records the call in `inserts` and returns a
/// synthetic `LeaseRow`. `revoke` records the bearer hash in `revokes`.
/// `revoke_by_id` records the lease UUID in `revokes_by_id`.
pub struct MockLeaseStore {
    inner: Mutex<MockLeaseStoreInner>,
}

struct MockLeaseStoreInner {
    outcomes: std::collections::VecDeque<LeaseOutcome>,
    active_lease_outcomes: std::collections::VecDeque<ActiveLeaseOutcome>,
    inserts: Vec<(Uuid, Vec<u8>)>,
    revokes: Vec<Vec<u8>>,
    revokes_by_id: Vec<Uuid>,
    revoke_by_id_errors: std::collections::VecDeque<String>,
}

impl MockLeaseStore {
    /// Create an empty store; push outcomes with [`push_outcome`][Self::push_outcome].
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MockLeaseStoreInner {
                outcomes: std::collections::VecDeque::new(),
                active_lease_outcomes: std::collections::VecDeque::new(),
                inserts: Vec::new(),
                revokes: Vec::new(),
                revokes_by_id: Vec::new(),
                revoke_by_id_errors: std::collections::VecDeque::new(),
            }),
        }
    }

    /// Enqueue one outcome to be returned by the next
    /// `validate_and_extend` call.
    pub fn push_outcome(&self, outcome: LeaseOutcome) {
        self.inner.lock().unwrap().outcomes.push_back(outcome);
    }

    /// Enqueue one outcome to be returned by the next
    /// `find_active_lease_for_tenant` call. When the queue is empty the
    /// implementation falls back to `Ok(None)`, preserving the existing
    /// default behaviour for tests that don't exercise this path.
    pub fn push_active_lease_outcome(&self, outcome: ActiveLeaseOutcome) {
        self.inner
            .lock()
            .unwrap()
            .active_lease_outcomes
            .push_back(outcome);
    }

    /// Drain the recorded `insert_lease` calls as `(tenant_id,
    /// bearer_hash_bytes)` pairs.
    pub fn drain_inserts(&self) -> Vec<(Uuid, Vec<u8>)> {
        std::mem::take(&mut self.inner.lock().unwrap().inserts)
    }

    /// Drain the recorded `revoke` calls as bearer-hash-bytes lists.
    pub fn drain_revokes(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.inner.lock().unwrap().revokes)
    }

    /// Drain the recorded `revoke_by_id` calls as lease UUID lists.
    pub fn drain_revokes_by_id(&self) -> Vec<Uuid> {
        std::mem::take(&mut self.inner.lock().unwrap().revokes_by_id)
    }

    /// Queue a DB error to be returned by the next `revoke_by_id` call
    /// instead of the default `Ok(1)`.
    pub fn push_revoke_by_id_error(&self, msg: impl Into<String>) {
        self.inner
            .lock()
            .unwrap()
            .revoke_by_id_errors
            .push_back(msg.into());
    }
}

impl Default for MockLeaseStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LeaseStore for MockLeaseStore {
    async fn validate_and_extend(
        &self,
        _bearer: &Bearer,
        _now: DateTime<Utc>,
    ) -> Result<Option<ValidatedLease>, ValidationError> {
        let outcome = self
            .inner
            .lock()
            .unwrap()
            .outcomes
            .pop_front()
            .expect("MockLeaseStore: validate_and_extend called but outcome queue is empty");

        match outcome {
            LeaseOutcome::Valid(row) => {
                let export_key = Zeroizing::new(vec![0x42u8; 32]);
                Ok(Some(ValidatedLease {
                    lease: row,
                    export_key,
                }))
            }
            LeaseOutcome::Miss => Ok(None),
            LeaseOutcome::Expired => Err(ValidationError::Expired),
            LeaseOutcome::Revoked => Err(ValidationError::Revoked),
            LeaseOutcome::DbError(msg) => Err(ValidationError::Db(DbErr::Custom(msg))),
        }
    }

    async fn insert_lease(
        &self,
        tenant_id: Uuid,
        bearer_hash: &BearerHash,
        _wrapped_export_key: &WrappedExportKey,
        lease_seconds: u64,
        now: DateTime<Utc>,
    ) -> Result<LeaseRow, DbErr> {
        self.inner
            .lock()
            .unwrap()
            .inserts
            .push((tenant_id, bearer_hash.to_vec()));
        let expires_at = now + chrono::Duration::seconds(lease_seconds as i64);
        Ok(LeaseRow {
            id: Uuid::new_v4(),
            tenant_id,
            issued_at: now,
            expires_at,
            idle_extends_to: expires_at,
            revoked_at: None,
        })
    }

    async fn revoke(&self, bearer_hash: &BearerHash, _now: DateTime<Utc>) -> Result<u64, DbErr> {
        self.inner
            .lock()
            .unwrap()
            .revokes
            .push(bearer_hash.to_vec());
        Ok(1)
    }

    async fn revoke_by_id(&self, lease_id: Uuid, _now: DateTime<Utc>) -> Result<u64, DbErr> {
        if let Some(msg) = self.inner.lock().unwrap().revoke_by_id_errors.pop_front() {
            return Err(DbErr::Custom(msg));
        }
        self.inner.lock().unwrap().revokes_by_id.push(lease_id);
        Ok(1)
    }

    async fn find_active_lease_for_tenant(
        &self,
        _tenant_id: Uuid,
        _now: DateTime<Utc>,
    ) -> Result<Option<LeaseRow>, DbErr> {
        let outcome = self.inner.lock().unwrap().active_lease_outcomes.pop_front();
        match outcome {
            None | Some(ActiveLeaseOutcome::NotFound) => Ok(None),
            Some(ActiveLeaseOutcome::Found(row)) => Ok(Some(row)),
            Some(ActiveLeaseOutcome::DbError(msg)) => Err(DbErr::Custom(msg)),
        }
    }
}

// ---------------------------------------------------------------------------
// TenantStore mock
// ---------------------------------------------------------------------------

/// In-memory tenant store with a pre-seeded name → UUID map.
pub struct MockTenantStore {
    by_name: Arc<Mutex<HashMap<String, Uuid>>>,
    by_id: Arc<Mutex<HashMap<Uuid, String>>>,
    error: Option<String>,
}

impl MockTenantStore {
    /// Empty store; resolve all lookups to `Ok(None)`.
    pub fn new() -> Self {
        Self {
            by_name: Arc::new(Mutex::new(HashMap::new())),
            by_id: Arc::new(Mutex::new(HashMap::new())),
            error: None,
        }
    }

    /// Single-tenant store; both directions resolve correctly.
    pub fn with_tenant(name: &str, id: Uuid) -> Self {
        let store = Self::new();
        store.insert_tenant(name, id);
        store
    }

    /// Add a tenant mapping to the store.
    pub fn insert_tenant(&self, name: &str, id: Uuid) {
        self.by_name.lock().unwrap().insert(name.to_string(), id);
        self.by_id.lock().unwrap().insert(id, name.to_string());
    }

    /// Return a store that always responds with a DB error.
    pub fn always_error(msg: impl Into<String>) -> Self {
        Self {
            by_name: Arc::new(Mutex::new(HashMap::new())),
            by_id: Arc::new(Mutex::new(HashMap::new())),
            error: Some(msg.into()),
        }
    }
}

impl Default for MockTenantStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TenantStore for MockTenantStore {
    async fn lookup_tenant_id_by_name(&self, name: &str) -> Result<Option<Uuid>, DbErr> {
        if let Some(msg) = &self.error {
            return Err(DbErr::Custom(msg.clone()));
        }
        Ok(self.by_name.lock().unwrap().get(name).copied())
    }

    async fn lookup_tenant_name_by_id(&self, tenant_id: Uuid) -> Result<Option<String>, DbErr> {
        if let Some(msg) = &self.error {
            return Err(DbErr::Custom(msg.clone()));
        }
        Ok(self.by_id.lock().unwrap().get(&tenant_id).cloned())
    }
}

// ---------------------------------------------------------------------------
// PasswordFileStore mock
// ---------------------------------------------------------------------------

/// In-memory password-file store keyed by tenant UUID.
pub struct MockPasswordFileStore {
    files: Arc<Mutex<HashMap<Uuid, Vec<u8>>>>,
    error: Option<String>,
}

impl MockPasswordFileStore {
    /// Empty store.
    pub fn new() -> Self {
        Self {
            files: Arc::new(Mutex::new(HashMap::new())),
            error: None,
        }
    }

    /// Pre-seed a password file for one tenant.
    pub fn with_file(tenant_id: Uuid, file: &PasswordFile) -> Self {
        let store = Self::new();
        store.insert_file(tenant_id, file);
        store
    }

    /// Insert (or overwrite) a password file.
    pub fn insert_file(&self, tenant_id: Uuid, file: &PasswordFile) {
        self.files
            .lock()
            .unwrap()
            .insert(tenant_id, file.as_bytes().to_vec());
    }

    /// Return a store that always responds with a DB error.
    pub fn always_error(msg: impl Into<String>) -> Self {
        Self {
            files: Arc::new(Mutex::new(HashMap::new())),
            error: Some(msg.into()),
        }
    }
}

impl Default for MockPasswordFileStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PasswordFileStore for MockPasswordFileStore {
    async fn load_password_file(&self, tenant_id: Uuid) -> Result<Option<PasswordFile>, DbErr> {
        if let Some(msg) = &self.error {
            return Err(DbErr::Custom(msg.clone()));
        }
        let map = self.files.lock().unwrap();
        match map.get(&tenant_id) {
            None => Ok(None),
            Some(bytes) => PasswordFile::from_bytes(bytes)
                .map(Some)
                .map_err(|_| DbErr::Custom("malformed password_file in mock".to_string())),
        }
    }

    async fn upsert_password_file(
        &self,
        tenant_id: Uuid,
        password_file: &PasswordFile,
        _suite_version: i32,
    ) -> Result<(), UpsertError> {
        if let Some(msg) = &self.error {
            return Err(UpsertError::Db(DbErr::Custom(msg.clone())));
        }
        let new_bytes = password_file.as_bytes().to_vec();
        let mut map = self.files.lock().unwrap();
        match map.get(&tenant_id) {
            None => {
                map.insert(tenant_id, new_bytes);
                Ok(())
            }
            Some(existing) if existing == &new_bytes => Ok(()),
            Some(_) => Err(UpsertError::Conflict),
        }
    }
}
