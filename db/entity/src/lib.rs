//! `botwork-entity` — workspace persistence-layer entry point.
//!
//! Holds SeaORM entity definitions and the helpers used to obtain a
//! [`sea_orm::DatabaseConnection`]. Every persistence-aware consumer
//! (config-broker, control-plane, the future admin-api, the bootstrap
//! binary, the future auth-broker) depends on this crate so the
//! schema has a single source of truth.
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
//! [RFE #105](https://github.com/botworkz/botwork/issues/105) adds
//! two further entities:
//!
//! * [`agent_session`] — durable identity of a goose agent's session
//!   keyed on `(tenant_id, workspace_id, agent_session_id)`. Tracks
//!   the lifecycle (`active`, `grace`, `inactive`, `teardown_requested`,
//!   `purged`) across container churn. Cost- and data-bearing: rows
//!   outlive their underlying containers and are operator-retained
//!   as the audit/billing surface for "what did this agent do?"
//! * [`session_worker`] — one row per plugin container that an agent
//!   session has spawned. 1:N from `agent_session` because one
//!   session talks to multiple plugins, each with its own container.
//!   Per-incarnation operational state (`container_name`,
//!   `container_ip`, `mcp_session_id`, `reaped_at`) lives here.
//!   Round-3 of the persistence cutover (this is what makes
//!   `/var/lib/botwork/sessions.json` deletable).
//!
//! [botworkz/botwork#141][issue-141] (cross-repo with
//! [botworkz/botwork-extra#123][rfe-123]) adds the auth-broker side:
//!
//! * [`opaque_password_file`] — one row per tenant. Holds the OPAQUE
//!   registration "password file" auth-broker reads on every login
//!   handshake. The blob is binary (`bytea`) and opaque to postgres
//!   — no `@>` predicates needed. UNIQUE on `tenant_id` enforces "one
//!   current suite per tenant" in v0; a future suite-rotation
//!   migration relaxes that to UNIQUE on `(tenant_id, suite_version)`.
//! * [`lease`] — one row per outstanding auth-broker lease. Bearer
//!   plaintext never lands in postgres; the hot-path lookup is by
//!   `bearer_hash` (SHA-256 of the bearer). UPDATEs on each request
//!   bump `idle_extends_to`; explicit revoke sets `revoked_at`. The
//!   janitor + per-tenant `max_lease` cap live in auth-broker, not
//!   here.
//!
//! [issue-141]: https://github.com/botworkz/botwork/issues/141
//! [rfe-123]: https://github.com/botworkz/botwork-extra/issues/123
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

pub mod agent_session;
pub mod connection;
pub mod lease;
pub mod opaque_password_file;
pub mod plugin;
pub mod session_worker;
pub mod tenant;
pub mod workspace;
pub mod workspace_plugin;

pub use connection::{connect, connect_from_env, ConnectError, DATABASE_URL_ENV};
