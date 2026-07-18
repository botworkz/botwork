//! Errors for the bootstrap apply-to-database path.
//!
//! Surfaces the per-entry + list-level rules from `botwork-api-core`
//! (1:1 mirror via `From<ValidationError>`) plus the sea-orm DB error
//! the writer produces. There is no file-IO / yaml-parse variant any
//! more — those used to belong to the boot-time binary's
//! `BootstrapConfig::load(path)` codepath, which retired in RFE #106
//! PR4 (botwork#118 / botwork#TBD). The caller now reads + validates
//! the yaml via `botwork_api_core::BootstrapConfig::from_raw` and
//! converts the resulting `LoadError` itself if it needs to.
//!
//! Variants stay enumerated rather than collapsing into a single
//! `Validation(#[from] …)` arm so downstream consumers (api
//! tests, config-broker tests, session-broker tests) can pattern-
//! match cleanly when they care.

use botwork_api_core::ValidationError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BootstrapError {
    // -- Per-entry rules (1:1 with api-core ValidationError) ------------
    #[error("empty {0}: must be a non-blank string")]
    EmptyName(&'static str),

    #[error("duplicate plugin name in plugins[]: {0}")]
    DuplicatePlugin(String),

    #[error("duplicate tenant name in tenants[]: {0}")]
    DuplicateTenant(String),

    #[error("duplicate workspace name in tenant '{tenant}': '{workspace}'")]
    DuplicateWorkspace { tenant: String, workspace: String },

    #[error(
        "duplicate plugin binding under tenant '{tenant}' workspace '{workspace}': '{plugin}'"
    )]
    DuplicateBinding {
        tenant: String,
        workspace: String,
        plugin: String,
    },

    #[error(
        "tenant '{tenant}' workspace '{workspace}' references unknown plugin '{plugin}' — \
         every workspace_plugin.name must appear in top-level plugins[]"
    )]
    UnknownPluginRef {
        tenant: String,
        workspace: String,
        plugin: String,
    },

    #[error("plugin '{plugin}': {detail}")]
    PluginInvalid { plugin: String, detail: String },

    #[error("{context}: {detail}")]
    BindingInvalid { context: String, detail: String },

    /// Bootstrap-side tripwire for `PackageInvalid` — the
    /// `mcp-package.yaml`-only validation variant
    /// (`botwork-api-core::ValidationError::PackageInvalid`) that
    /// `validate_package` emits for the package-side-only rules
    /// (isolation, spill). Bootstrap reads `bootstrap.yaml`, not
    /// `mcp-package.yaml`, so the package-side validator is never on
    /// the reachable codepath here — but the exhaustiveness check on
    /// the `ValidationError -> BootstrapError` lowering forces us to
    /// name the variant or pattern-match it out. We name it, with a
    /// loud `internal` error rather than a wildcard `_` arm, so that
    /// if the impossibility ever stops being one (e.g. a future
    /// refactor routes a package-side check through bootstrap), the
    /// operator gets a precise bug report rather than a silent fallback.
    #[error(
        "internal: bootstrap reached package-side validator path for plugin \
         '{plugin}': {detail} (this is a bug; packages are validated by \
         mcp-probe, not bootstrap — see botworkz/botwork#147)"
    )]
    UnexpectedPackageValidation { plugin: String, detail: String },

    // -- DB-side errors ---------------------------------------------------
    #[error(transparent)]
    Db(#[from] sea_orm::DbErr),
}

