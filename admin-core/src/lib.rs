//! `botwork-admin-core` — write-side validators + bootstrap-config shape.
//!
//! Shared between the three writers in the workspace:
//!
//! * `botwork-bootstrap` — boot-time oneshot that upserts via sea-orm.
//!   Retired by RFE #106 PR4 (botwork#TBD); the crate stays available
//!   during the cutover so its tests can keep using these types.
//! * `botwork-admin-api` — HTTP+JSON writer (RFE #106 PR3) consumed by
//!   the operator UI and `botwork-tools bootstrap`.
//! * `botwork-tools bootstrap` — operator-facing import subcommand;
//!   parses the same yaml shape and POSTs through admin-api.
//!
//! The three writers are structurally different but the answer to
//! "what makes a plugin/binding/tree spec valid" has exactly one
//! answer; this crate holds it.
//!
//! # What lives here
//!
//! * [`error::ValidationError`] — structured errors for every rule
//!   the validators enforce, including the list-level
//!   (`Duplicate*` / `UnknownPluginRef`) variants.
//! * [`plugin_spec`] — per-entry plugin-spec validation: image, port,
//!   path, upstream_auth, env, resources, egress.
//! * [`config`] — yaml-shape parser + list-level tree validation
//!   (`BootstrapConfig` / `BootstrapConfigRaw`). Lifted out of the
//!   `botwork-bootstrap` crate so consumers don't need to depend on
//!   bootstrap's runtime stack (sea-orm, multi-thread tokio) just to
//!   parse a config file.
//! * [`package`] — `mcp-package.yaml` parser + validator consumed by
//!   `botwork-tools mcp-probe`. Shares per-field rules with
//!   [`plugin_spec`] (image-less plugin entry + `isolation` + `spill`)
//!   so the producer-side rules and the consumer-side rules can't
//!   drift apart.
//!
//! # What does NOT live here
//!
//! * SeaORM entity types — the admin-core crate stays DB-agnostic so
//!   it can be consumed by tests / tooling that don't link sea-orm.
//!   Conversions live in the consumer crates.
//! * Apply / upsert logic — `botwork-bootstrap::runner` (DB-side
//!   sea-orm txn) and `botwork-tools::bootstrap` (HTTP POSTs through
//!   admin-api) each own their own write path.
//!
//! # Stability
//!
//! The constants exported here (env-name caps, reserved prefixes,
//! the plugin-name regex) are part of the contract with
//! `launcher/src/validate.rs`. Any change must land here AND there.

pub mod config;
pub mod error;
pub mod package;
pub mod plugin_spec;

pub use config::{
    BootstrapConfig, BootstrapConfigRaw, LoadError, TenantEntry, TenantRaw, WorkspaceEntry,
    WorkspacePluginEntry, WorkspacePluginRaw, WorkspaceRaw,
};
pub use error::ValidationError;
pub use package::{
    validate_package, Isolation, PackageFileEntry, SpillEntry, SpillMode, ValidatedPackage,
    DEFAULT_PACKAGE_PATH,
};
pub use plugin_spec::{
    validate_one, validate_workspace_plugin_config, RawPluginEntry, ValidatedPlugin,
    CONFIG_ENV_NAME, MAX_ENV_VALUE_BYTES, MAX_STATIC_ENV_ENTRIES, PLUGIN_NAME_RE,
    RESERVED_ENV_NAMES, SECRET_ENV_PREFIX, TOOL_NAME_RE,
};
