//! Diff-and-apply algorithm for `botwork-tools bootstrap`.
//!
//! ## What "apply" means
//!
//! Walk a [`BootstrapConfig`] (validated by admin-core) and bring
//! admin-api's state into agreement. Idempotent: re-running with an
//! unchanged yaml is a no-op (no POSTs, no PUTs, only the read-side
//! GETs).
//!
//! The algorithm is:
//!
//! 1. Walk `plugins[]`: list plugins via admin-api, then for each
//!    yaml entry either POST (new) or PUT (existing-but-changed).
//! 2. Walk `tenants[]`: list tenants, then for each yaml tenant
//!    POST-or-PUT (no PUT today — name is the join key, rename via
//!    yaml means add+delete which bootstrap never did; we PUT only
//!    when the row exists by name AND no other fields changed,
//!    which makes the round-trip a no-op verifying touch).
//! 3. Walk `tenants[].workspaces[]`: list workspaces filtered by
//!    tenant_id, POST-or-PUT.
//! 4. Walk `tenants[].workspaces[].plugins[]` (bindings): list
//!    workspace_plugins filtered by workspace_id, POST-or-PUT.
//!
//! Diff comparison is done on the comparable-field set per entity
//! (same shape `bootstrap/src/runner.rs` uses for sea-orm
//! upserts): name + content fields. `updated_at` round-trips
//! through `if_unmodified_since` only on PUTs so admin-api's
//! optimistic-lock check stays consistent — and the tool's GET-
//! before-PUT means the lock is fresh.
//!
//! There is intentionally NO DELETE path. Removing rows belongs
//! to the operator UI / explicit `botwork-tools admin delete`
//! subcommand, not boot-time tooling. That property is the whole
//! point of "bootstrap is throwaway": it adds + updates, never
//! takes away.

use std::collections::HashMap;

use botwork_admin_core::config::{
    BootstrapConfig, TenantEntry, WorkspaceEntry, WorkspacePluginEntry,
};
use botwork_admin_core::plugin_spec::ValidatedPlugin;
use serde_json::json;
use thiserror::Error;
use uuid::Uuid;

use crate::bootstrap::client::{
    AdminClient, ClientError, CreateTenant, CreateWorkspace, CreateWorkspacePlugin, Plugin, Tenant,
    UpdateTenant, UpdateWorkspace, UpdateWorkspacePlugin, Workspace, WorkspacePlugin,
};

/// Apply a validated bootstrap config through `client`.
///
/// `dry_run=true` plans the same diff but issues zero writes; only
/// the GETs run. Reports the same counts so operators can sanity-
/// check the planned change before flipping the switch.
pub fn apply(
    client: &AdminClient,
    config: &BootstrapConfig,
    dry_run: bool,
) -> Result<ApplyOutcome, ApplyError> {
    let mut outcome = ApplyOutcome::default();

    // -- plugins ----------------------------------------------------------
    let live_plugins: HashMap<String, Plugin> = client
        .list_plugins()?
        .into_iter()
        .map(|p| (p.name.clone(), p))
        .collect();
    let mut plugin_ids: HashMap<String, Uuid> = HashMap::new();
    for entry in &config.plugins {
        outcome.plugins_total += 1;
        match live_plugins.get(&entry.name) {
            Some(existing) => {
                plugin_ids.insert(entry.name.clone(), existing.id);
                if plugin_differs(existing, entry) {
                    if !dry_run {
                        let body = plugin_update_body(entry, existing.updated_at);
                        client.update_plugin(existing.id, &body)?;
                    }
                    outcome.plugins_updated += 1;
                }
            }
            None => {
                if dry_run {
                    plugin_ids.insert(entry.name.clone(), Uuid::nil());
                } else {
                    let body = plugin_create_body(entry);
                    let created = client.create_plugin(&body)?;
                    plugin_ids.insert(entry.name.clone(), created.id);
                }
                outcome.plugins_created += 1;
            }
        }
    }

    // -- tenants + workspaces + bindings ----------------------------------
    let live_tenants: HashMap<String, Tenant> = client
        .list_tenants()?
        .into_iter()
        .map(|t| (t.name.clone(), t))
        .collect();
    for tenant_entry in &config.tenants {
        outcome.tenants_total += 1;
        let (tenant_id, tenant_updated_at) = match live_tenants.get(&tenant_entry.name) {
            Some(existing) => (existing.id, existing.updated_at),
            None => {
                outcome.tenants_created += 1;
                if dry_run {
                    (Uuid::nil(), chrono::Utc::now())
                } else {
                    let created = client.create_tenant(&CreateTenant {
                        name: &tenant_entry.name,
                    })?;
                    (created.id, created.updated_at)
                }
            }
        };
        // Tenant has no comparable fields beyond name; the join key
        // is name. There's no PUT to issue when the tenant exists
        // and the name matches.
        let _ = tenant_updated_at;

        apply_workspaces(
            client,
            tenant_entry,
            tenant_id,
            &plugin_ids,
            dry_run,
            &mut outcome,
        )?;
    }

    Ok(outcome)
}

