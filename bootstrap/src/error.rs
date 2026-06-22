//! Errors for the bootstrap apply-to-database path.
//!
//! Surfaces the per-entry + list-level rules from `botwork-admin-core`
//! (1:1 mirror via `From<ValidationError>`) plus the sea-orm DB error
//! the writer produces. There is no file-IO / yaml-parse variant any
//! more — those used to belong to the boot-time binary's
//! `BootstrapConfig::load(path)` codepath, which retired in RFE #106
//! PR4 (botwork#118 / botwork#TBD). The caller now reads + validates
//! the yaml via `botwork_admin_core::BootstrapConfig::from_raw` and
//! converts the resulting `LoadError` itself if it needs to.
//!
//! Variants stay enumerated rather than collapsing into a single
//! `Validation(#[from] …)` arm so downstream consumers (admin-api
//! tests, config-broker tests, session-broker tests) can pattern-
//! match cleanly when they care.

use botwork_admin_core::ValidationError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BootstrapError {
    // -- Per-entry rules (1:1 with admin-core ValidationError) ------------
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
