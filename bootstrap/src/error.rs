//! Structured errors for the bootstrap binary.
//!
//! Two audiences: operators (who see them in `journalctl -u
//! botwork-bootstrap.service`) and CI (which keys exit codes off the
//! variant in `src/main.rs`). Keep the variant set small and stable;
//! the exit-code mapping is part of the systemd contract.
//!
//! All shape / per-entry / list-level validation lives in
//! `botwork-admin-core` (which also fronts admin-api and
//! `botwork-tools bootstrap`). This file owns the bootstrap-specific
//! errors only: file IO, sea-orm DB failures, and the lift from
//! `ValidationError` into this enum's flat variants so the exit-code
//! switch in `main.rs` keeps discriminating cleanly.

use botwork_admin_core::ValidationError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BootstrapError {
    // -- Config-side errors -----------------------------------------------
    #[error("bootstrap config not found: {0}")]
    ConfigNotFound(String),

    #[error("failed to read bootstrap config {path}: {err}")]
    ConfigRead {
        path: String,
        #[source]
        err: std::io::Error,
    },

    #[error("failed to parse bootstrap config: {0}")]
    ConfigParse(#[from] serde_yaml::Error),

    // -- Per-entry rules (also emitted by admin-core::ValidationError) ----
    //
    // These mirror `ValidationError` 1:1. They stay enumerated here
    // (rather than collapsing into a single `Validation(#[from] …)`
    // arm) so the systemd exit-code mapping in main.rs continues to
    // discriminate between "operator typo in a field" and "operator
    // wrote a duplicate row" without pattern-matching through a nested
    // enum on a critical path. The `From<ValidationError>` impl below
    // does the lift.
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

    /// Plugin spec failed shape validation (image, port, path,
    /// upstream_auth, env, resources, egress, etc.). The `detail` is
    /// the human-readable rule that fired — comes from
    /// `botwork-admin-core`. Carrying the plugin name + detail
    /// rather than a free-form string keeps logs greppable.
    #[error("plugin '{plugin}': {detail}")]
    PluginInvalid { plugin: String, detail: String },

    /// Per-binding `config:` blob failed validation. Same shape as
    /// `PluginInvalid` but for `tenants[].workspaces[].plugins[].config`.
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

impl From<botwork_admin_core::config::LoadError> for BootstrapError {
    fn from(err: botwork_admin_core::config::LoadError) -> Self {
        use botwork_admin_core::config::LoadError as L;
        match err {
            L::NotFound(p) => Self::ConfigNotFound(p),
            L::Read { path, err } => Self::ConfigRead { path, err },
            L::Parse(e) => Self::ConfigParse(e),
            L::Validation(v) => v.into(),
        }
    }
}
