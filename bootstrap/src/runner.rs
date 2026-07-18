//! Apply a parsed [`BootstrapConfig`] to a database via the [`BootstrapStore`]
//! trait.
//!
//! All operations are idempotent upserts. There is no delete path —
//! removing rows is api territory, not boot-time territory.
//!
//! ## Production path
//!
//! [`apply`] takes a [`DatabaseConnection`], wraps all upserts in a single
//! transaction (atomicity guarantee), builds a
//! [`SeaOrmBootstrapStore`](crate::store::sea_orm_impl::SeaOrmBootstrapStore),
//! then calls [`apply_with_store`]. Behaviour is identical to the previous
//! direct-DB path.
//!
//! ## Test path
//!
//! [`apply_with_store`] takes any `impl BootstrapStore`. Pass a
//! [`MockBootstrapStore`](crate::store::mock::MockBootstrapStore) to drive the
//! full control-flow in unit tests without a real Postgres connection and
//! without the `sea_orm::MockDatabase` transaction caveat that previously
//! blocked some `apply` branches from being covered.

use std::collections::HashMap;

use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
    TransactionTrait,
};
use tracing::info;
use uuid::Uuid;

use botwork_entity::{plugin, tenant, workspace, workspace_plugin};

use botwork_api_core::plugin_spec::ValidatedPlugin;

use botwork_api_core::BootstrapConfig;

use crate::error::BootstrapError;
use crate::store::{BootstrapStore, UpsertOutcome};

/// Per-table mutation counts so the production binary can emit a
/// one-line journal summary; tests assert "second run was a no-op".
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ApplyStats {
    pub tenants_inserted: usize,
    pub tenants_updated: usize,
    pub workspaces_inserted: usize,
    pub workspaces_updated: usize,
    pub plugins_inserted: usize,
    pub plugins_updated: usize,
    pub bindings_inserted: usize,
    pub bindings_updated: usize,
}

/// Apply `config` to the database inside a single transaction.
///
/// This is the production entry point.  All upserts are wrapped in one
/// `BEGIN`/`COMMIT` so that a partial failure is never visible to readers.
pub async fn apply(
    db: &DatabaseConnection,
    config: &BootstrapConfig,
) -> Result<ApplyStats, BootstrapError> {
    let tx = db.begin().await?;
    let store = crate::store::sea_orm_impl::SeaOrmBootstrapStore::new(&tx);
    let stats = apply_with_store(&store, config).await?;
    tx.commit().await?;
    info!(
        ?stats,
        "bootstrap apply complete: tenants={} workspaces={} plugins={} bindings={}",
        stats.tenants_inserted + stats.tenants_updated,
        stats.workspaces_inserted + stats.workspaces_updated,
        stats.plugins_inserted + stats.plugins_updated,
        stats.bindings_inserted + stats.bindings_updated,
    );
    Ok(stats)
}