fn apply_workspaces(
    client: &AdminClient,
    tenant_entry: &TenantEntry,
    tenant_id: Uuid,
    plugin_ids: &HashMap<String, Uuid>,
    dry_run: bool,
    outcome: &mut ApplyOutcome,
) -> Result<(), ApplyError> {
    // Skip the GET if we just created the tenant in dry-run with a
    // nil id (no workspaces could possibly exist for it).
    let live_workspaces: HashMap<String, Workspace> = if tenant_id == Uuid::nil() {
        HashMap::new()
    } else {
        client
            .list_workspaces(tenant_id)?
            .into_iter()
            .map(|w| (w.name.clone(), w))
            .collect()
    };

    for workspace_entry in &tenant_entry.workspaces {
        outcome.workspaces_total += 1;
        let (workspace_id, _) = match live_workspaces.get(&workspace_entry.name) {
            Some(existing) => (existing.id, existing.updated_at),
            None => {
                outcome.workspaces_created += 1;
                if dry_run {
                    (Uuid::nil(), chrono::Utc::now())
                } else {
                    let created = client.create_workspace(&CreateWorkspace {
                        tenant_id,
                        name: &workspace_entry.name,
                    })?;
                    (created.id, created.updated_at)
                }
            }
        };

        apply_bindings(
            client,
            workspace_entry,
            workspace_id,
            plugin_ids,
            dry_run,
            outcome,
        )?;
    }
    Ok(())
}

fn apply_bindings(
    client: &AdminClient,
    workspace_entry: &WorkspaceEntry,
    workspace_id: Uuid,
    plugin_ids: &HashMap<String, Uuid>,
    dry_run: bool,
    outcome: &mut ApplyOutcome,
) -> Result<(), ApplyError> {
    let live_bindings: HashMap<Uuid, WorkspacePlugin> = if workspace_id == Uuid::nil() {
        HashMap::new()
    } else {
        client
            .list_workspace_plugins(workspace_id)?
            .into_iter()
            .map(|b| (b.plugin_id, b))
            .collect()
    };

    for binding in &workspace_entry.plugins {
        outcome.bindings_total += 1;
        let plugin_id = plugin_ids.get(&binding.name).copied().ok_or_else(|| {
            // Validation guarantees this can't happen — admin-core's
            // BootstrapConfig::from_raw fails on UnknownPluginRef. The
            // expect-shaped error is a tripwire.
            ApplyError::MissingPluginId(binding.name.clone())
        })?;
        match live_bindings.get(&plugin_id) {
            Some(existing) => {
                if binding_differs(existing, binding) {
                    if !dry_run {
                        client.update_workspace_plugin(
                            workspace_id,
                            plugin_id,
                            &UpdateWorkspacePlugin {
                                config: binding.config.clone(),
                                if_unmodified_since: existing.updated_at,
                            },
                        )?;
                    }
                    outcome.bindings_updated += 1;
                }
            }
            None => {
                outcome.bindings_created += 1;
                if !dry_run {
                    client.create_workspace_plugin(&CreateWorkspacePlugin {
                        workspace_id,
                        plugin_id,
                        config: binding.config.clone(),
                    })?;
                }
            }
        }
    }
    Ok(())
}

