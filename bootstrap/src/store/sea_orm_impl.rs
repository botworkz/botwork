//! `SeaOrmBootstrapStore` — [`BootstrapStore`] implementation backed by a live
//! `sea_orm::DatabaseTransaction`.
//!
//! Each trait method delegates to the corresponding `pub(crate)` free function in
//! [`crate::runner`], which owns all the SQL. No SQL lives here.
//!
//! Construct from a `DatabaseTransaction` that the caller (i.e.
//! [`crate::runner::apply`]) owns. The caller is responsible for committing or
//! rolling back the transaction after
//! [`apply_with_store`](crate::runner::apply_with_store) returns.

use async_trait::async_trait;
use botwork_api_core::plugin_spec::ValidatedPlugin;
use sea_orm::DatabaseTransaction;
use uuid::Uuid;

use crate::error::BootstrapError;
use crate::runner::{db_upsert_binding, db_upsert_plugin, db_upsert_tenant, db_upsert_workspace};
use crate::store::{BootstrapStore, UpsertOutcome};

/// A [`BootstrapStore`] that delegates to the real SeaORM free functions via a
/// borrowed `DatabaseTransaction`.
pub struct SeaOrmBootstrapStore<'tx> {
    tx: &'tx DatabaseTransaction,
}

impl<'tx> SeaOrmBootstrapStore<'tx> {
    /// Wrap an existing transaction.  The caller drives the transaction
    /// lifecycle (begin / commit / rollback).
    pub fn new(tx: &'tx DatabaseTransaction) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl<'tx> BootstrapStore for SeaOrmBootstrapStore<'tx> {
    async fn upsert_plugin(
        &self,
        entry: &ValidatedPlugin,
    ) -> Result<(Uuid, UpsertOutcome), BootstrapError> {
        db_upsert_plugin(self.tx, entry).await
    }

    async fn upsert_tenant(&self, name: &str) -> Result<(Uuid, UpsertOutcome), BootstrapError> {
        db_upsert_tenant(self.tx, name).await
    }

    async fn upsert_workspace(
        &self,
        tenant_id: Uuid,
        name: &str,
    ) -> Result<(Uuid, UpsertOutcome), BootstrapError> {
        db_upsert_workspace(self.tx, tenant_id, name).await
    }

    async fn upsert_binding(
        &self,
        workspace_id: Uuid,
        plugin_id: Uuid,
        config: Option<&serde_json::Value>,
    ) -> Result<UpsertOutcome, BootstrapError> {
        db_upsert_binding(self.tx, workspace_id, plugin_id, config).await
    }
}