impl From<ValidationError> for BootstrapError {
    fn from(err: ValidationError) -> Self {
        match err {
            ValidationError::EmptyName(path) => Self::EmptyName(path),
            ValidationError::PluginInvalid { plugin, detail } => {
                Self::PluginInvalid { plugin, detail }
            }
            ValidationError::BindingInvalid { context, detail } => {
                Self::BindingInvalid { context, detail }
            }
            ValidationError::PackageInvalid { plugin, detail } => {
                Self::UnexpectedPackageValidation { plugin, detail }
            }
            ValidationError::DuplicatePlugin(name) => Self::DuplicatePlugin(name),
            ValidationError::DuplicateTenant(name) => Self::DuplicateTenant(name),
            ValidationError::DuplicateWorkspace { tenant, workspace } => {
                Self::DuplicateWorkspace { tenant, workspace }
            }
            ValidationError::DuplicateBinding {
                tenant,
                workspace,
                plugin,
            } => Self::DuplicateBinding {
                tenant,
                workspace,
                plugin,
            },
            ValidationError::UnknownPluginRef {
                tenant,
                workspace,
                plugin,
            } => Self::UnknownPluginRef {
                tenant,
                workspace,
                plugin,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Display / Error impl -------------------------------------------------

    #[test]
    fn empty_name_display() {
        let err = BootstrapError::EmptyName("plugins[].name");
        assert_eq!(
            err.to_string(),
            "empty plugins[].name: must be a non-blank string"
        );
    }

    #[test]
    fn duplicate_plugin_display() {
        let err = BootstrapError::DuplicatePlugin("mcp-bash".into());
        assert_eq!(
            err.to_string(),
            "duplicate plugin name in plugins[]: mcp-bash"
        );
    }

    #[test]
    fn duplicate_tenant_display() {
        let err = BootstrapError::DuplicateTenant("phlax".into());
        assert_eq!(err.to_string(), "duplicate tenant name in tenants[]: phlax");
    }

    #[test]
    fn duplicate_workspace_display() {
        let err = BootstrapError::DuplicateWorkspace {
            tenant: "phlax".into(),
            workspace: "mcp".into(),
        };
        assert_eq!(
            err.to_string(),
            "duplicate workspace name in tenant 'phlax': 'mcp'"
        );
    }

    #[test]
    fn duplicate_binding_display() {
        let err = BootstrapError::DuplicateBinding {
            tenant: "phlax".into(),
            workspace: "mcp".into(),
            plugin: "mcp-bash".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("phlax"), "msg: {msg}");
        assert!(msg.contains("mcp-bash"), "msg: {msg}");
    }

    #[test]
    fn unknown_plugin_ref_display() {
        let err = BootstrapError::UnknownPluginRef {
            tenant: "phlax".into(),
            workspace: "mcp".into(),
            plugin: "ghost".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("phlax"), "msg: {msg}");
        assert!(msg.contains("mcp"), "msg: {msg}");
        assert!(msg.contains("ghost"), "msg: {msg}");
    }

    #[test]
    fn plugin_invalid_display() {
        let err = BootstrapError::PluginInvalid {
            plugin: "bad-plugin".into(),
            detail: "image is required".into(),
        };
        assert_eq!(err.to_string(), "plugin 'bad-plugin': image is required");
    }

    #[test]
    fn binding_invalid_display() {
        let err = BootstrapError::BindingInvalid {
            context: "tenant 'phlax' workspace 'mcp' plugin 'mcp-fetch'".into(),
            detail: "config too large".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("config too large"), "msg: {msg}");
        assert!(msg.contains("mcp-fetch"), "msg: {msg}");
    }

    #[test]
    fn unexpected_package_validation_display() {
        let err = BootstrapError::UnexpectedPackageValidation {
            plugin: "mcp-bash".into(),
            detail: "isolation required".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("internal"), "msg: {msg}");
        assert!(msg.contains("mcp-bash"), "msg: {msg}");
    }

    #[test]
    fn db_error_is_transparent() {
        let err = BootstrapError::Db(sea_orm::DbErr::Custom("db down".into()));
        assert!(err.to_string().contains("db down"));
    }

    // --- From<ValidationError> ------------------------------------------------

    #[test]
    fn from_validation_empty_name() {
        let be: BootstrapError = ValidationError::EmptyName("plugins[].name").into();
        assert!(matches!(be, BootstrapError::EmptyName("plugins[].name")));
    }

    #[test]
    fn from_validation_plugin_invalid() {
        let be: BootstrapError = ValidationError::PluginInvalid {
            plugin: "bad".into(),
            detail: "bad image".into(),
        }
        .into();
        assert!(
            matches!(be, BootstrapError::PluginInvalid { ref plugin, ref detail }
                if plugin == "bad" && detail == "bad image")
        );
    }

    #[test]
    fn from_validation_binding_invalid() {
        let be: BootstrapError = ValidationError::BindingInvalid {
            context: "ctx".into(),
            detail: "too big".into(),
        }
        .into();
        assert!(
            matches!(be, BootstrapError::BindingInvalid { ref context, ref detail }
                if context == "ctx" && detail == "too big")
        );
    }

    #[test]
    fn from_validation_package_invalid_becomes_unexpected() {
        let be: BootstrapError = ValidationError::PackageInvalid {
            plugin: "mcp-bash".into(),
            detail: "isolation required".into(),
        }
        .into();
        assert!(
            matches!(be, BootstrapError::UnexpectedPackageValidation { ref plugin, .. }
                if plugin == "mcp-bash")
        );
    }

    #[test]
    fn from_validation_duplicate_plugin() {
        let be: BootstrapError = ValidationError::DuplicatePlugin("mcp-bash".into()).into();
        assert!(matches!(be, BootstrapError::DuplicatePlugin(ref n) if n == "mcp-bash"));
    }

    #[test]
    fn from_validation_duplicate_tenant() {
        let be: BootstrapError = ValidationError::DuplicateTenant("phlax".into()).into();
        assert!(matches!(be, BootstrapError::DuplicateTenant(ref n) if n == "phlax"));
    }

    #[test]
    fn from_validation_duplicate_workspace() {
        let be: BootstrapError = ValidationError::DuplicateWorkspace {
            tenant: "t".into(),
            workspace: "w".into(),
        }
        .into();
        assert!(
            matches!(be, BootstrapError::DuplicateWorkspace { ref tenant, ref workspace }
                if tenant == "t" && workspace == "w")
        );
    }

    #[test]
    fn from_validation_duplicate_binding() {
        let be: BootstrapError = ValidationError::DuplicateBinding {
            tenant: "t".into(),
            workspace: "w".into(),
            plugin: "p".into(),
        }
        .into();
        assert!(
            matches!(be, BootstrapError::DuplicateBinding { ref tenant, ref workspace, ref plugin }
                if tenant == "t" && workspace == "w" && plugin == "p")
        );
    }

    #[test]
    fn from_validation_unknown_plugin_ref() {
        let be: BootstrapError = ValidationError::UnknownPluginRef {
            tenant: "t".into(),
            workspace: "w".into(),
            plugin: "p".into(),
        }
        .into();
        assert!(
            matches!(be, BootstrapError::UnknownPluginRef { ref tenant, ref workspace, ref plugin }
                if tenant == "t" && workspace == "w" && plugin == "p")
        );
    }
}
