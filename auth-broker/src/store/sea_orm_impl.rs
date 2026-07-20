//! SeaORM-backed implementations of the broker store traits.

use std::sync::Arc;

use async_trait::async_trait;
use botwork_opaque_handshake::PasswordFile;
use chrono::{DateTime, Utc};
use sea_orm::{DatabaseConnection, DbErr};
use uuid::Uuid;

use crate::auth::lease::{
    insert_lease as db_insert_lease, revoke as db_revoke, revoke_by_id as db_revoke_by_id,
    validate_and_extend as db_validate, Bearer, BearerHash, LeaseRow, ValidatedLease,
    ValidationError, WrappedExportKey,
};
use crate::auth::opaque::{
    load_password_file as db_load_password_file, lookup_tenant_id_by_name as db_lookup_by_name,
    lookup_tenant_name_by_id as db_lookup_by_id, upsert_password_file as db_upsert_password_file,
    UpsertError,
};
use crate::store::{LeaseStore, PasswordFileStore, TenantStore};

/// SeaORM-backed lease store.
///
/// Wraps a [`DatabaseConnection`] behind an `Arc` so the struct is always
/// `Clone` regardless of which sea-orm backend features are active (the
/// `mock` feature removes `Clone` from `DatabaseConnection` itself, so
/// we carry the `Arc` rather than a bare clone of the connection).
#[derive(Clone)]
pub struct SeaOrmLeaseStore {
    // Private: callers must use `new` or `new_shared`; references to the
    // underlying connection go through the `LeaseStore` trait methods which
    // dereference the Arc appropriately.
    db: Arc<DatabaseConnection>,
}

impl SeaOrmLeaseStore {
    /// Wrap a [`DatabaseConnection`] in this store.
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db: Arc::new(db) }
    }

    /// Construct from a pre-existing `Arc<DatabaseConnection>`, allowing
    /// multiple stores to share the same underlying connection without an
    /// extra allocation.  Used internally by [`crate::auth::endpoints::AuthState::new`].
    pub(crate) fn new_shared(db: Arc<DatabaseConnection>) -> Self {
        Self { db }
    }
}

#[async_trait]
impl LeaseStore for SeaOrmLeaseStore {
    async fn validate_and_extend(
        &self,
        bearer: &Bearer,
        now: DateTime<Utc>,
    ) -> Result<Option<ValidatedLease>, ValidationError> {
        db_validate(&self.db, bearer, now).await
    }

    async fn insert_lease(
        &self,
        tenant_id: Uuid,
        bearer_hash: &BearerHash,
        wrapped_export_key: &WrappedExportKey,
        lease_seconds: u64,
        now: DateTime<Utc>,
    ) -> Result<LeaseRow, DbErr> {
        db_insert_lease(
            &*self.db,
            tenant_id,
            bearer_hash,
            wrapped_export_key,
            lease_seconds,
            now,
        )
        .await
    }

    async fn revoke(&self, bearer_hash: &BearerHash, now: DateTime<Utc>) -> Result<u64, DbErr> {
        db_revoke(&*self.db, bearer_hash, now).await
    }

    async fn revoke_by_id(&self, lease_id: Uuid, now: DateTime<Utc>) -> Result<u64, DbErr> {
        db_revoke_by_id(&*self.db, lease_id, now).await
    }

    async fn find_active_lease_for_tenant(
        &self,
        tenant_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<LeaseRow>, DbErr> {
        use botwork_entity::lease;
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

        let model = lease::Entity::find()
            .filter(lease::Column::TenantId.eq(tenant_id))
            .filter(lease::Column::RevokedAt.is_null())
            .filter(lease::Column::ExpiresAt.gt(now))
            .filter(lease::Column::IdleExtendsTo.gt(now))
            .order_by_desc(lease::Column::IssuedAt)
            .one(&*self.db)
            .await?;
        Ok(model.map(LeaseRow::from))
    }
}

/// SeaORM-backed tenant store.
#[derive(Clone)]
pub struct SeaOrmTenantStore {
    db: Arc<DatabaseConnection>,
}

impl SeaOrmTenantStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db: Arc::new(db) }
    }

    pub(crate) fn new_shared(db: Arc<DatabaseConnection>) -> Self {
        Self { db }
    }
}

#[async_trait]
impl TenantStore for SeaOrmTenantStore {
    async fn lookup_tenant_id_by_name(&self, name: &str) -> Result<Option<Uuid>, DbErr> {
        db_lookup_by_name(&*self.db, name).await
    }

    async fn lookup_tenant_name_by_id(&self, tenant_id: Uuid) -> Result<Option<String>, DbErr> {
        db_lookup_by_id(&*self.db, tenant_id).await
    }
}

/// SeaORM-backed password-file store.
#[derive(Clone)]
pub struct SeaOrmPasswordFileStore {
    db: Arc<DatabaseConnection>,
}

impl SeaOrmPasswordFileStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db: Arc::new(db) }
    }

    pub(crate) fn new_shared(db: Arc<DatabaseConnection>) -> Self {
        Self { db }
    }
}

#[async_trait]
impl PasswordFileStore for SeaOrmPasswordFileStore {
    async fn load_password_file(&self, tenant_id: Uuid) -> Result<Option<PasswordFile>, DbErr> {
        db_load_password_file(&*self.db, tenant_id).await
    }

    async fn upsert_password_file(
        &self,
        tenant_id: Uuid,
        password_file: &PasswordFile,
        suite_version: i32,
    ) -> Result<(), UpsertError> {
        db_upsert_password_file(&*self.db, tenant_id, password_file, suite_version).await
    }
}
