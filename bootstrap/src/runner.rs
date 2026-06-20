//! Apply a parsed [`BootstrapConfig`] to a [`DatabaseConnection`].
//!
//! All operations are idempotent upserts. There is no delete path —
//! removing rows is admin-api territory, not boot-time territory.
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

use crate::config::BootstrapConfig;
use crate::error::BootstrapError;
use crate::plugin_spec::ValidatedPlugin;

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
