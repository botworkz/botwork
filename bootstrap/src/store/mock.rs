//! In-memory [`BootstrapStore`] mock for unit tests.
//!
//! `MockBootstrapStore` mirrors the auth-broker and `botwork-api` mock
//! conventions:
//!
//! * `::new()` — empty store (all upserts insert).
//! * `::always_error(msg)` — every method returns `BootstrapError::Db(…)`.
//! * `::with_plugin / ::with_tenant / ::with_workspace / ::with_binding` — seed
//!   with pre-existing rows so that specific upsert branches are taken.
//! * `drain_upserted_*()` — call-recording for ordering assertions.
//!
//! The mock faithfully implements the same upsert semantics as the production
//! SeaORM path (field-level diff for plugin and binding; existence-only check
//! for tenant and workspace), so test outcomes match production outcomes.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use botwork_api_core::plugin_spec::ValidatedPlugin;
use botwork_entity::{plugin, tenant, workspace, workspace_plugin};
use uuid::Uuid;

use crate::error::BootstrapError;
use crate::store::{BootstrapStore, UpsertOutcome};

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Inner {
    plugins: HashMap<String, plugin::Model>,
    tenants: HashMap<String, tenant::Model>,
    workspaces: HashMap<(Uuid, String), workspace::Model>,
    bindings: HashMap<(Uuid, Uuid), workspace_plugin::Model>,
    // call recording — drained by drain_upserted_* helpers
    upserted_plugins: Vec<String>,
    upserted_tenants: Vec<String>,
    upserted_workspaces: Vec<(Uuid, String)>,
    upserted_bindings: Vec<(Uuid, Uuid)>,
}

// ---------------------------------------------------------------------------
// Public mock type
// ---------------------------------------------------------------------------

/// In-memory [`BootstrapStore`] for unit tests.
#[derive(Clone, Default)]
pub struct MockBootstrapStore {
    inner: Arc<Mutex<Inner>>,
    always_error: Option<String>,
}

impl MockBootstrapStore {
    /// Empty store — every upsert inserts a fresh row.
    pub fn new() -> Self {
        Self::default()
    }

    /// All trait methods immediately return `BootstrapError::Db(DbErr::Custom(msg))`.
    pub fn always_error(msg: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            always_error: Some(msg.into()),
        }
    }

    /// Seed the store with a pre-existing plugin row (keyed by `row.name`).
    pub fn with_plugin(self, row: plugin::Model) -> Self {
        self.inner
            .lock()
            .expect("lock")
            .plugins
            .insert(row.name.clone(), row);
        self
    }

    /// Seed the store with a pre-existing tenant row (keyed by `row.name`).
    pub fn with_tenant(self, row: tenant::Model) -> Self {
        self.inner
            .lock()
            .expect("lock")
            .tenants
            .insert(row.name.clone(), row);
        self
    }

    /// Seed the store with a pre-existing workspace row (keyed by
    /// `(row.tenant_id, row.name)`).
    pub fn with_workspace(self, row: workspace::Model) -> Self {
        self.inner
            .lock()
            .expect("lock")
            .workspaces
            .insert((row.tenant_id, row.name.clone()), row);
        self
    }

    /// Seed the store with a pre-existing workspace-plugin binding (keyed by
    /// `(row.workspace_id, row.plugin_id)`).
    pub fn with_binding(self, row: workspace_plugin::Model) -> Self {
        self.inner
            .lock()
            .expect("lock")
            .bindings
            .insert((row.workspace_id, row.plugin_id), row);
        self
    }

    /// Drain and return the plugin names that were upserted, in call order.
    pub fn drain_upserted_plugins(&self) -> Vec<String> {
        std::mem::take(&mut self.inner.lock().expect("lock").upserted_plugins)
    }

    /// Drain and return the tenant names that were upserted, in call order.
    pub fn drain_upserted_tenants(&self) -> Vec<String> {
        std::mem::take(&mut self.inner.lock().expect("lock").upserted_tenants)
    }

    /// Drain and return the `(tenant_id, workspace_name)` pairs that were
    /// upserted, in call order.
    pub fn drain_upserted_workspaces(&self) -> Vec<(Uuid, String)> {
        std::mem::take(&mut self.inner.lock().expect("lock").upserted_workspaces)
    }

    /// Drain and return the `(workspace_id, plugin_id)` pairs that were
    /// upserted, in call order.
    pub fn drain_upserted_bindings(&self) -> Vec<(Uuid, Uuid)> {
        std::mem::take(&mut self.inner.lock().expect("lock").upserted_bindings)
    }

    fn maybe_err(&self) -> Option<BootstrapError> {
        self.always_error
            .as_ref()
            .map(|msg| BootstrapError::Db(sea_orm::DbErr::Custom(msg.to_string())))
    }
}

// ---------------------------------------------------------------------------
// BootstrapStore impl
// ---------------------------------------------------------------------------

