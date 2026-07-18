//! Apply a parsed [`BootstrapConfig`] to a [`DatabaseConnection`].
//!
//! All operations are idempotent upserts. There is no delete path —
//! removing rows is api territory, not boot-time territory.
//!
//! The whole apply is wrapped in a single transaction. Either the boot
//! sees the full new state of bootstrap.yaml or it sees the previous
//! state — never a partial merge. config-broker's hot-path reads happen
//! against the committed state.

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

pub async fn apply(
    db: &DatabaseConnection,
    config: &BootstrapConfig,
) -> Result<ApplyStats, BootstrapError> {
    let tx = db.begin().await?;
    let mut stats = ApplyStats::default();

    // -- Top-level plugins ------------------------------------------------
    let mut plugin_ids: HashMap<String, Uuid> = HashMap::new();
    for entry in &config.plugins {
        let id = upsert_plugin(&tx, entry, &mut stats).await?;
        plugin_ids.insert(entry.name.clone(), id);
    }

    // -- Tenants + nested workspaces + bindings ---------------------------
    for tenant_entry in &config.tenants {
        let tenant_id = upsert_tenant(&tx, &tenant_entry.name, &mut stats).await?;
        for workspace_entry in &tenant_entry.workspaces {
            let workspace_id =
                upsert_workspace(&tx, tenant_id, &workspace_entry.name, &mut stats).await?;
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
                upsert_binding(
                    &tx,
                    workspace_id,
                    plugin_id,
                    binding.config.as_ref(),
                    &mut stats,
                )
                .await?;
            }
        }
    }

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

async fn upsert_plugin(
    tx: &sea_orm::DatabaseTransaction,
    entry: &ValidatedPlugin,
    stats: &mut ApplyStats,
) -> Result<Uuid, BootstrapError> {
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
                stats.plugins_updated += 1;
            }
            Ok(row.id)
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
            stats.plugins_inserted += 1;
            Ok(id)
        }
    }
}

