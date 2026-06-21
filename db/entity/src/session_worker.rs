//! `session_worker` — one row per spawned plugin container that an
//! agent session has used.
//!
//! Sessions are 1:N over containers, and this is the table that holds
//! the N side. Where [`agent_session`] is the durable, cost-/data-bearing
//! identity of one goose agent session (lives forever as audit + billing
//! evidence), `session_worker` rows are the per-container incarnations
//! — they exist for the life of one `mcp_session_<token>` container and
//! become reaped audit rows when that container exits.
//!
//! See [RFE #105](https://github.com/botworkz/botwork/issues/105) for
//! the design context. Round-3 (this entity) is what makes
//! sessions.json deletable: every fact session-broker used to write to
//! the JSON file (container name + IP + mcp_session_id + plugin) now
//! lives here.
//!
//! ## Why a separate table from `agent_session`
//!
//! The natural shape is 1:N — one agent session talks to multiple
//! plugins, and each plugin gets its own per-session container. Folding
//! the container metadata into `agent_session` would either:
//!
//! 1. force every per-container UPDATE to bump the agent_session row
//!    (lock contention as soon as goose has >1 plugin in flight), or
//! 2. encode "one plugin at a time" into the schema, which the wire
//!    contract does not enforce (and the smoke test exercises
//!    concurrent plugins explicitly).
//!
//! Operationally the two tables also have different lifecycle pressures:
//! `agent_session` rows are cost-/data-bearing and want a long retention
//! curve; `session_worker` rows are operational state that the janitor
//! will sweep on a much shorter cycle once `reaped_at` is set.
//!
//! ## Lifecycle
//!
//! A row is INSERTed by session-broker the instant the spawn path's
//! control-plane gate returns 2xx and is paired with a fresh
//! mcp_session_id (we know all four identifying bits — tenant,
//! workspace, plugin, container_name — at spawn time; agent_session_id
//! comes one round-trip later from goose's first non-init call, so
//! `agent_session_id` is `NULL` at insert time and back-filled on the
//! first request whose body carries it).
//!
//! `reaped_at IS NULL` means "the container is live (or thought to be);
//! routing is allowed". `reaped_at IS NOT NULL` is the terminal audit
//! state — the container is gone, the row stays for the janitor to
//! retain or delete on a separate timer.
//!
//! ```text
//!   (spawn)                                (teardown / exit)
//!   INSERT ──────► reaped_at IS NULL ──────► UPDATE reaped_at ──────► (janitor DELETE)
//! ```
//!
//! ## Identity
//!
//! `container_name` is globally unique on a single docker host —
//! launchers reject collisions at create time — so we lift that
//! property into the table as a UNIQUE constraint. session-broker's
//! cold-start recovery uses it to join `docker ps` output back to
//! row state with one indexed lookup per container.
//!
//! ## "One live worker per `(agent_session, plugin)`"
//!
//! Two live containers for the same agent session × the same plugin is
//! a bug — goose only addresses the plugin by name, so the second
//! container would be unreachable and leak. We enforce that here
//! rather than rely on session-broker's in-memory map by adding a
//! partial UNIQUE on `(agent_session_id, plugin_id) WHERE reaped_at
//! IS NULL`. The partial predicate is what makes audit rows
//! (multiple incarnations across a session's lifetime) legal.
//!
//! ## `ON DELETE`
//!
//! * `agent_session_id → agent_session.id` **CASCADE** — workers are a
//!   secondary projection of the session; deleting the parent session
//!   (which itself only happens via the agent_session lifecycle into
//!   `purged` + janitor DELETE) sweeps its workers.
//! * `plugin_id → plugin.id` **RESTRICT** — same posture as
//!   `workspace_plugin.plugin_id`: never silently lose a worker row
//!   because a plugin was deleted out from under us.
//! * `agent_session_id` is `NULL`-able to model the
//!   spawn-before-first-bind window described above.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "session_worker")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    /// FK → `agent_session.id`. CASCADE on agent_session delete.
    ///
    /// `NULL` for the spawn-to-first-bind window: we INSERT the row
    /// the moment control-plane gates the container in, and the
    /// agent-session-id arrives one round-trip later on the first
    /// non-init request. session-broker UPDATEs to populate the
    /// linkage and bumps `agent_session.last_active_at` in the same
    /// transaction.
    pub agent_session_id: Option<Uuid>,
    /// FK → `plugin.id`. RESTRICT on plugin delete (a plugin with
    /// live workers must be admin-explicitly drained first).
    pub plugin_id: Uuid,
    /// Docker container name (`mcp_session_<token>`). Globally unique
    /// on one host; we lift the property into a UNIQUE constraint
    /// here so cold-start recovery can join `docker ps` output back
    /// to row state with an indexed lookup.
    pub container_name: String,
    /// Container's IPv4 address on `botwork-plugin`. Captured at spawn
    /// time from `docker inspect` after the IPAM assignment lands.
    pub container_ip: String,
    /// MCP transport identifier the upstream surfaces in
    /// `Mcp-Session-Id`. Recorded after the initialize response. A
    /// row in the spawn-to-initialize-response window has an empty
    /// string here; the column is non-NULL to keep equality lookups
    /// cheap (NULL-tolerant indexes are a footgun on postgres).
    pub mcp_session_id: String,
    /// First `docker run` completion timestamp. Immutable.
    pub spawned_at: ChronoDateTimeUtc,
    /// `NULL` while the container is live (or believed-live).
    /// Set to a wall-clock timestamp on teardown — the row then sits
    /// for the janitor to retain for an operator-configurable window
    /// before DELETE.
    pub reaped_at: Option<ChronoDateTimeUtc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::agent_session::Entity",
        from = "Column::AgentSessionId",
        to = "super::agent_session::Column::Id",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    AgentSession,
    #[sea_orm(
        belongs_to = "super::plugin::Entity",
        from = "Column::PluginId",
        to = "super::plugin::Column::Id",
        on_update = "NoAction",
        on_delete = "Restrict"
    )]
    Plugin,
}

impl Related<super::agent_session::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::AgentSession.def()
    }
}

impl Related<super::plugin::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Plugin.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
