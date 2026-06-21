//! `botwork-admin-core` — write-side validators for the persistence layer.
//!
//! Shared between `botwork-bootstrap` (today's boot-time writer) and
//! `botwork-admin-api` (the HTTP+JSON writer that replaces it under
//! RFE #106). The two writers are structurally different but the
//! "what makes a plugin / binding spec valid" question has exactly
//! one answer; this crate holds it.
//!
//! # What lives here
//!
//! * [`error::ValidationError`] — structured errors for every rule
//!   the validators enforce. Carries the offending field path and a
//!   human-readable detail; the bootstrap binary lifts these into
//!   `BootstrapError::PluginInvalid` / `BindingInvalid`, and admin-api
//!   maps them into HTTP 400/409 response bodies.
//! * [`plugin_spec`] — full plugin-spec validation lifted from the
//!   pre-cutover `config-broker/src/registry.rs` (and the bootstrap
//!   crate's pre-extraction copy). Same rules, same constants, same
//!   compatibility surface — see the module docs for the exact list.
//!
//! # What does NOT live here
//!
//! * SeaORM entity types — the admin-core crate stays DB-agnostic so
//!   it can be consumed by tests / future tooling that don't link
//!   sea-orm. Conversions live in the consumer crates.
//! * Apply / upsert logic — that's `botwork-bootstrap::runner` today
//!   and `botwork-admin-api` tomorrow.
//! * The yaml-shape `BootstrapConfig` struct — that's bootstrap-only
//!   (it models the on-disk file shape, not the validation rules).
//! * List-level rules: duplicate-name detection, unknown-plugin
//!   references in bindings. Those are caller-driven —
//!   bootstrap enforces them while walking its yaml tree, admin-api
//!   enforces them per-request against the live DB. This crate only
//!   owns the *per-entry* rules.
//!
//! # Stability
//!
//! The constants exported here (env-name caps, reserved prefixes,
//! the plugin-name regex) are part of the contract with
//! `launcher/src/validate.rs`. Any change must land here AND there.

pub mod error;
pub mod plugin_spec;

pub use error::ValidationError;
pub use plugin_spec::{
    validate_one, validate_workspace_plugin_config, RawPluginEntry, ValidatedPlugin,
    CONFIG_ENV_NAME, MAX_ENV_VALUE_BYTES, MAX_STATIC_ENV_ENTRIES, PLUGIN_NAME_RE,
    RESERVED_ENV_NAMES, SECRET_ENV_PREFIX,
};
