//! Yaml-shape parser + list-level validation for the bootstrap config.
//!
//! Lifted from `botwork-bootstrap/src/config.rs` so the same shape can
//! be parsed by `botctl` (which talks to api over HTTP)
//! without depending on the bootstrap crate (which drags sea-orm and
//! tokio multi-thread into anything that links it).
//!
//! The yaml shape (unchanged from the bootstrap copy):
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
//! # Scope split with [`crate::plugin_spec`]
//!
//! Per-entry plugin / binding rules live in [`crate::plugin_spec`] and
//! emit a [`crate::ValidationError`]. List-level rules — duplicate
//! tenant/workspace/plugin names, unknown plugin references from
//! bindings — live HERE because they're the caller's tree walk.
//! `from_raw` calls into `plugin_spec` for per-entry validation and
//! catches list-level issues in its own pass.
//!
//! # Stability
//!
//! This is the production yaml contract. The shape is `#[serde(deny_unknown_fields)]`
//! at every level so a typo in a field name is a parse failure, not a
//! silently-dropped field. Same posture api's write bodies use.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::ValidationError;
use crate::plugin_spec::{
    validate_one, validate_workspace_plugin_config, RawPluginEntry, ValidatedPlugin,
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
    /// [`validate_workspace_plugin_config`] against the same size cap
    /// as static env values.
    #[serde(default)]
    pub config: Option<serde_yaml::Value>,
}

/// Fully-validated bootstrap config, ready to apply.
///
/// What "ready to apply" means depends on the consumer:
/// * `botwork-bootstrap` runs a sea-orm transaction that upserts each
///   row.
/// * `botctl bootstrap` walks the tree and translates each
///   entry into POST/PUT calls against api.
///
/// The validated shape is the same; only the write path differs.
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

