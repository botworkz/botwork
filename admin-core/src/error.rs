//! Structured errors emitted by every validator in this crate.
//!
//! Two production audiences:
//!
//! * `botwork-bootstrap` — lifts each variant into its own
//!   `BootstrapError` variant so the exit-code contract documented in
//!   `bootstrap/src/main.rs` stays intact (exit 5 on validation
//!   failure).
//! * `botwork-admin-api` — maps each variant into an HTTP 4xx
//!   response body that follows the shared envelope shape:
//!
//!   ```json
//!   { "error": "<machine code>", "message": "<human detail>" }
//!   ```
//!
//! * `botwork-tools bootstrap` — surfaces the variant verbatim through
//!   stderr; exit-code-mapping mirrors the bootstrap binary's so the
//!   systemd-callable interface stays uniform across writers.
//!
//! Variant set is small and intentional — adding one is a public-API
//! change because all three consumers key off the variant rather than
//! the `Display` text.

use thiserror::Error;

/// Validation errors for plugin specs, per-binding config blobs, and
/// the list-level rules that govern a full `bootstrap.yaml`.
///
/// `EmptyName` carries the field path (`plugins[].name`,
/// `tenants[].workspaces[].name`, …) so the operator can find the
/// offending entry. `PluginInvalid` and `BindingInvalid` carry the
/// plugin / binding identity plus the human-readable rule that fired.
///
/// The `Duplicate*` and `UnknownPluginRef` variants are list-level
/// rules — they fire while walking a full bootstrap tree
/// ([`crate::config::BootstrapConfig::from_raw`]) rather than from a
/// single-entry validator. They live in this enum (rather than a
/// separate "config error" enum) so the consumer crates only need to
/// reason about one validation result type.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    /// A required string field is blank after trim. Path is the
    /// dotted/bracketed field name (`"plugins[].name"`).
    #[error("empty {0}: must be a non-blank string")]
    EmptyName(&'static str),

    /// Plugin entry failed shape validation (image, port, path,
    /// upstream_auth, env, resources, egress, etc.). `detail` is the
    /// rule that fired.
    #[error("plugin '{plugin}': {detail}")]
    PluginInvalid { plugin: String, detail: String },

    /// Per-binding `config:` blob failed validation. `context` is e.g.
    /// `tenant 'phlax' workspace 'mcp' plugin 'mcp-fetch'`.
    #[error("{context}: {detail}")]
    BindingInvalid { context: String, detail: String },

    // -- list-level rules (only fired by config::BootstrapConfig::from_raw) -
    /// Two `plugins[]` entries share a name.
    #[error("duplicate plugin name in plugins[]: {0}")]
    DuplicatePlugin(String),

    /// Two `tenants[]` entries share a name.
    #[error("duplicate tenant name in tenants[]: {0}")]
    DuplicateTenant(String),

    /// Two workspaces under the same tenant share a name.
    #[error("duplicate workspace name in tenant '{tenant}': '{workspace}'")]
    DuplicateWorkspace { tenant: String, workspace: String },

    /// Two bindings under the same `(tenant, workspace)` reference
    /// the same plugin.
    #[error(
        "duplicate plugin binding under tenant '{tenant}' workspace '{workspace}': '{plugin}'"
    )]
    DuplicateBinding {
        tenant: String,
        workspace: String,
        plugin: String,
    },

    /// A binding references a plugin name that doesn't appear in the
    /// top-level `plugins[]` list. The whole tree must be
    /// self-contained.
    #[error(
        "tenant '{tenant}' workspace '{workspace}' references unknown plugin '{plugin}' — \
         every workspace_plugin.name must appear in top-level plugins[]"
    )]
    UnknownPluginRef {
        tenant: String,
        workspace: String,
        plugin: String,
    },
}
