//! `store` — Mockable database seam for `botwork-auth-broker`.
//!
//! This module defines three store traits that abstract every
//! database call in the broker so unit tests can inject in-memory
//! implementations without Docker or a live Postgres connection.
//!
//! ## Trait overview
//!
//! | Trait               | Responsibility                                      |
//! |---------------------|-----------------------------------------------------|
//! | [`LeaseStore`]      | Insert, validate-and-extend, revoke lease rows      |
//! | [`TenantStore`]     | Resolve tenant name ↔ UUID                         |
//! | [`PasswordFileStore`] | Load and upsert OPAQUE password files             |
//!
//! ## Production use
//!
//! [`sea_orm_impl`] provides concrete implementations backed by a
//! SeaORM [`DatabaseConnection`][sea_orm::DatabaseConnection]. The
//! `AuthState::new(db, setup)` constructor wraps the connection in
//! the three concrete impls and stores `Arc<dyn Trait + Send + Sync>`
//! for each.
//!
//! ## Test use
//!
//! The `mock` submodule (gated by `cfg(any(test, feature =
//! "test-support"))`) supplies in-memory stubs. The
//! `AuthState::from_stores` constructor (same gate) lets tests inject
//! mocks directly.

pub mod sea_orm_impl;

#[cfg(any(test, feature = "test-support"))]
pub mod mock;

use async_trait::async_trait;
use botwork_opaque_handshake::PasswordFile;
use chrono::{DateTime, Utc};
use sea_orm::DbErr;
use uuid::Uuid;

use crate::auth::lease::{
    Bearer, BearerHash, LeaseRow, ValidatedLease, ValidationError, WrappedExportKey,
};
use crate::auth::opaque::UpsertError;

/// Lease persistence and validation.
///
/// Every method maps 1-to-1 onto a function in [`crate::auth::lease`];
/// the SeaORM implementation simply delegates to those free functions.
/// The mock implementation uses an in-memory store.
#[async_trait]
pub trait LeaseStore: Send + Sync {
    /// Look up a lease by `bearer`, validate liveness, slide
    /// `idle_extends_to`, and return the unwrapped export key.
    ///
    /// - `Ok(Some(_))` — live lease, extension committed.
    /// - `Ok(None)` — row not found or KEK unwrap failed.
    /// - `Err(ValidationError::Expired)` — row past either deadline.
    /// - `Err(ValidationError::Revoked)` — row has `revoked_at` set.
    /// - `Err(ValidationError::Db(_))` — database error.
    async fn validate_and_extend(
        &self,
        bearer: &Bearer,
        now: DateTime<Utc>,
    ) -> Result<Option<ValidatedLease>, ValidationError>;

    /// Insert a fresh lease row and return the projected [`LeaseRow`].
    async fn insert_lease(
        &self,
        tenant_id: Uuid,
        bearer_hash: &BearerHash,
        wrapped_export_key: &WrappedExportKey,
        lease_seconds: u64,
        now: DateTime<Utc>,
    ) -> Result<LeaseRow, DbErr>;

    /// Mark a lease as revoked by bearer hash. Returns the number of
    /// rows affected (0 if already revoked or not found).
    async fn revoke(&self, bearer_hash: &BearerHash, now: DateTime<Utc>) -> Result<u64, DbErr>;

    /// Mark a lease as revoked by its primary-key UUID. Returns the
    /// number of rows affected (0 if already revoked or not found).
    ///
    /// Used by the admin `DELETE /admin/api/v1/leases/:id` endpoint
    /// where the operator knows the lease UUID but not the raw bearer.
    async fn revoke_by_id(&self, lease_id: Uuid, now: DateTime<Utc>) -> Result<u64, DbErr>;

    /// Find the most recently issued, currently-active (non-revoked,
    /// non-expired) lease for the given tenant. Returns `Ok(None)` if
    /// no live lease exists. Used by the remote-write path to locate
    /// the tenant's active lease without a bearer token.
    async fn find_active_lease_for_tenant(
        &self,
        tenant_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<LeaseRow>, DbErr>;
}

/// Tenant name ↔ UUID resolution.
#[async_trait]
pub trait TenantStore: Send + Sync {
    /// Resolve a tenant name to its UUID. Returns `Ok(None)` for
    /// unknown tenants.
    async fn lookup_tenant_id_by_name(&self, name: &str) -> Result<Option<Uuid>, DbErr>;

    /// Resolve a tenant UUID to its name. Returns `Ok(None)` if the
    /// tenant row no longer exists.
    async fn lookup_tenant_name_by_id(&self, tenant_id: Uuid) -> Result<Option<String>, DbErr>;
}

/// OPAQUE password-file CRUD.
#[async_trait]
pub trait PasswordFileStore: Send + Sync {
    /// Load the [`PasswordFile`] for a tenant, if one is registered.
    async fn load_password_file(&self, tenant_id: Uuid) -> Result<Option<PasswordFile>, DbErr>;

    /// Idempotent-insert of a password file. Returns
    /// [`UpsertError::Conflict`] when a **different** file already
    /// exists for the tenant.
    async fn upsert_password_file(
        &self,
        tenant_id: Uuid,
        password_file: &PasswordFile,
        suite_version: i32,
    ) -> Result<(), UpsertError>;
}