/// Errors specific to loading/parsing a bootstrap file from disk.
///
/// These are NOT validation errors (which use [`ValidationError`]) —
/// they're filesystem / yaml-parse failures that exist before
/// validation runs.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("bootstrap config not found: {0}")]
    NotFound(String),

    #[error("failed to read bootstrap config {path}: {err}")]
    Read {
        path: String,
        #[source]
        err: std::io::Error,
    },

    #[error("failed to parse bootstrap config: {0}")]
    Parse(#[from] serde_yaml::Error),

    #[error(transparent)]
    Validation(#[from] ValidationError),
}

impl BootstrapConfig {
    /// Read and validate a `bootstrap.yaml` from disk.
    pub fn load(path: &Path) -> Result<Self, LoadError> {
        if !path.exists() {
            return Err(LoadError::NotFound(path.display().to_string()));
        }
        let bytes = std::fs::read_to_string(path).map_err(|err| LoadError::Read {
            path: path.display().to_string(),
            err,
        })?;
        let raw: BootstrapConfigRaw = serde_yaml::from_str(&bytes)?;
        Ok(Self::from_raw(raw)?)
    }

    /// Lower a fully-parsed raw config into a fully-validated one.
    /// Pulled out of [`Self::load`] so tests can construct a
    /// `BootstrapConfigRaw` directly without going through disk.
    pub fn from_raw(raw: BootstrapConfigRaw) -> Result<Self, ValidationError> {
        // Per-entry validation lives in plugin_spec; list-level
        // duplicate / dangling-ref detection lives here.
        let mut plugins = Vec::with_capacity(raw.plugins.len());
        let mut seen_plugin: HashSet<&str> = HashSet::new();
        for entry in &raw.plugins {
            let validated = validate_one(entry)?;
            if !seen_plugin.insert(entry.name.as_str()) {
                return Err(ValidationError::DuplicatePlugin(entry.name.clone()));
            }
            plugins.push(validated);
        }
        let plugin_names: HashSet<&str> = plugins.iter().map(|p| p.name.as_str()).collect();

        let mut tenants = Vec::with_capacity(raw.tenants.len());
        let mut seen_tenant: HashSet<&str> = HashSet::new();
        for tenant in &raw.tenants {
            let tenant_name = tenant.name.trim().to_string();
            if tenant_name.is_empty() {
                return Err(ValidationError::EmptyName("tenants[].name"));
            }
            if !seen_tenant.insert(tenant.name.as_str()) {
                return Err(ValidationError::DuplicateTenant(tenant_name));
            }

            let mut workspaces = Vec::with_capacity(tenant.workspaces.len());
            let mut seen_workspace: HashSet<&str> = HashSet::new();
            for workspace in &tenant.workspaces {
                let ws_name = workspace.name.trim().to_string();
                if ws_name.is_empty() {
                    return Err(ValidationError::EmptyName("tenants[].workspaces[].name"));
                }
                if !seen_workspace.insert(workspace.name.as_str()) {
                    return Err(ValidationError::DuplicateWorkspace {
                        tenant: tenant_name.clone(),
                        workspace: ws_name,
                    });
                }

                let mut bindings = Vec::with_capacity(workspace.plugins.len());
                let mut seen_binding: HashSet<&str> = HashSet::new();
                for binding in &workspace.plugins {
                    let binding_name = binding.name.trim().to_string();
                    if binding_name.is_empty() {
                        return Err(ValidationError::EmptyName(
                            "tenants[].workspaces[].plugins[].name",
                        ));
                    }
                    if !seen_binding.insert(binding.name.as_str()) {
                        return Err(ValidationError::DuplicateBinding {
                            tenant: tenant_name.clone(),
                            workspace: ws_name.clone(),
                            plugin: binding_name,
                        });
                    }
                    if !plugin_names.contains(binding.name.as_str()) {
                        return Err(ValidationError::UnknownPluginRef {
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
    use std::time::{SystemTime, UNIX_EPOCH};

    fn parse(yaml: &str) -> Result<BootstrapConfig, ValidationError> {
        let raw: BootstrapConfigRaw = serde_yaml::from_str(yaml).expect("yaml parse");
        BootstrapConfig::from_raw(raw)
    }

    #[test]
    fn round_trips_minimal_well_formed_yaml() {
        let cfg = parse(
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
        )
        .expect("validate");
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
        let bash = cfg.plugins.iter().find(|p| p.name == "mcp-bash").unwrap();
        assert_eq!(bash.port, 8000);
        assert_eq!(bash.path, "/");
        assert_eq!(bash.upstream_auth, "none");
        assert_eq!(bash.egress, serde_json::json!({"mode": "none"}));
    }

    #[test]
    fn fails_on_unknown_plugin_ref() {
        let err = parse(
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
        )
        .unwrap_err();
        assert!(matches!(err, ValidationError::UnknownPluginRef { .. }));
    }

    #[test]
    fn fails_on_duplicate_workspace_in_tenant() {
        let err = parse(
            r#"
tenants:
- name: phlax
  workspaces:
  - name: mcp
  - name: mcp

plugins: []
"#,
        )
        .unwrap_err();
        assert!(matches!(err, ValidationError::DuplicateWorkspace { .. }));
    }

    #[test]
    fn allows_default_workspace_across_distinct_tenants() {
        parse(
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
        )
        .expect("validate");
    }

    #[test]
    fn fails_on_missing_egress_in_plugin() {
        let err = parse(
            r#"
tenants: []
plugins:
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:1.0
"#,
        )
        .unwrap_err();
        assert!(matches!(err, ValidationError::PluginInvalid { .. }));
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
        let err = parse(&yaml).unwrap_err();
        assert!(matches!(err, ValidationError::BindingInvalid { .. }));
    }

    #[test]
    fn fails_on_duplicate_plugin_name() {
        let err = parse(
            r#"
tenants: []
plugins:
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:1.0
  egress: none
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:2.0
  egress: all
"#,
        )
        .unwrap_err();
        assert!(matches!(err, ValidationError::DuplicatePlugin(_)));
    }

    #[test]
    fn fails_on_duplicate_tenant_name() {
        let err = parse(
            r#"
tenants:
- name: phlax
- name: phlax

plugins: []
"#,
        )
        .unwrap_err();
        assert!(matches!(err, ValidationError::DuplicateTenant(_)));
    }

    #[test]
    fn fails_on_duplicate_binding_within_workspace() {
        let err = parse(
            r#"
tenants:
- name: phlax
  workspaces:
  - name: mcp
    plugins:
    - name: mcp-bash
    - name: mcp-bash

plugins:
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:1.0
  egress: none
"#,
        )
        .unwrap_err();
        assert!(matches!(err, ValidationError::DuplicateBinding { .. }));
    }

    #[test]
    fn fails_on_empty_names_across_tree_levels() {
        for yaml in [
            "tenants:\n- name: \" \"\nplugins: []\n",
            "tenants:\n- name: t\n  workspaces:\n  - name: \"\"\nplugins: []\n",
            "tenants:\n- name: t\n  workspaces:\n  - name: w\n    plugins:\n    - name: \"\"\nplugins:\n- name: p\n  image: ghcr.io/example/p:1.0\n  egress: none\n",
        ] {
            let err = parse(yaml).unwrap_err();
            assert!(matches!(err, ValidationError::EmptyName(_)), "{yaml}");
        }
    }

    #[test]
    fn load_reports_not_found_parse_read_and_validation_failures() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("epoch")
            .as_nanos();
        let base = std::env::temp_dir().join(format!("botwork-api-core-config-tests-{unique}"));
        std::fs::create_dir_all(&base).expect("create temp base");
        let missing = base.join("missing.yaml");
        let err = BootstrapConfig::load(&missing).unwrap_err();
        assert!(matches!(err, LoadError::NotFound(_)));

        let parse_path = base.join("parse.yaml");
        std::fs::write(&parse_path, "tenants: [").expect("write parse fixture");
        let err = BootstrapConfig::load(&parse_path).unwrap_err();
        assert!(matches!(err, LoadError::Parse(_)));

        let invalid_path = base.join("invalid.yaml");
        std::fs::write(
            &invalid_path,
            "tenants: []\nplugins:\n- name: p\n  image: ghcr.io/example/p:1.0\n",
        )
        .expect("write invalid fixture");
        let err = BootstrapConfig::load(&invalid_path).unwrap_err();
        assert!(matches!(err, LoadError::Validation(_)));

        let dir_path = base.join("dir-as-file");
        std::fs::create_dir_all(&dir_path).expect("make dir");
        let err = BootstrapConfig::load(&dir_path).unwrap_err();
        assert!(matches!(err, LoadError::Read { .. }));

        let _ = std::fs::remove_dir_all(&base);
    }
}
