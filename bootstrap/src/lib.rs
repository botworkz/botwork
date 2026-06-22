//! `botwork-bootstrap` — apply `bootstrap.yaml` to the database.
//!
//! Runs once at boot via the systemd oneshot
//! `botwork-bootstrap.service`, ordered between
//! `botwork-db-migrate.service` (which lands the schema) and
//! `botwork-config-broker.service` (which reads from the DB).
//!
//! The yaml shape, the per-entry validators, and the list-level
//! tree validation all live in `botwork-admin-core`. This crate
//! is just the sea-orm writer that walks a validated tree and
//! upserts rows.
//!
//! # Lifetime
//!
//! Bootstrap is **deliberately throwaway**. RFE #106 PR4 ships
//! `botwork-tools bootstrap` as the replacement: same yaml,
//! HTTP-POSTed through admin-api instead of sea-orm-written
//! directly. The vm-side and space-side cutovers happen as
//! follow-up PRs. This crate stays in the workspace through the
//! cutover so its tests remain available; it goes away once the
//! systemd unit moves over.
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

pub mod error;
pub mod runner;

pub use error::BootstrapError;

// Re-export from admin-core so existing consumers
// (`botwork-admin-api`, `botwork-config-broker`, `botwork-session-broker`
// integration tests) keep their `use botwork_bootstrap::{...}` paths
// working through the cutover.
pub use botwork_admin_core::{
    BootstrapConfig, BootstrapConfigRaw, RawPluginEntry, TenantEntry, ValidatedPlugin,
    WorkspaceEntry, WorkspacePluginEntry,
};

pub use runner::apply;
