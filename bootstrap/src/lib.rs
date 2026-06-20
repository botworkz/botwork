//! `botwork-bootstrap` — apply `bootstrap.yaml` to the database.
//!
//! Runs once at boot via the systemd oneshot
//! `botwork-bootstrap.service`, ordered between
//! `botwork-db-migrate.service` (which lands the schema) and
//! `botwork-config-broker.service` (which reads from the DB).
//!
//! The yaml shape carries the full plugin spec (image, port, path,
//! upstream_auth, env, resources, egress) as RFE #101 PR2; the
//! pre-cutover registry validation has moved here from config-broker.
//! See [`plugin_spec`] for the validation rules.
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
pub mod plugin_spec;
pub mod runner;

pub use config::{
    BootstrapConfig, BootstrapConfigRaw, TenantEntry, WorkspaceEntry, WorkspacePluginEntry,
};
pub use error::BootstrapError;
pub use plugin_spec::{RawPluginEntry, ValidatedPlugin};
pub use runner::apply;
