//! Structured errors for the bootstrap binary.
//!
//! Two audiences: operators (who see them in `journalctl -u
//! botwork-bootstrap.service`) and CI (which keys exit codes off the
//! variant in `src/main.rs`). Keep the variant set small and stable;
//! the exit-code mapping is part of the systemd contract.

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
    /// `plugin_spec::validate_*`. Carrying the plugin name + detail
    /// rather than a free-form string keeps logs greppable.
    #[error("plugin '{plugin}': {detail}")]
    PluginInvalid { plugin: String, detail: String },

    /// Per-binding `config:` blob failed validation (non-mapping,
    /// oversized, etc.). `context` is e.g.
    /// `tenant 'phlax' workspace 'mcp' plugin 'mcp-fetch'` so the
    /// operator can find it.
    #[error("{context}: {detail}")]
    BindingInvalid { context: String, detail: String },

    // -- Runtime-side errors ----------------------------------------------
    #[error("connection failed: {0}")]
    Connect(#[from] botwork_entity::ConnectError),

    /// Generic DB error from SeaORM. We don't try to discriminate further
    /// in v0; the wrapping `journalctl` line already names the operation.
    #[error("database error: {0}")]
    Db(#[from] sea_orm::DbErr),
}
