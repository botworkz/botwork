//! Store-trait abstraction over the bootstrap crate's DB operations.
//!
//! The [`BootstrapStore`] trait exposes one method per upsert operation so that
//! the [`apply_with_store`](crate::runner::apply_with_store) control flow can be
//! driven by an in-memory [`MockBootstrapStore`](mock::MockBootstrapStore) in
//! unit tests without requiring a real Postgres connection or
//! `sea_orm::MockDatabase` transaction plumbing.
//!
//! Production code wires [`sea_orm_impl::SeaOrmBootstrapStore`] (backed by a
//! live `DatabaseTransaction`) and the existing transaction semantics are
//! preserved in [`crate::runner::apply`].

use async_trait::async_trait;
use botwork_api_core::plugin_spec::ValidatedPlugin;
use uuid::Uuid;

use crate::error::BootstrapError;

pub mod mock;
pub mod sea_orm_impl;

/// Whether an upsert wrote a new row, mutated an existing row, or was a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    /// A new row was inserted.
    Inserted,
    /// An existing row was found and one or more fields were updated.
    Updated,
    /// An existing row was found and all comparable fields already matched; no
    /// DB write was issued.
    Unchanged,
}

/// Abstraction over the four idempotent upsert operations that
/// [`crate::runner::apply_with_store`] drives.
///
/// Implementations must be `Send + Sync` so that the store can be passed across
/// await points inside `apply_with_store`.
#[async_trait]
pub trait BootstrapStore: Send + Sync {
    /// Upsert a plugin row keyed on `entry.name`.
    ///
    /// * `Ok((id, Inserted))` — new row written.
    /// * `Ok((id, Updated))` — existing row found; one or more fields changed.
    /// * `Ok((id, Unchanged))` — existing row found; all fields matched.
    /// * `Err(BootstrapError::Db(_))` — database failure.
    async fn upsert_plugin(
        &self,
        entry: &ValidatedPlugin,
    ) -> Result<(Uuid, UpsertOutcome), BootstrapError>;

    /// Upsert a tenant row keyed on `name`.
    ///
    /// * `Ok((id, Inserted))` — new row written.
    /// * `Ok((id, Updated))` — existing tenant found (tenant has no field-diff
    ///   logic; finding it is always counted as "updated" for statistics).
    /// * `Err(BootstrapError::Db(_))` — database failure.
    async fn upsert_tenant(&self, name: &str) -> Result<(Uuid, UpsertOutcome), BootstrapError>;

    /// Upsert a workspace row keyed on `(tenant_id, name)`.
    ///
    /// * `Ok((id, Inserted))` — new row written.
    /// * `Ok((id, Updated))` — existing workspace found (workspace has no
    ///   field-diff logic; finding it is always counted as "updated").
    /// * `Err(BootstrapError::Db(_))` — database failure.
    async fn upsert_workspace(
        &self,
        tenant_id: Uuid,
        name: &str,
    ) -> Result<(Uuid, UpsertOutcome), BootstrapError>;

    /// Upsert a workspace-plugin binding keyed on `(workspace_id, plugin_id)`.
    ///
    /// * `Ok(Inserted)` — new row written.
    /// * `Ok(Updated)` — existing binding found; `config` differed.
    /// * `Ok(Unchanged)` — existing binding found; `config` already matched.
    /// * `Err(BootstrapError::Db(_))` — database failure.
    async fn upsert_binding(
        &self,
        workspace_id: Uuid,
        plugin_id: Uuid,
        config: Option<&serde_json::Value>,
    ) -> Result<UpsertOutcome, BootstrapError>;
}
