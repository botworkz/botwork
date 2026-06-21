//! Structured errors emitted by every validator in this crate.
//!
//! Two production audiences:
//!
//! * `botwork-bootstrap` — lifts each variant into its own
//!   `BootstrapError` variant so the exit-code contract documented in
//!   `bootstrap/src/main.rs` stays intact (exit 5 on validation
//!   failure).
//! * `botwork-admin-api` — maps each variant into an HTTP 400 / 409
//!   response body that follows the shared envelope shape:
//!
//!   ```json
//!   { "error": "<machine code>", "message": "<human detail>" }
//!   ```
//!
//! Variant set is small and intentional — adding one is a public-API
//! change because both consumers key off the variant rather than the
//! `Display` text.

use thiserror::Error;

/// Validation errors for plugin specs + per-binding config blobs.
///
/// The `EmptyName` variant carries the field path (`plugins[].name`,
/// `tenants[].workspaces[].name`, …) so the operator can find the
/// offending entry even in a 100-row registry. `PluginInvalid` and
/// `BindingInvalid` carry the plugin / binding identity plus the
/// human-readable rule that fired.
///
/// Note: shape-level errors (duplicate names, unknown plugin
/// references in bindings) are NOT modelled here because they're
/// caller-driven — `botwork-bootstrap` enforces them while traversing
/// its yaml tree, and admin-api enforces them per-request against the
/// live DB. This crate only owns the *per-entry* rules.
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
}