#[async_trait]
impl BootstrapStore for MockBootstrapStore {
    async fn upsert_plugin(
        &self,
        entry: &ValidatedPlugin,
    ) -> Result<(Uuid, UpsertOutcome), BootstrapError> {
        if let Some(err) = self.maybe_err() {
            return Err(err);
        }
        let mut inner = self.inner.lock().expect("lock");
        inner.upserted_plugins.push(entry.name.clone());

        if let Some(existing) = inner.plugins.get(&entry.name).cloned() {
            let changed = existing.image != entry.image
                || existing.port != i32::from(entry.port)
                || existing.path != entry.path
                || existing.upstream_auth != entry.upstream_auth
                || existing.env != entry.env
                || existing.resources != entry.resources
                || existing.egress != entry.egress;

            if changed {
                let updated = plugin::Model {
                    image: entry.image.clone(),
                    port: i32::from(entry.port),
                    path: entry.path.clone(),
                    upstream_auth: entry.upstream_auth.clone(),
                    env: entry.env.clone(),
                    resources: entry.resources.clone(),
                    egress: entry.egress.clone(),
                    updated_at: chrono::Utc::now(),
                    ..existing.clone()
                };
                inner.plugins.insert(entry.name.clone(), updated);
                Ok((existing.id, UpsertOutcome::Updated))
            } else {
                Ok((existing.id, UpsertOutcome::Unchanged))
            }
        } else {
            let id = Uuid::new_v4();
            let now = chrono::Utc::now();
            inner.plugins.insert(
                entry.name.clone(),
                plugin::Model {
                    id,
                    name: entry.name.clone(),
                    image: entry.image.clone(),
                    port: i32::from(entry.port),
                    path: entry.path.clone(),
                    upstream_auth: entry.upstream_auth.clone(),
                    env: entry.env.clone(),
                    resources: entry.resources.clone(),
                    egress: entry.egress.clone(),
                    created_at: now,
                    updated_at: now,
                    current_facet_id: None,
                },
            );
            Ok((id, UpsertOutcome::Inserted))
        }
    }

    async fn upsert_tenant(&self, name: &str) -> Result<(Uuid, UpsertOutcome), BootstrapError> {
        if let Some(err) = self.maybe_err() {
            return Err(err);
        }
        let mut inner = self.inner.lock().expect("lock");
        inner.upserted_tenants.push(name.to_string());

        if let Some(existing) = inner.tenants.get(name) {
            Ok((existing.id, UpsertOutcome::Updated))
        } else {
            let id = Uuid::new_v4();
            let now = chrono::Utc::now();
            inner.tenants.insert(
                name.to_string(),
                tenant::Model {
                    id,
                    name: name.to_string(),
                    created_at: now,
                    updated_at: now,
                },
            );
            Ok((id, UpsertOutcome::Inserted))
        }
    }

    async fn upsert_workspace(
        &self,
        tenant_id: Uuid,
        name: &str,
    ) -> Result<(Uuid, UpsertOutcome), BootstrapError> {
        if let Some(err) = self.maybe_err() {
            return Err(err);
        }
        let mut inner = self.inner.lock().expect("lock");
        inner
            .upserted_workspaces
            .push((tenant_id, name.to_string()));

        let key = (tenant_id, name.to_string());
        if let Some(existing) = inner.workspaces.get(&key) {
            Ok((existing.id, UpsertOutcome::Updated))
        } else {
            let id = Uuid::new_v4();
            let now = chrono::Utc::now();
            inner.workspaces.insert(
                key,
                workspace::Model {
                    id,
                    tenant_id,
                    name: name.to_string(),
                    created_at: now,
                    updated_at: now,
                },
            );
            Ok((id, UpsertOutcome::Inserted))
        }
    }

    async fn upsert_binding(
        &self,
        workspace_id: Uuid,
        plugin_id: Uuid,
        config: Option<&serde_json::Value>,
    ) -> Result<UpsertOutcome, BootstrapError> {
        if let Some(err) = self.maybe_err() {
            return Err(err);
        }
        let mut inner = self.inner.lock().expect("lock");
        inner.upserted_bindings.push((workspace_id, plugin_id));

        let key = (workspace_id, plugin_id);
        if let Some(existing) = inner.bindings.get(&key).cloned() {
            if existing.config.as_ref() != config {
                let updated = workspace_plugin::Model {
                    config: config.cloned(),
                    updated_at: chrono::Utc::now(),
                    ..existing
                };
                inner.bindings.insert(key, updated);
                Ok(UpsertOutcome::Updated)
            } else {
                Ok(UpsertOutcome::Unchanged)
            }
        } else {
            let now = chrono::Utc::now();
            inner.bindings.insert(
                key,
                workspace_plugin::Model {
                    workspace_id,
                    plugin_id,
                    config: config.cloned(),
                    created_at: now,
                    updated_at: now,
                },
            );
            Ok(UpsertOutcome::Inserted)
        }
    }
}