/// Apply `config` using any `BootstrapStore` implementation.
///
/// This is the testable inner function.  Production code calls [`apply`] which
/// wraps it in a transaction via
/// [`SeaOrmBootstrapStore`](crate::store::sea_orm_impl::SeaOrmBootstrapStore).
/// Tests pass a
/// [`MockBootstrapStore`](crate::store::mock::MockBootstrapStore).
pub async fn apply_with_store<S: BootstrapStore>(
    store: &S,
    config: &BootstrapConfig,
) -> Result<ApplyStats, BootstrapError> {
    let mut stats = ApplyStats::default();

    // -- Top-level plugins ------------------------------------------------
    let mut plugin_ids: HashMap<String, Uuid> = HashMap::new();
    for entry in &config.plugins {
        let (id, outcome) = store.upsert_plugin(entry).await?;
        match outcome {
            UpsertOutcome::Inserted => stats.plugins_inserted += 1,
            UpsertOutcome::Updated => stats.plugins_updated += 1,
            UpsertOutcome::Unchanged => {}
        }
        plugin_ids.insert(entry.name.clone(), id);
    }

    // -- Tenants + nested workspaces + bindings ---------------------------
    for tenant_entry in &config.tenants {
        let (tenant_id, outcome) = store.upsert_tenant(&tenant_entry.name).await?;
        match outcome {
            UpsertOutcome::Inserted => stats.tenants_inserted += 1,
            // Tenant has no field-diff update logic — finding an existing row
            // is always counted as "updated" for statistics purposes.
            // (Neither `upsert_tenant` nor `db_upsert_tenant` returns
            // `Unchanged`; the `Unchanged` arm is handled here defensively
            // to keep the match exhaustive without a wildcard.)
            UpsertOutcome::Updated | UpsertOutcome::Unchanged => stats.tenants_updated += 1,
        }
        for workspace_entry in &tenant_entry.workspaces {
            let (workspace_id, outcome) = store
                .upsert_workspace(tenant_id, &workspace_entry.name)
                .await?;
            match outcome {
                UpsertOutcome::Inserted => stats.workspaces_inserted += 1,
                // Same semantics as tenant: workspace has no field-diff update
                // logic, so "found existing" always maps to the updated counter.
                UpsertOutcome::Updated | UpsertOutcome::Unchanged => stats.workspaces_updated += 1,
            }
            for binding in &workspace_entry.plugins {
                let plugin_id = *plugin_ids.get(binding.name.as_str()).ok_or_else(|| {
                    // Validation guarantees this can't happen; the
                    // expect-shaped error is a tripwire.
                    BootstrapError::UnknownPluginRef {
                        tenant: tenant_entry.name.clone(),
                        workspace: workspace_entry.name.clone(),
                        plugin: binding.name.clone(),
                    }
                })?;
                let outcome = store
                    .upsert_binding(workspace_id, plugin_id, binding.config.as_ref())
                    .await?;
                match outcome {
                    UpsertOutcome::Inserted => stats.bindings_inserted += 1,
                    UpsertOutcome::Updated => stats.bindings_updated += 1,
                    UpsertOutcome::Unchanged => {}
                }
            }
        }
    }

    Ok(stats)
}

/// Look up or insert a plugin row by `entry.name`.
///
/// Returns `(id, UpsertOutcome)` without touching stats — stats are
/// accumulated by the caller ([`apply_with_store`]).
pub(crate) async fn db_upsert_plugin(
    tx: &sea_orm::DatabaseTransaction,
    entry: &ValidatedPlugin,
) -> Result<(Uuid, UpsertOutcome), BootstrapError> {
    let existing = plugin::Entity::find()
        .filter(plugin::Column::Name.eq(&entry.name))
        .one(tx)
        .await?;
    match existing {
        Some(row) => {
            let mut active: plugin::ActiveModel = row.clone().into();
            let mut changed = false;
            if row.image != entry.image {
                active.image = Set(entry.image.clone());
                changed = true;
            }
            if row.port != i32::from(entry.port) {
                active.port = Set(i32::from(entry.port));
                changed = true;
            }
            if row.path != entry.path {
                active.path = Set(entry.path.clone());
                changed = true;
            }
            if row.upstream_auth != entry.upstream_auth {
                active.upstream_auth = Set(entry.upstream_auth.clone());
                changed = true;
            }
            if row.env != entry.env {
                active.env = Set(entry.env.clone());
                changed = true;
            }
            if row.resources != entry.resources {
                active.resources = Set(entry.resources.clone());
                changed = true;
            }
            if row.egress != entry.egress {
                active.egress = Set(entry.egress.clone());
                changed = true;
            }
            if changed {
                active.updated_at = Set(chrono::Utc::now());
                active.update(tx).await?;
                Ok((row.id, UpsertOutcome::Updated))
            } else {
                Ok((row.id, UpsertOutcome::Unchanged))
            }
        }
        None => {
            let id = Uuid::new_v4();
            let now = chrono::Utc::now();
            plugin::ActiveModel {
                id: Set(id),
                name: Set(entry.name.clone()),
                image: Set(entry.image.clone()),
                port: Set(i32::from(entry.port)),
                path: Set(entry.path.clone()),
                upstream_auth: Set(entry.upstream_auth.clone()),
                env: Set(entry.env.clone()),
                resources: Set(entry.resources.clone()),
                egress: Set(entry.egress.clone()),
                created_at: Set(now),
                updated_at: Set(now),
                // RFE #146: the operator-intent `plugin` row leaves
                // `current_facet_id` NULL on create. The future
                // `botwork-image-catalog` oneshot is the only writer of
                // that pointer; bootstrap never touches it.
                current_facet_id: sea_orm::ActiveValue::NotSet,
            }
            .insert(tx)
            .await?;
            Ok((id, UpsertOutcome::Inserted))
        }
    }
}

