//! `botwork-entity` — workspace persistence-layer entry point.
//!
//! Holds SeaORM entity definitions and the helpers used to obtain a
//! [`sea_orm::DatabaseConnection`]. Every persistence-aware consumer
//! (config-broker, control-plane, the future admin-api, the bootstrap
//! binary) depends on this crate so the schema has a single source of
//! truth.
//!
//! # v0 schema (RFE #101)
//!
//! ```text
//!     tenant ─1:N─┐
//!                 │
//!              workspace ─M:N─ workspace_plugin ─N:1─ plugin
//! ```
//!
//! * [`tenant`] — top-level account. `name` is globally unique.
//! * [`workspace`] — scoped to a tenant. `(tenant_id, name)` unique.
//!   A tenant has ≥ 1 workspace; deletion of a tenant with workspaces
//!   is `RESTRICT` (must be deliberate two-step).
//! * [`plugin`] — globally-named package. Carries the *intrinsic* shape
//!   of the plugin (image, egress posture). Today's `plugins.yaml`
//!   collapses identity and binding; this crate splits them.
//! * [`workspace_plugin`] — the binding row. `(workspace_id, plugin_id)`
//!   composite PK. Carries the per-binding `config` blob (nullable —
//!   nothing today populates inheritance from the plugin row).
//!
//! Resolve hot-path (config-broker, post-cutover):
//!
//! ```sql
//! SELECT p.image, p.egress, wp.config
//! FROM   plugin p
//! JOIN   workspace_plugin wp ON wp.plugin_id    = p.id
//! JOIN   workspace        w  ON w.id            = wp.workspace_id
//! JOIN   tenant           t  ON t.id            = w.tenant_id
//! WHERE  t.name = $1 AND w.name = $2 AND p.name = $3;
//! ```
//!
//! # Trust posture
//!
//! v0 has a single DB role (`botwork`) used by every consumer. Per-consumer
//! roles + GRANTs are a follow-up that pays off once admin-api lands — until
//! then trust is enforced at the docker-network boundary (`botwork-internal`)
//! and at the bind-mounted credential file (`/var/lib/botwork-db/secret.env`,
//! mode 0600). The crate itself does no credential plumbing: it reads
//! `BOTWORK_DATABASE_URL` from the process environment via
//! [`connection::connect_from_env`].
//!
//! # JSONB columns
//!
//! `plugin.egress` and `workspace_plugin.config` are `jsonb`. The decision
//! was deliberately deferred until a real query forces structure — see
//! RFE #101 § "JSONB vs typed columns". Treat the JSON as opaque at the
//! storage layer; validation happens on the wire boundary
//! (config-broker / future admin-api), not in the entity layer.

pub mod connection;
pub mod plugin;
pub mod tenant;
pub mod workspace;
pub mod workspace_plugin;

pub use connection::{connect, connect_from_env, ConnectError, DATABASE_URL_ENV};
