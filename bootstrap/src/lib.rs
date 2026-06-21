//! `botwork-bootstrap` — apply `bootstrap.yaml` to the database.
//!
//! Runs once at boot via the systemd oneshot
//! `botwork-bootstrap.service`, ordered between
//! `botwork-db-migrate.service` (which lands the schema) and
//! `botwork-config-broker.service` (which reads from the DB).
//!
//! The yaml shape carries the full plugin spec (image, port, path,
//! upstream_auth, env, resources, egress) as RFE #101 PR2; per-entry
//! validation rules live in `botwork-admin-core` (RFE #106 PR2)
//! shared with `botwork-admin-api`. List-level rules (duplicate
//! names, unknown plugin refs in bindings) live here.
//!
//! # Lifetime
//!
//! Bootstrap is **deliberately throwaway**. When admin-api lands the
//! whole crate goes away (one config file, one shape, no
//! subcommands, no clever reconciliation — only upserts).
//!
//! # Idempotency
//!
//! Every operation is `find-then-INSERT-or-UPDATE` on the join keys:
//!
//! * `tenant` keyed on `name`,
//! * `workspace` keyed on `(tenant_id, name)`,
//! * `plugin` keyed on `name`,
//! * `workspace_plugin` keyed on `(workspace_id, plugin_id)`.
//!
//! Re-running with an unchanged yaml is a no-op observable only in
//! `updated_at` bumps — and even those don't change unless the
//! comparable columns differ. That property matters: the systemd
//! unit restarts at every boot, and we want "we re-ran bootstrap"
//! to never be a behaviour change.

pub mod config;
pub mod error;
pub mod runner;

pub use config::{
    BootstrapConfig, BootstrapConfigRaw, TenantEntry, WorkspaceEntry, WorkspacePluginEntry,
};
pub use error::BootstrapError;

// Re-export the validator types so existing downstream callers
// (`botwork-admin-api`'s integration test uses these via
// `botwork_bootstrap::{BootstrapConfig, BootstrapConfigRaw}`) get the
// same surface they had before the admin-core extraction. New code
// should depend on `botwork-admin-core` directly.
pub use botwork_admin_core::plugin_spec::{RawPluginEntry, ValidatedPlugin};

pub use runner::apply;
