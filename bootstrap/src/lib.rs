//! `botwork-bootstrap` — apply `bootstrap.yaml` to the database.
//!
//! The bootstrap binary is the bridge between the deploy-time
//! YAML-as-source-of-truth and the runtime DB-as-source-of-truth.
//! It runs at every boot as a oneshot, between `botwork-db-migrate`
//! (which lands the schema) and `botwork-config-broker` (which reads
//! from it).
//!
//! v0 wire shape (RFE #101):
//!
//! ```yaml
//! tenants:
//! - name: phlax
//!   workspaces:
//!   - name: mcp
//!     plugins:
//!     - name: mcp-bash
//!       # optional per-binding config blob
//!       config:
//!         foo: bar
//!     - name: mcp-fetch
//!
//! plugins:
//! - name: mcp-bash
//!   image: ghcr.io/.../mcp-bash@sha256:...
//!   egress:
//!     mode: none
//! - name: mcp-fetch
//!   image: ghcr.io/.../mcp-fetch@sha256:...
//!   egress:
//!     allow:
//!     - host: example.com
//!       ports: [443]
//! ```
//!
//! # Lifetime
//!
//! Bootstrap is **deliberately throwaway**. The plan is for the future
//! admin-api to own the entity lifecycle (create/update/delete via
//! authenticated HTTP, with audit + validation). When that lands, this
//! crate goes away. The convention is to keep the surface narrow so the
//! deletion is mechanical:
//!
//! * One config file, one shape.
//! * One subcommand-less binary.
//! * No clever "reconciliation": delete-on-diff is out of scope; v0
//!   only upserts. Removing rows requires the admin-api.
//!
//! # Idempotency
//!
//! Every operation is `INSERT ... ON CONFLICT DO UPDATE`-shaped:
//!
//! * `tenant` keyed on `name`.
//! * `workspace` keyed on `(tenant_id, name)`.
//! * `plugin` keyed on `name`.
//! * `workspace_plugin` keyed on `(workspace_id, plugin_id)`.
//!
//! Re-running with an unchanged yaml is a no-op observable only in
//! `updated_at` bumps — and even those don't change unless the
//! comparable columns differ. That property matters: the systemd unit
//! restarts at every boot, and we want "we re-ran bootstrap" to never
//! be a behaviour change.

pub mod config;
pub mod error;
pub mod runner;

pub use config::{BootstrapConfig, PluginEntry, TenantEntry, WorkspaceEntry, WorkspacePluginEntry};
pub use error::BootstrapError;
pub use runner::apply;