/// Look up or insert a tenant row by `name`.
pub(crate) async fn db_upsert_tenant(
    tx: &sea_orm::DatabaseTransaction,
    name: &str,
) -> Result<(Uuid, UpsertOutcome), BootstrapError> {
    let existing = tenant::Entity::find()
        .filter(tenant::Column::Name.eq(name))
        .one(tx)
        .await?;
    if let Some(row) = existing {
        return Ok((row.id, UpsertOutcome::Updated));
    }
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();
    tenant::ActiveModel {
        id: Set(id),
        name: Set(name.to_owned()),
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(tx)
    .await?;
    Ok((id, UpsertOutcome::Inserted))
}

/// Look up or insert a workspace row by `(tenant_id, name)`.
pub(crate) async fn db_upsert_workspace(
    tx: &sea_orm::DatabaseTransaction,
    tenant_id: Uuid,
    name: &str,
) -> Result<(Uuid, UpsertOutcome), BootstrapError> {
    let existing = workspace::Entity::find()
        .filter(workspace::Column::TenantId.eq(tenant_id))
        .filter(workspace::Column::Name.eq(name))
        .one(tx)
        .await?;
    if let Some(row) = existing {
        return Ok((row.id, UpsertOutcome::Updated));
    }
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();
    workspace::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant_id),
        name: Set(name.to_owned()),
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(tx)
    .await?;
    Ok((id, UpsertOutcome::Inserted))
}

/// Look up or insert a workspace-plugin binding by `(workspace_id, plugin_id)`.
pub(crate) async fn db_upsert_binding(
    tx: &sea_orm::DatabaseTransaction,
    workspace_id: Uuid,
    plugin_id: Uuid,
    config: Option<&serde_json::Value>,
) -> Result<UpsertOutcome, BootstrapError> {
    let existing = workspace_plugin::Entity::find_by_id((workspace_id, plugin_id))
        .one(tx)
        .await?;
    match existing {
        Some(row) => {
            let mut active: workspace_plugin::ActiveModel = row.clone().into();
            let mut changed = false;
            if row.config.as_ref() != config {
                active.config = Set(config.cloned());
                changed = true;
            }
            if changed {
                active.updated_at = Set(chrono::Utc::now());
                active.update(tx).await?;
                Ok(UpsertOutcome::Updated)
            } else {
                Ok(UpsertOutcome::Unchanged)
            }
        }
        None => {
            let now = chrono::Utc::now();
            workspace_plugin::ActiveModel {
                workspace_id: Set(workspace_id),
                plugin_id: Set(plugin_id),
                config: Set(config.cloned()),
                created_at: Set(now),
                updated_at: Set(now),
            }
            .insert(tx)
            .await?;
            Ok(UpsertOutcome::Inserted)
        }
    }
}

#[cfg(test)]
mod tests {
    use botwork_entity::{plugin, tenant, workspace, workspace_plugin};

    use super::*;
    use crate::store::mock::MockBootstrapStore;
    use crate::{BootstrapConfigRaw, TenantEntry, WorkspaceEntry, WorkspacePluginEntry};

    // -----------------------------------------------------------------------
    // Config helpers
    // -----------------------------------------------------------------------