fn plugin_differs(existing: &Plugin, entry: &ValidatedPlugin) -> bool {
    existing.image != entry.image
        || existing.port != i32::from(entry.port)
        || existing.path != entry.path
        || existing.upstream_auth != entry.upstream_auth
        || existing.env != entry.env
        || existing.resources != entry.resources
        || existing.egress != entry.egress
}

fn plugin_create_body(entry: &ValidatedPlugin) -> serde_json::Value {
    // admin-api's POST /plugins body uses the raw plugin-entry shape
    // (admin-core::RawPluginEntry) because the validator runs again
    // on the API side. We round-trip our already-validated values
    // through the raw shape: name + image + port + path +
    // upstream_auth + env + resources + egress.
    let mut body = json!({
        "name": entry.name,
        "image": entry.image,
        "port": entry.port,
        "path": entry.path,
        "upstream_auth": entry.upstream_auth,
        "env": entry.env,
        "egress": entry.egress,
    });
    if let Some(resources) = &entry.resources {
        body["resources"] = resources.clone();
    }
    body
}

fn plugin_update_body(
    entry: &ValidatedPlugin,
    if_unmodified_since: chrono::DateTime<chrono::Utc>,
) -> serde_json::Value {
    let mut body = plugin_create_body(entry);
    body["if_unmodified_since"] = serde_json::Value::String(if_unmodified_since.to_rfc3339());
    body
}

fn binding_differs(existing: &WorkspacePlugin, entry: &WorkspacePluginEntry) -> bool {
    existing.config != entry.config
}

// Suppress dead-code warning on unused helpers; the tenant + workspace
// PUT paths exist for future "rename via PK by-id" support but aren't
// reachable from the current yaml shape (which uses name as the join
// key). Keeping them avoids a churn churn diff when that lands.
#[allow(dead_code)]
fn unused_tenant_update<'a>(
    name: &'a str,
    updated_at: chrono::DateTime<chrono::Utc>,
) -> UpdateTenant<'a> {
    UpdateTenant {
        name,
        if_unmodified_since: updated_at,
    }
}

#[allow(dead_code)]
fn unused_workspace_update<'a>(
    name: &'a str,
    updated_at: chrono::DateTime<chrono::Utc>,
) -> UpdateWorkspace<'a> {
    UpdateWorkspace {
        name,
        if_unmodified_since: updated_at,
    }
}

/// Mutation counts for the apply summary.
///
/// `_created` is "what the apply added"; `_updated` is "rows that
/// already existed but had different content". `_total` is the
/// yaml-side count — useful for "ok we saw this many entries in the
/// file" sanity checks.
#[derive(Debug, Default, Clone, Copy)]
pub struct ApplyOutcome {
    pub tenants_total: usize,
    pub tenants_created: usize,
    pub workspaces_total: usize,
    pub workspaces_created: usize,
    pub plugins_total: usize,
    pub plugins_created: usize,
    pub plugins_updated: usize,
    pub bindings_total: usize,
    pub bindings_created: usize,
    pub bindings_updated: usize,
}

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error(transparent)]
    Client(#[from] ClientError),

    /// Validation tripwire — admin-core's BootstrapConfig::from_raw
    /// should have caught any unknown plugin reference before we
    /// reached the apply phase. If this fires it means the validator
    /// missed a case.
    #[error("internal: plugin '{0}' referenced from a binding but no id was resolved")]
    MissingPluginId(String),
}
