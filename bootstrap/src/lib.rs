//! `botwork-bootstrap` — sea-orm seed helper used by integration tests.
//!
//! Lifecycle note (RFE #106 PR4, botwork#118):
//!
//! This crate used to host the production boot-time DB writer:
//! a `botwork-bootstrap` binary, a `botwork/bootstrap:local`
//! container, and a `botwork-bootstrap.service` systemd oneshot
//! that ran between db-migrate and config-broker on every boot.
//!
//! All three of those are gone:
//!
//!   1. botwork#118 added `botwork-tools bootstrap`, an
//!      operator-facing replacement that POSTs through
//!      `botwork-api` instead of writing sea-orm directly.
//!   2. botwork#vm-side replaced `botwork-bootstrap.service`
//!      with `botwork-import.service` (a host-side oneshot
//!      calling `botwork-tools bootstrap`).
//!   3. This PR drops the container, the binary, the Earthfile
//!      target, and the CI plumbing that built them.
//!
//! What survives is this crate's library API — the
//! sea-orm-direct `apply()` algorithm and its supporting types.
//! Three integration tests (`api`, `config-broker`,
//! `session-broker`) use it as a fast seed helper:
//!
//! ```ignore
//!     let raw: BootstrapConfigRaw = serde_yaml::from_str(yaml)?;
//!     let cfg = BootstrapConfig::from_raw(raw)?;
//!     apply(&db, &cfg).await?;
//! ```
//!
//! Going through api in those tests would cost ~3x the
//! per-test setup (postgres + api + the HTTP roundtrip per
//! row) for no signal that doesn't already live in api's
//! own integration tests. The crate stays as a test-only lib.
//!
//! # Idempotency
//!
//! Every operation is `find-then-INSERT-or-UPDATE` on the join keys
//! (`tenant` keyed on `name`, `workspace` on `(tenant_id, name)`,
//! `plugin` on `name`, `workspace_plugin` on `(workspace_id, plugin_id)`).
//! Re-running `apply` against the same config is a no-op observable
//! only in `updated_at` bumps for rows whose comparable columns
//! changed.

pub mod error;
pub mod runner;
pub mod store;

pub use error::BootstrapError;
pub use store::BootstrapStore;

// Re-export from api-core so existing consumers
// (`botwork-api`, `botwork-config-broker`,
// `botwork-session-broker` integration tests) keep their
// `use botwork_bootstrap::{...}` paths working.
pub use botwork_api_core::{
    BootstrapConfig, BootstrapConfigRaw, RawPluginEntry, TenantEntry, ValidatedPlugin,
    WorkspaceEntry, WorkspacePluginEntry,
};

pub use runner::{apply, apply_with_store};