    fn sample_config() -> BootstrapConfig {
        let raw: BootstrapConfigRaw = serde_yaml::from_str(
            r#"
tenants:
- name: phlax
  workspaces:
  - name: mcp
    plugins:
    - name: mcp-bash

plugins:
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:1.0
  egress: none
"#,
        )
        .expect("parse");
        BootstrapConfig::from_raw(raw).expect("validate")
    }

    /// Extract the single plugin entry from [`sample_config`].
    fn sample_plugin_entry() -> ValidatedPlugin {
        sample_config()
            .plugins
            .into_iter()
            .next()
            .expect("sample_config has one plugin")
    }

    // -----------------------------------------------------------------------
    // Model helpers
    // -----------------------------------------------------------------------

    /// A [`plugin::Model`] whose fields all match `entry` exactly
    /// (so that `upsert_plugin` treats it as a no-op match).
    fn plugin_model_matching(id: Uuid, entry: &ValidatedPlugin) -> plugin::Model {
        let now = chrono::Utc::now();
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
        }
    }

    // -----------------------------------------------------------------------
    // apply_with_store: empty config and error propagation
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn apply_with_empty_config_is_no_op() {
        let store = MockBootstrapStore::new();
        let empty = BootstrapConfig::from_raw(BootstrapConfigRaw {
            tenants: Vec::new(),
            plugins: Vec::new(),
        })
        .expect("empty config should validate");

        let stats = apply_with_store(&store, &empty)
            .await
            .expect("should succeed");

        assert_eq!(stats, ApplyStats::default());
    }

    #[tokio::test]
    async fn apply_maps_db_error() {
        let store = MockBootstrapStore::always_error("boom");

        let err = apply_with_store(&store, &sample_config())
            .await
            .expect_err("should fail");

        assert!(matches!(err, BootstrapError::Db(_)));
    }

    // -----------------------------------------------------------------------
    // upsert_plugin branches (driven via apply_with_store)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn upsert_plugin_insert_branch_increments_inserted() {
        // No existing plugin → insert path.
        let store = MockBootstrapStore::new();
        let config = BootstrapConfig {
            plugins: vec![sample_plugin_entry()],
            tenants: vec![],
        };

        let stats = apply_with_store(&store, &config)
            .await
            .expect("should succeed");

        // id is generated internally; assert via the call-recording drain.
        assert_eq!(store.drain_upserted_plugins(), vec!["mcp-bash"]);
        assert_eq!(stats.plugins_inserted, 1);
        assert_eq!(stats.plugins_updated, 0);
    }

    #[tokio::test]
    async fn upsert_plugin_update_branch_increments_updated_when_image_differs() {
        let entry = sample_plugin_entry();
        let existing_id = Uuid::new_v4();
        // Existing row has a stale image — all other fields match.
        let existing = plugin::Model {
            image: "ghcr.io/example/mcp-bash:0.9".to_string(),
            ..plugin_model_matching(existing_id, &entry)
        };

        let store = MockBootstrapStore::new().with_plugin(existing);
        let config = BootstrapConfig {
            plugins: vec![entry],
            tenants: vec![],
        };

        let stats = apply_with_store(&store, &config)
            .await
            .expect("should succeed");

        assert_eq!(stats.plugins_inserted, 0);
        assert_eq!(stats.plugins_updated, 1);
    }

    #[tokio::test]
    async fn upsert_plugin_no_op_when_all_fields_match() {
        let entry = sample_plugin_entry();
        let existing_id = Uuid::new_v4();
        // All fields already match the entry → Unchanged.
        let existing = plugin_model_matching(existing_id, &entry);

        let store = MockBootstrapStore::new().with_plugin(existing);
        let config = BootstrapConfig {
            plugins: vec![entry],
            tenants: vec![],
        };

        let stats = apply_with_store(&store, &config)
            .await
            .expect("should succeed");

        assert_eq!(stats.plugins_inserted, 0);
        assert_eq!(stats.plugins_updated, 0);
    }

    // -----------------------------------------------------------------------
    // upsert_tenant branches
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn upsert_tenant_insert_branch_increments_inserted() {
        let entry = sample_plugin_entry();
        let existing_plugin_id = Uuid::new_v4();
        // Pre-seed plugin so it doesn't show up in insert stats.
        // No tenant pre-seeded → tenant insert path.
        let store = MockBootstrapStore::new()
            .with_plugin(plugin_model_matching(existing_plugin_id, &entry));

        let stats = apply_with_store(&store, &sample_config())
            .await
            .expect("should succeed");

        assert_eq!(stats.tenants_inserted, 1);
        assert_eq!(stats.tenants_updated, 0);
    }

    #[tokio::test]
    async fn upsert_tenant_update_branch_increments_updated() {
        let now = chrono::Utc::now();
        let entry = sample_plugin_entry();
        let existing_plugin_id = Uuid::new_v4();
        let existing_tenant_id = Uuid::new_v4();
        // Pre-seed plugin + existing tenant → tenant "update" (found) path.
        let store = MockBootstrapStore::new()
            .with_plugin(plugin_model_matching(existing_plugin_id, &entry))
            .with_tenant(tenant::Model {
                id: existing_tenant_id,
                name: "phlax".to_string(),
                created_at: now,
                updated_at: now,
            });

        let stats = apply_with_store(&store, &sample_config())
            .await
            .expect("should succeed");

        assert_eq!(stats.tenants_inserted, 0);
        assert_eq!(stats.tenants_updated, 1);
    }

    // -----------------------------------------------------------------------
    // upsert_workspace branches
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn upsert_workspace_insert_branch_increments_inserted() {
        let now = chrono::Utc::now();
        let entry = sample_plugin_entry();
        let existing_plugin_id = Uuid::new_v4();
        let existing_tenant_id = Uuid::new_v4();
        // Pre-seed plugin + tenant. No workspace → workspace insert path.
        let store = MockBootstrapStore::new()
            .with_plugin(plugin_model_matching(existing_plugin_id, &entry))
            .with_tenant(tenant::Model {
                id: existing_tenant_id,
                name: "phlax".to_string(),
                created_at: now,
                updated_at: now,
            });

        let stats = apply_with_store(&store, &sample_config())
            .await
            .expect("should succeed");

        assert_eq!(stats.workspaces_inserted, 1);
        assert_eq!(stats.workspaces_updated, 0);
    }

    #[tokio::test]
    async fn upsert_workspace_update_branch_increments_updated() {
        let now = chrono::Utc::now();
        let entry = sample_plugin_entry();
        let existing_plugin_id = Uuid::new_v4();
        let existing_tenant_id = Uuid::new_v4();
        let existing_workspace_id = Uuid::new_v4();
        // Pre-seed all three → workspace "update" (found) path.
        let store = MockBootstrapStore::new()
            .with_plugin(plugin_model_matching(existing_plugin_id, &entry))
            .with_tenant(tenant::Model {
                id: existing_tenant_id,
                name: "phlax".to_string(),
                created_at: now,
                updated_at: now,
            })
            .with_workspace(workspace::Model {
                id: existing_workspace_id,
                tenant_id: existing_tenant_id,
                name: "mcp".to_string(),
                created_at: now,
                updated_at: now,
            });

        let stats = apply_with_store(&store, &sample_config())
            .await
            .expect("should succeed");

        assert_eq!(stats.workspaces_inserted, 0);
        assert_eq!(stats.workspaces_updated, 1);
    }

    // -----------------------------------------------------------------------
    // upsert_binding branches
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn upsert_binding_insert_branch_increments_inserted() {
        let now = chrono::Utc::now();
        let entry = sample_plugin_entry();
        let existing_plugin_id = Uuid::new_v4();
        let existing_tenant_id = Uuid::new_v4();
        let existing_workspace_id = Uuid::new_v4();
        // Pre-seed plugin + tenant + workspace. No binding → insert path.
        let store = MockBootstrapStore::new()
            .with_plugin(plugin_model_matching(existing_plugin_id, &entry))
            .with_tenant(tenant::Model {
                id: existing_tenant_id,
                name: "phlax".to_string(),
                created_at: now,
                updated_at: now,
            })
            .with_workspace(workspace::Model {
                id: existing_workspace_id,
                tenant_id: existing_tenant_id,
                name: "mcp".to_string(),
                created_at: now,
                updated_at: now,
            });

        let stats = apply_with_store(&store, &sample_config())
            .await
            .expect("should succeed");

        assert_eq!(stats.bindings_inserted, 1);
        assert_eq!(stats.bindings_updated, 0);
    }

    #[tokio::test]
    async fn upsert_binding_update_branch_increments_updated_when_config_differs() {
        let now = chrono::Utc::now();
        let entry = sample_plugin_entry();
        let existing_plugin_id = Uuid::new_v4();
        let existing_tenant_id = Uuid::new_v4();
        let existing_workspace_id = Uuid::new_v4();
        let new_config = serde_json::json!({"url": "https://new.example.com"});

        // Binding pre-seeded with old config; apply will see a diff.
        let store = MockBootstrapStore::new()
            .with_plugin(plugin_model_matching(existing_plugin_id, &entry))
            .with_tenant(tenant::Model {
                id: existing_tenant_id,
                name: "phlax".to_string(),
                created_at: now,
                updated_at: now,
            })
            .with_workspace(workspace::Model {
                id: existing_workspace_id,
                tenant_id: existing_tenant_id,
                name: "mcp".to_string(),
                created_at: now,
                updated_at: now,
            })
            .with_binding(workspace_plugin::Model {
                workspace_id: existing_workspace_id,
                plugin_id: existing_plugin_id,
                config: Some(serde_json::json!({"url": "https://old.example.com"})),
                created_at: now,
                updated_at: now,
            });

        let config = BootstrapConfig {
            plugins: sample_config().plugins,
            tenants: vec![TenantEntry {
                name: "phlax".to_string(),
                workspaces: vec![WorkspaceEntry {
                    name: "mcp".to_string(),
                    plugins: vec![WorkspacePluginEntry {
                        name: "mcp-bash".to_string(),
                        config: Some(new_config),
                    }],
                }],
            }],
        };

        let stats = apply_with_store(&store, &config)
            .await
            .expect("should succeed");

        assert_eq!(stats.bindings_inserted, 0);
        assert_eq!(stats.bindings_updated, 1);
    }

    #[tokio::test]
    async fn upsert_binding_no_op_when_config_unchanged() {
        let now = chrono::Utc::now();
        let entry = sample_plugin_entry();
        let existing_plugin_id = Uuid::new_v4();
        let existing_tenant_id = Uuid::new_v4();
        let existing_workspace_id = Uuid::new_v4();

        // Binding pre-seeded with None config; sample_config binding also has None.
        let store = MockBootstrapStore::new()
            .with_plugin(plugin_model_matching(existing_plugin_id, &entry))
            .with_tenant(tenant::Model {
                id: existing_tenant_id,
                name: "phlax".to_string(),
                created_at: now,
                updated_at: now,
            })
            .with_workspace(workspace::Model {
                id: existing_workspace_id,
                tenant_id: existing_tenant_id,
                name: "mcp".to_string(),
                created_at: now,
                updated_at: now,
            })
            .with_binding(workspace_plugin::Model {
                workspace_id: existing_workspace_id,
                plugin_id: existing_plugin_id,
                config: None,
                created_at: now,
                updated_at: now,
            });

        let stats = apply_with_store(&store, &sample_config())
            .await
            .expect("should succeed");

        assert_eq!(stats.bindings_inserted, 0);
        assert_eq!(stats.bindings_updated, 0);
    }

    // -----------------------------------------------------------------------
    // apply_with_store full-path: stats accounting + UnknownPluginRef tripwire
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn apply_all_inserts_increments_all_stats() {
        // Empty store → every entity gets inserted.
        let store = MockBootstrapStore::new();

        let stats = apply_with_store(&store, &sample_config())
            .await
            .expect("apply should succeed");

        assert_eq!(stats.plugins_inserted, 1);
        assert_eq!(stats.plugins_updated, 0);
        assert_eq!(stats.tenants_inserted, 1);
        assert_eq!(stats.tenants_updated, 0);
        assert_eq!(stats.workspaces_inserted, 1);
        assert_eq!(stats.workspaces_updated, 0);
        assert_eq!(stats.bindings_inserted, 1);
        assert_eq!(stats.bindings_updated, 0);
    }

    #[tokio::test]
    async fn apply_all_existing_unchanged_increments_tenant_workspace_updated_only() {
        // All rows already exist and match → no inserts, no DB-level updates.
        // Tenant and workspace always increment _updated when found.
        // Plugin and binding only increment _updated when fields differ —
        // here they match, so those counters stay at zero.
        let now = chrono::Utc::now();
        let entry = sample_plugin_entry();
        let plugin_id = Uuid::new_v4();
        let tenant_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();

        let store = MockBootstrapStore::new()
            .with_plugin(plugin_model_matching(plugin_id, &entry))
            .with_tenant(tenant::Model {
                id: tenant_id,
                name: "phlax".to_string(),
                created_at: now,
                updated_at: now,
            })
            .with_workspace(workspace::Model {
                id: workspace_id,
                tenant_id,
                name: "mcp".to_string(),
                created_at: now,
                updated_at: now,
            })
            .with_binding(workspace_plugin::Model {
                workspace_id,
                plugin_id,
                config: None, // matches the sample_config binding (no config blob)
                created_at: now,
                updated_at: now,
            });

        let stats = apply_with_store(&store, &sample_config())
            .await
            .expect("apply should succeed");

        assert_eq!(stats.plugins_inserted, 0);
        assert_eq!(stats.plugins_updated, 0); // same fields → no DB update
        assert_eq!(stats.tenants_inserted, 0);
        assert_eq!(stats.tenants_updated, 1);
        assert_eq!(stats.workspaces_inserted, 0);
        assert_eq!(stats.workspaces_updated, 1);
        assert_eq!(stats.bindings_inserted, 0);
        assert_eq!(stats.bindings_updated, 0); // same config → no DB update
    }

    #[tokio::test]
    async fn apply_returns_unknown_plugin_ref_for_dangling_binding() {
        // Directly construct a BootstrapConfig with a dangling binding
        // reference. BootstrapConfig::from_raw would reject this, so
        // we build the struct directly — exercising the apply-layer
        // tripwire that guards against future invariant violations.
        let config = BootstrapConfig {
            plugins: vec![],
            tenants: vec![TenantEntry {
                name: "test".to_string(),
                workspaces: vec![WorkspaceEntry {
                    name: "ws".to_string(),
                    plugins: vec![WorkspacePluginEntry {
                        name: "ghost-plugin".to_string(),
                        config: None,
                    }],
                }],
            }],
        };

        // Empty store; the error is raised in apply_with_store's own logic
        // before any binding DB query because "ghost-plugin" was never added
        // to the plugin_ids map (config.plugins is empty).
        let store = MockBootstrapStore::new();

        let err = apply_with_store(&store, &config)
            .await
            .expect_err("should fail with UnknownPluginRef");

        assert!(
            matches!(&err, BootstrapError::UnknownPluginRef { plugin, .. } if plugin == "ghost-plugin"),
            "expected UnknownPluginRef, got: {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // call-recording: upsert order matches config-document order
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn apply_upserts_plugins_in_config_order() {
        let store = MockBootstrapStore::new();

        apply_with_store(&store, &sample_config())
            .await
            .expect("should succeed");

        let upserted = store.drain_upserted_plugins();
        assert_eq!(upserted, vec!["mcp-bash"]);
    }

    #[tokio::test]
    async fn apply_upserts_tenants_in_config_order() {
        let store = MockBootstrapStore::new();

        apply_with_store(&store, &sample_config())
            .await
            .expect("should succeed");

        let upserted = store.drain_upserted_tenants();
        assert_eq!(upserted, vec!["phlax"]);
    }
}