async fn upsert_tenant(
    tx: &sea_orm::DatabaseTransaction,
    name: &str,
    stats: &mut ApplyStats,
) -> Result<Uuid, BootstrapError> {
    let existing = tenant::Entity::find()
        .filter(tenant::Column::Name.eq(name))
        .one(tx)
        .await?;
    if let Some(row) = existing {
        stats.tenants_updated += 1;
        return Ok(row.id);
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
    stats.tenants_inserted += 1;
    Ok(id)
}

async fn upsert_workspace(
    tx: &sea_orm::DatabaseTransaction,
    tenant_id: Uuid,
    name: &str,
    stats: &mut ApplyStats,
) -> Result<Uuid, BootstrapError> {
    let existing = workspace::Entity::find()
        .filter(workspace::Column::TenantId.eq(tenant_id))
        .filter(workspace::Column::Name.eq(name))
        .one(tx)
        .await?;
    if let Some(row) = existing {
        stats.workspaces_updated += 1;
        return Ok(row.id);
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
    stats.workspaces_inserted += 1;
    Ok(id)
}

async fn upsert_binding(
    tx: &sea_orm::DatabaseTransaction,
    workspace_id: Uuid,
    plugin_id: Uuid,
    config: Option<&serde_json::Value>,
    stats: &mut ApplyStats,
) -> Result<(), BootstrapError> {
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
                stats.bindings_updated += 1;
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
            stats.bindings_inserted += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sea_orm::{DatabaseBackend, DbErr, MockDatabase};

    use super::*;
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
    // Existing apply-level smoke tests (kept unchanged)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn apply_works_with_mock_database() {
        let db =
            crate::test_support::mock_db_connection(MockDatabase::new(DatabaseBackend::Postgres));
        let empty = BootstrapConfig::from_raw(BootstrapConfigRaw {
            tenants: Vec::new(),
            plugins: Vec::new(),
        })
        .expect("empty config should validate");

        let stats = apply(&db, &empty).await.expect("mock apply should succeed");

        assert_eq!(stats, ApplyStats::default());
    }

    #[tokio::test]
    async fn apply_maps_query_error() {
        let db = crate::test_support::mock_db_connection(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_errors([DbErr::Custom("boom".to_string())]),
        );

        let err = apply(&db, &sample_config()).await.expect_err("should fail");
        assert!(matches!(err, BootstrapError::Db(_)));
    }

    // -----------------------------------------------------------------------
    // upsert_plugin
    //
    // Sea-orm on a Postgres MockDatabase uses INSERT … RETURNING * and
    // UPDATE … RETURNING * (both via SelectorRaw::one), so every
    // statement — SELECT, INSERT, and UPDATE alike — consumes one entry
    // from the `query_results` queue.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn upsert_plugin_insert_branch_increments_inserted() {
        let entry = sample_plugin_entry();
        let fake_id = Uuid::new_v4();
        let returned = plugin_model_matching(fake_id, &entry);

        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![] as Vec<plugin::Model>]) // SELECT → empty
            .append_query_results([vec![returned]]) //              INSERT RETURNING
            .into_connection();
        let tx = db.begin().await.expect("begin");
        let mut stats = ApplyStats::default();

        let id = upsert_plugin(&tx, &entry, &mut stats)
            .await
            .expect("upsert_plugin should succeed");

        // id is generated client-side; it's a fresh non-nil UUID.
        assert_ne!(id, Uuid::nil());
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
        let updated = plugin_model_matching(existing_id, &entry); // updated row returned by UPDATE RETURNING

        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![existing]]) // SELECT → existing (stale image)
            .append_query_results([vec![updated]]) //  UPDATE RETURNING → updated row
            .into_connection();
        let tx = db.begin().await.expect("begin");
        let mut stats = ApplyStats::default();

        let id = upsert_plugin(&tx, &entry, &mut stats)
            .await
            .expect("upsert_plugin should succeed");

        assert_eq!(id, existing_id); // returns existing row's id
        assert_eq!(stats.plugins_inserted, 0);
        assert_eq!(stats.plugins_updated, 1);
    }

    #[tokio::test]
    async fn upsert_plugin_no_op_when_all_fields_match() {
        let entry = sample_plugin_entry();
        let existing_id = Uuid::new_v4();
        // All fields match the entry → no UPDATE is issued.
        let existing = plugin_model_matching(existing_id, &entry);

        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![existing]]) // SELECT → existing (unchanged)
            // No UPDATE result needed.
            .into_connection();
        let tx = db.begin().await.expect("begin");
        let mut stats = ApplyStats::default();

        let id = upsert_plugin(&tx, &entry, &mut stats)
            .await
            .expect("upsert_plugin should succeed");

        assert_eq!(id, existing_id);
        assert_eq!(stats.plugins_inserted, 0);
        assert_eq!(stats.plugins_updated, 0);
    }

    // -----------------------------------------------------------------------
    // upsert_tenant
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn upsert_tenant_insert_branch_increments_inserted() {
        let now = chrono::Utc::now();
        let fake_id = Uuid::new_v4();
        let returned = tenant::Model {
            id: fake_id,
            name: "phlax".to_string(),
            created_at: now,
            updated_at: now,
        };

        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![] as Vec<tenant::Model>]) // SELECT → empty
            .append_query_results([vec![returned]]) //              INSERT RETURNING
            .into_connection();
        let tx = db.begin().await.expect("begin");
        let mut stats = ApplyStats::default();

        let id = upsert_tenant(&tx, "phlax", &mut stats)
            .await
            .expect("upsert_tenant should succeed");

        assert_ne!(id, Uuid::nil());
        assert_eq!(stats.tenants_inserted, 1);
        assert_eq!(stats.tenants_updated, 0);
    }

    #[tokio::test]
    async fn upsert_tenant_update_branch_increments_updated() {
        let now = chrono::Utc::now();
        let existing_id = Uuid::new_v4();
        let existing = tenant::Model {
            id: existing_id,
            name: "phlax".to_string(),
            created_at: now,
            updated_at: now,
        };

        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![existing]]) // SELECT → existing row; tenant has no UPDATE path
            .into_connection();
        let tx = db.begin().await.expect("begin");
        let mut stats = ApplyStats::default();

        let id = upsert_tenant(&tx, "phlax", &mut stats)
            .await
            .expect("upsert_tenant should succeed");

        assert_eq!(id, existing_id);
        assert_eq!(stats.tenants_inserted, 0);
        assert_eq!(stats.tenants_updated, 1);
    }

    // -----------------------------------------------------------------------
    // upsert_workspace
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn upsert_workspace_insert_branch_increments_inserted() {
        let now = chrono::Utc::now();
        let tenant_id = Uuid::new_v4();
        let fake_id = Uuid::new_v4();
        let returned = workspace::Model {
            id: fake_id,
            tenant_id,
            name: "mcp".to_string(),
            created_at: now,
            updated_at: now,
        };

        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![] as Vec<workspace::Model>]) // SELECT → empty
            .append_query_results([vec![returned]]) //                 INSERT RETURNING
            .into_connection();
        let tx = db.begin().await.expect("begin");
        let mut stats = ApplyStats::default();

        let id = upsert_workspace(&tx, tenant_id, "mcp", &mut stats)
            .await
            .expect("upsert_workspace should succeed");

        assert_ne!(id, Uuid::nil());
        assert_eq!(stats.workspaces_inserted, 1);
        assert_eq!(stats.workspaces_updated, 0);
    }

    #[tokio::test]
    async fn upsert_workspace_update_branch_increments_updated() {
        let now = chrono::Utc::now();
        let tenant_id = Uuid::new_v4();
        let existing_id = Uuid::new_v4();
        let existing = workspace::Model {
            id: existing_id,
            tenant_id,
            name: "mcp".to_string(),
            created_at: now,
            updated_at: now,
        };

        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![existing]]) // SELECT → existing; workspace has no UPDATE path
            .into_connection();
        let tx = db.begin().await.expect("begin");
        let mut stats = ApplyStats::default();

        let id = upsert_workspace(&tx, tenant_id, "mcp", &mut stats)
            .await
            .expect("upsert_workspace should succeed");

        assert_eq!(id, existing_id);
        assert_eq!(stats.workspaces_inserted, 0);
        assert_eq!(stats.workspaces_updated, 1);
    }

    // -----------------------------------------------------------------------
    // upsert_binding
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn upsert_binding_insert_branch_increments_inserted() {
        let now = chrono::Utc::now();
        let workspace_id = Uuid::new_v4();
        let plugin_id = Uuid::new_v4();
        let returned = workspace_plugin::Model {
            workspace_id,
            plugin_id,
            config: None,
            created_at: now,
            updated_at: now,
        };

        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![] as Vec<workspace_plugin::Model>]) // SELECT → empty
            .append_query_results([vec![returned]]) //                        INSERT RETURNING
            .into_connection();
        let tx = db.begin().await.expect("begin");
        let mut stats = ApplyStats::default();

        upsert_binding(&tx, workspace_id, plugin_id, None, &mut stats)
            .await
            .expect("upsert_binding should succeed");

        assert_eq!(stats.bindings_inserted, 1);
        assert_eq!(stats.bindings_updated, 0);
    }

    #[tokio::test]
    async fn upsert_binding_update_branch_increments_updated_when_config_differs() {
        let now = chrono::Utc::now();
        let workspace_id = Uuid::new_v4();
        let plugin_id = Uuid::new_v4();
        let new_config = serde_json::json!({"url": "https://new.example.com"});
        let existing = workspace_plugin::Model {
            workspace_id,
            plugin_id,
            config: Some(serde_json::json!({"url": "https://old.example.com"})),
            created_at: now,
            updated_at: now,
        };
        let updated = workspace_plugin::Model {
            config: Some(new_config.clone()),
            ..existing.clone()
        };

        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![existing]]) // SELECT → existing (stale config)
            .append_query_results([vec![updated]]) //  UPDATE RETURNING → updated row
            .into_connection();
        let tx = db.begin().await.expect("begin");
        let mut stats = ApplyStats::default();

        upsert_binding(&tx, workspace_id, plugin_id, Some(&new_config), &mut stats)
            .await
            .expect("upsert_binding should succeed");

        assert_eq!(stats.bindings_inserted, 0);
        assert_eq!(stats.bindings_updated, 1);
    }

    #[tokio::test]
    async fn upsert_binding_no_op_when_config_unchanged() {
        let now = chrono::Utc::now();
        let workspace_id = Uuid::new_v4();
        let plugin_id = Uuid::new_v4();
        // Existing row has config=None; we pass config=None → no UPDATE issued.
        let existing = workspace_plugin::Model {
            workspace_id,
            plugin_id,
            config: None,
            created_at: now,
            updated_at: now,
        };

        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![existing]]) // SELECT → existing (same config)
            // No UPDATE result needed.
            .into_connection();
        let tx = db.begin().await.expect("begin");
        let mut stats = ApplyStats::default();

        upsert_binding(&tx, workspace_id, plugin_id, None, &mut stats)
            .await
            .expect("upsert_binding should succeed");

        assert_eq!(stats.bindings_inserted, 0);
        assert_eq!(stats.bindings_updated, 0);
    }

    // -----------------------------------------------------------------------
    // apply() full-path: stats accounting + UnknownPluginRef tripwire
    //
    // Note: `apply` wraps all upserts in a single db.begin()/commit().
    // MockDatabase handles begin/commit at protocol level (no result
    // consumed). All SELECT, INSERT RETURNING, and UPDATE RETURNING
    // statements consume from the query_results queue in the order they
    // are issued.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn apply_all_inserts_increments_all_stats() {
        let entry = sample_plugin_entry();
        let now = chrono::Utc::now();
        let plugin_id = Uuid::new_v4();
        let tenant_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();

        let plugin_row = plugin_model_matching(plugin_id, &entry);
        let tenant_row = tenant::Model {
            id: tenant_id,
            name: "phlax".to_string(),
            created_at: now,
            updated_at: now,
        };
        let workspace_row = workspace::Model {
            id: workspace_id,
            tenant_id,
            name: "mcp".to_string(),
            created_at: now,
            updated_at: now,
        };
        let binding_row = workspace_plugin::Model {
            workspace_id,
            plugin_id,
            config: None,
            created_at: now,
            updated_at: now,
        };

        let db = crate::test_support::mock_db_connection(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![] as Vec<plugin::Model>]) // plugin SELECT → empty
                .append_query_results([vec![plugin_row]]) //            plugin INSERT RETURNING
                .append_query_results([vec![] as Vec<tenant::Model>]) // tenant SELECT → empty
                .append_query_results([vec![tenant_row]]) //            tenant INSERT RETURNING
                .append_query_results([vec![] as Vec<workspace::Model>]) // workspace SELECT → empty
                .append_query_results([vec![workspace_row]]) //           workspace INSERT RETURNING
                .append_query_results([vec![] as Vec<workspace_plugin::Model>]) // binding SELECT → empty
                .append_query_results([vec![binding_row]]), //                   binding INSERT RETURNING
        );

        let stats = apply(&db, &sample_config())
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
        // All rows already exist and match → no inserts, no DB updates.
        // Tenant and workspace always increment _updated when found.
        // Plugin and binding only increment _updated when fields differ —
        // here they match, so those counters stay at zero.
        let entry = sample_plugin_entry();
        let now = chrono::Utc::now();
        let plugin_id = Uuid::new_v4();
        let tenant_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();

        let plugin_row = plugin_model_matching(plugin_id, &entry); // all fields match
        let tenant_row = tenant::Model {
            id: tenant_id,
            name: "phlax".to_string(),
            created_at: now,
            updated_at: now,
        };
        let workspace_row = workspace::Model {
            id: workspace_id,
            tenant_id,
            name: "mcp".to_string(),
            created_at: now,
            updated_at: now,
        };
        let binding_row = workspace_plugin::Model {
            workspace_id,
            plugin_id,
            config: None, // matches binding.config = None in sample_config
            created_at: now,
            updated_at: now,
        };

        let db = crate::test_support::mock_db_connection(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![plugin_row]]) //    plugin SELECT → existing (matching)
                // No plugin UPDATE (all fields match).
                .append_query_results([vec![tenant_row]]) //    tenant SELECT → existing
                // Tenant upsert has no UPDATE path.
                .append_query_results([vec![workspace_row]]) // workspace SELECT → existing
                // Workspace upsert has no UPDATE path.
                .append_query_results([vec![binding_row]]), //  binding SELECT → existing (config=None matches)
                                                            // No binding UPDATE (config unchanged).
        );

        let stats = apply(&db, &sample_config())
            .await
            .expect("apply should succeed");

        assert_eq!(stats.plugins_inserted, 0);
        assert_eq!(stats.plugins_updated, 0); // same image → no DB update
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
        // reference.  BootstrapConfig::from_raw would reject this, so
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

        let now = chrono::Utc::now();
        let tenant_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let tenant_row = tenant::Model {
            id: tenant_id,
            name: "test".to_string(),
            created_at: now,
            updated_at: now,
        };
        let workspace_row = workspace::Model {
            id: workspace_id,
            tenant_id,
            name: "ws".to_string(),
            created_at: now,
            updated_at: now,
        };

        let db = crate::test_support::mock_db_connection(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![] as Vec<tenant::Model>]) // tenant SELECT → empty
                .append_query_results([vec![tenant_row]]) //            tenant INSERT RETURNING
                .append_query_results([vec![] as Vec<workspace::Model>]) // workspace SELECT → empty
                .append_query_results([vec![workspace_row]]), //          workspace INSERT RETURNING
                                                              // The UnknownPluginRef error is raised before any binding DB query.
        );

        let err = apply(&db, &config)
            .await
            .expect_err("should fail with UnknownPluginRef");

        assert!(
            matches!(&err, BootstrapError::UnknownPluginRef { plugin, .. } if plugin == "ghost-plugin"),
            "expected UnknownPluginRef, got: {err:?}"
        );
    }
}
