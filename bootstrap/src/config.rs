//! Strongly-typed view of `bootstrap.yaml` and its load + validate path.
//!
//! The yaml shape (post-PR2):
//!
//! ```yaml
//! tenants:
//! - name: phlax
//!   workspaces:
//!   - name: mcp
//!     plugins:
//!     - name: mcp-bash
//!     - name: mcp-fetch
//!       config:
//!         url: https://example.com
//!
//! plugins:
//! - name: mcp-bash
//!   image: ghcr.io/.../mcp-bash@sha256:...
//!   egress: none
//! - name: mcp-fetch
//!   image: ghcr.io/.../mcp-fetch@sha256:...
//!   port: 8000
//!   path: /
//!   upstream_auth: bearer/github.com
//!   env:
//!     LOG_LEVEL: info
//!   resources:
//!     memory: 4g
//!     pids: 1024
//!   egress:
//!     allow:
//!     - host: example.com
//!       ports: [443]
//! ```
//!
//! Top-level plugin entries carry the full set of fields that the
//! config-broker `/resolve` wire shape exposes; see
//! [`plugin_spec`] for the validation rules. Per-binding `config:`
//! lives under `tenants[].workspaces[].plugins[].config` and is
//! validated separately.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::BootstrapError;
use crate::plugin_spec::{
    validate_all, validate_workspace_plugin_config, RawPluginEntry, ValidatedPlugin,
};

/// Top-level shape: a list of tenants (each with its workspaces and
/// plugin bindings) plus a flat list of globally-named plugins.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapConfigRaw {
    #[serde(default)]
    pub tenants: Vec<TenantRaw>,
    #[serde(default)]
    pub plugins: Vec<RawPluginEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantRaw {
    pub name: String,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceRaw>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRaw {
    pub name: String,
    #[serde(default)]
    pub plugins: Vec<WorkspacePluginRaw>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePluginRaw {
    pub name: String,
    /// Optional per-binding config blob (YAML mapping). Validated by
    /// `plugin_spec::validate_workspace_plugin_config` against the
    /// same size cap as static env values.
    #[serde(default)]
    pub config: Option<serde_yaml::Value>,
}

/// Fully-validated bootstrap config, ready to apply to the DB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapConfig {
    pub tenants: Vec<TenantEntry>,
    pub plugins: Vec<ValidatedPlugin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantEntry {
    pub name: String,
    pub workspaces: Vec<WorkspaceEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    pub name: String,
    pub plugins: Vec<WorkspacePluginEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePluginEntry {
    pub name: String,
    pub config: Option<serde_json::Value>,
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
        let raw: BootstrapConfigRaw =
            serde_yaml::from_str(&bytes).map_err(BootstrapError::ConfigParse)?;
        Self::from_raw(raw)
    }

    /// Lower a fully-parsed raw config into a fully-validated one.
    /// Pulled out of `load` so tests can construct `BootstrapConfigRaw`
    /// directly without going through the on-disk path.
    pub fn from_raw(raw: BootstrapConfigRaw) -> Result<Self, BootstrapError> {
        let plugins = validate_all(&raw.plugins)?;
        let plugin_names: HashSet<&str> = plugins.iter().map(|p| p.name.as_str()).collect();

        let mut tenants = Vec::with_capacity(raw.tenants.len());
        let mut seen_tenant: HashSet<&str> = HashSet::new();
        for tenant in &raw.tenants {
            let tenant_name = tenant.name.trim().to_string();
            if tenant_name.is_empty() {
                return Err(BootstrapError::EmptyName("tenants[].name"));
            }
            if !seen_tenant.insert(tenant.name.as_str()) {
                return Err(BootstrapError::DuplicateTenant(tenant_name));
            }

            let mut workspaces = Vec::with_capacity(tenant.workspaces.len());
            let mut seen_workspace: HashSet<&str> = HashSet::new();
            for workspace in &tenant.workspaces {
                let ws_name = workspace.name.trim().to_string();
                if ws_name.is_empty() {
                    return Err(BootstrapError::EmptyName("tenants[].workspaces[].name"));
                }
                if !seen_workspace.insert(workspace.name.as_str()) {
                    return Err(BootstrapError::DuplicateWorkspace {
                        tenant: tenant_name.clone(),
                        workspace: ws_name,
                    });
                }

                let mut bindings = Vec::with_capacity(workspace.plugins.len());
                let mut seen_binding: HashSet<&str> = HashSet::new();
                for binding in &workspace.plugins {
                    let binding_name = binding.name.trim().to_string();
                    if binding_name.is_empty() {
                        return Err(BootstrapError::EmptyName(
                            "tenants[].workspaces[].plugins[].name",
                        ));
                    }
                    if !seen_binding.insert(binding.name.as_str()) {
                        return Err(BootstrapError::DuplicateBinding {
                            tenant: tenant_name.clone(),
                            workspace: ws_name.clone(),
                            plugin: binding_name,
                        });
                    }
                    if !plugin_names.contains(binding.name.as_str()) {
                        return Err(BootstrapError::UnknownPluginRef {
                            tenant: tenant_name.clone(),
                            workspace: ws_name.clone(),
                            plugin: binding_name,
                        });
                    }
                    let binding_ctx = format!(
                        "tenant '{tenant_name}' workspace '{ws_name}' plugin '{binding_name}'"
                    );
                    let config =
                        validate_workspace_plugin_config(&binding_ctx, binding.config.as_ref())?;
                    bindings.push(WorkspacePluginEntry {
                        name: binding_name,
                        config,
                    });
                }
                workspaces.push(WorkspaceEntry {
                    name: ws_name,
                    plugins: bindings,
                });
            }
            tenants.push(TenantEntry {
                name: tenant_name,
                workspaces,
            });
        }

        Ok(Self { tenants, plugins })
    }
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
  egress: none
- name: mcp-fetch
  image: ghcr.io/example/mcp-fetch:1.0
  egress:
    allow:
    - host: example.com
      ports: [443]
"#,
        );

        let cfg = BootstrapConfig::load(f.path()).expect("load");
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
        // Defaults filled in.
        let bash = cfg.plugins.iter().find(|p| p.name == "mcp-bash").unwrap();
        assert_eq!(bash.port, 8000);
        assert_eq!(bash.path, "/");
        assert_eq!(bash.upstream_auth, "none");
        assert_eq!(bash.egress, serde_json::json!({"mode": "none"}));
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
  egress: none
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
        // Two tenants, both with a `mcp` workspace.
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

        BootstrapConfig::load(f.path()).expect("validate");
    }

    #[test]
    fn rejects_unknown_top_level_field() {
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

    #[test]
    fn fails_on_missing_egress_in_plugin() {
        let f = yaml_to_file(
            r#"
tenants: []
plugins:
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:1.0
"#,
        );
        let err = BootstrapConfig::load(f.path()).unwrap_err();
        assert!(matches!(err, BootstrapError::PluginInvalid { .. }));
    }

    #[test]
    fn binding_config_size_limit_enforced() {
        let big = "x".repeat(70 * 1024);
        let yaml = format!(
            r#"
tenants:
- name: phlax
  workspaces:
  - name: mcp
    plugins:
    - name: mcp-bash
      config:
        big: "{big}"

plugins:
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:1.0
  egress: none
"#
        );
        let f = yaml_to_file(&yaml);
        let err = BootstrapConfig::load(f.path()).unwrap_err();
        assert!(matches!(err, BootstrapError::BindingInvalid { .. }));
    }
}
