//! Strongly-typed view of `bootstrap.yaml` and its load + validate path.
//!
//! The yaml shape is documented at the crate root. Validation here is
//! deliberately narrow: ensure every reference resolves locally (every
//! workspace.plugin name appears under top-level `plugins:`) and refuse
//! obvious garbage (empty names, duplicate names within a scope). Deep
//! schema validation of the `egress` block lives in config-broker (and
//! will keep living there post-cutover): bootstrap stores it as opaque
//! JSON the same way the DB does.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::BootstrapError;

/// Top-level shape: a list of tenants (each with its workspaces and
/// plugin bindings) plus a flat list of globally-named plugins.
///
/// This mirrors RFE #101's "plugins are global, workspaces reference
/// them" decision: the package definition lives once at the top level,
/// the per-(tenant, workspace) binding sits inside the tenant tree and
/// can carry an optional override config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapConfig {
    #[serde(default)]
    pub tenants: Vec<TenantEntry>,
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantEntry {
    pub name: String,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceEntry {
    pub name: String,
    #[serde(default)]
    pub plugins: Vec<WorkspacePluginEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePluginEntry {
    pub name: String,
    /// Optional per-binding config blob. Stored opaque; config-broker
    /// owns the schema (today via plugins.yaml's `config:` field).
    #[serde(default)]
    pub config: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginEntry {
    pub name: String,
    pub image: String,
    pub egress: serde_json::Value,
}

impl BootstrapConfig {
    /// Read and validate a `bootstrap.yaml` from disk.
    pub fn load(path: &Path) -> Result<Self, BootstrapError> {
        if !path.exists() {
            return Err(BootstrapError::ConfigNotFound(path.display().to_string()));
        }
        let bytes = std::fs::read_to_string(path).map_err(|err| BootstrapError::ConfigRead {
            path: path.display().to_string(),
            err,
        })?;
        let config: Self = serde_yaml::from_str(&bytes).map_err(BootstrapError::ConfigParse)?;
        config.validate()?;
        Ok(config)
    }

    /// Cross-reference + uniqueness checks. Returns the first violation;
    /// keeps the surface small because the production posture is "yaml
    /// is human-edited, fail loud on first issue".
    fn validate(&self) -> Result<(), BootstrapError> {
        // Top-level plugin name uniqueness (the DB unique index catches
        // this too, but we want the operator-facing error to point at the
        // yaml, not at a postgres constraint).
        let mut seen_plugin: HashSet<&str> = HashSet::new();
        for plugin in &self.plugins {
            check_name(&plugin.name, "plugins[].name")?;
            if !seen_plugin.insert(plugin.name.as_str()) {
                return Err(BootstrapError::DuplicatePlugin(plugin.name.clone()));
            }
            if plugin.image.trim().is_empty() {
                return Err(BootstrapError::PluginMissingImage(plugin.name.clone()));
            }
        }

        // Tenant + nested workspace uniqueness, and reference integrity:
        // every workspace_plugin.name must appear in self.plugins.
        let mut seen_tenant: HashSet<&str> = HashSet::new();
        for tenant in &self.tenants {
            check_name(&tenant.name, "tenants[].name")?;
            if !seen_tenant.insert(tenant.name.as_str()) {
                return Err(BootstrapError::DuplicateTenant(tenant.name.clone()));
            }

            let mut seen_workspace: HashSet<&str> = HashSet::new();
            for workspace in &tenant.workspaces {
                check_name(&workspace.name, "tenants[].workspaces[].name")?;
                if !seen_workspace.insert(workspace.name.as_str()) {
                    return Err(BootstrapError::DuplicateWorkspace {
                        tenant: tenant.name.clone(),
                        workspace: workspace.name.clone(),
                    });
                }

                let mut seen_binding: HashSet<&str> = HashSet::new();
                for binding in &workspace.plugins {
                    check_name(&binding.name, "tenants[].workspaces[].plugins[].name")?;
                    if !seen_binding.insert(binding.name.as_str()) {
                        return Err(BootstrapError::DuplicateBinding {
                            tenant: tenant.name.clone(),
                            workspace: workspace.name.clone(),
                            plugin: binding.name.clone(),
                        });
                    }
                    if !seen_plugin.contains(binding.name.as_str()) {
                        return Err(BootstrapError::UnknownPluginRef {
                            tenant: tenant.name.clone(),
                            workspace: workspace.name.clone(),
                            plugin: binding.name.clone(),
                        });
                    }
                }
            }
        }

        Ok(())
    }
}

/// Names are operator-facing slugs and identify rows by string. We don't
/// pin a regex in v0 — the DB columns are `text` — but we do reject
/// empties and pure-whitespace, which would otherwise round-trip as an
/// invisible identity.
fn check_name(name: &str, field: &'static str) -> Result<(), BootstrapError> {
    if name.trim().is_empty() {
        return Err(BootstrapError::EmptyName(field));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn yaml_to_file(yaml: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        f
    }

    #[test]
    fn round_trips_minimal_well_formed_yaml() {
        let f = yaml_to_file(
            r#"
tenants:
- name: phlax
  workspaces:
  - name: mcp
    plugins:
    - name: mcp-bash
    - name: mcp-fetch
      config:
        url: https://example.com

plugins:
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:1.0
  egress:
    mode: none
- name: mcp-fetch
  image: ghcr.io/example/mcp-fetch:1.0
  egress:
    allow:
    - host: example.com
      ports: [443]
"#,
        );

        let cfg = BootstrapConfig::load(f.path()).unwrap();
        assert_eq!(cfg.tenants.len(), 1);
        assert_eq!(cfg.tenants[0].workspaces[0].plugins[1].name, "mcp-fetch");
        assert_eq!(
            cfg.tenants[0].workspaces[0].plugins[1]
                .config
                .as_ref()
                .unwrap()["url"],
            "https://example.com"
        );
        assert_eq!(cfg.plugins.len(), 2);
    }

    #[test]
    fn fails_on_unknown_plugin_ref() {
        let f = yaml_to_file(
            r#"
tenants:
- name: phlax
  workspaces:
  - name: mcp
    plugins:
    - name: does-not-exist

plugins:
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:1.0
  egress: { mode: none }
"#,
        );

        let err = BootstrapConfig::load(f.path()).unwrap_err();
        assert!(matches!(err, BootstrapError::UnknownPluginRef { .. }));
    }

    #[test]
    fn fails_on_duplicate_workspace_in_tenant() {
        let f = yaml_to_file(
            r#"
tenants:
- name: phlax
  workspaces:
  - name: mcp
  - name: mcp

plugins: []
"#,
        );

        let err = BootstrapConfig::load(f.path()).unwrap_err();
        assert!(matches!(err, BootstrapError::DuplicateWorkspace { .. }));
    }

    #[test]
    fn allows_default_workspace_across_distinct_tenants() {
        // Two tenants, both with a `mcp` workspace. Per RFE #101 the
        // workspace unique key is (tenant, name), so this must parse +
        // validate cleanly even though `mcp` repeats.
        let f = yaml_to_file(
            r#"
tenants:
- name: phlax
  workspaces:
  - name: mcp
- name: ada
  workspaces:
  - name: mcp

plugins: []
"#,
        );

        BootstrapConfig::load(f.path()).unwrap();
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        // serde(deny_unknown_fields) belt-and-braces: a typo in the top
        // section becomes a load error rather than a silently dropped
        // configuration.
        let f = yaml_to_file(
            r#"
tenants: []
plugins: []
tennents: []   # typo
"#,
        );
        let err = BootstrapConfig::load(f.path()).unwrap_err();
        assert!(matches!(err, BootstrapError::ConfigParse(_)));
    }
}
