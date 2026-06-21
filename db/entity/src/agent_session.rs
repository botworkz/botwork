//! `agent_session` — durable identity of a goose agent's session.
//!
//! Identity is the natural triple `(tenant_id, workspace_id,
//! agent_session_id)`. The row tracks the lifecycle of one goose agent
//! session across container churn — every plugin container spawned
//! while goose runs under one `agent-session-id` references the same
//! row. Per-container identity (`mcp_session_<token>`, container IP,
//! image) stays in session-broker's in-memory `transport_sessions` map
//! and never hits the DB.
//!
//! See [RFE #105](https://github.com/botworkz/botwork/issues/105) for
//! the design context (identity flip from container-incarnation to
//! agent-session, lifecycle, who-writes-what).
//!
//! ## Lifecycle
//!
//! `state` is the lifecycle, not a transient teardown signal. Allowed
//! values are documented as the [`state`] module constants:
//!
//! ```text
//!   active ↔ grace ↔ inactive → teardown_requested → purged → DELETE
//! ```
//!
//! `active`, `grace`, `inactive` are "alive" states (workspace dir
//! exists, row addressable). `teardown_requested`, `purged` are "dead"
//! states (row exists for audit, workspace may or may not).
//!
//! ## Stored as text, not a typed enum
//!
//! `state` is `text` (not a postgres enum) for the same reason
//! `plugin.upstream_auth` is `text` — the wire form *is* the contract,
//! Rust callers go through the [`state`] constants for type-safety,
//! and a future migration that grows the value set is `ALTER TABLE
//! ALTER COLUMN ... TYPE text` -> nothing. A postgres enum would force
//! an `ALTER TYPE ... ADD VALUE` migration per addition, which is its
//! own footgun.
//!
//! ## Why no `plugin_id`
//!
//! Agent sessions are a property of `(tenant, workspace)`, not of
//! plugin. One goose agent talks to many plugins within a single
//! session; all of them share the same on-disk `/workspace`. Per-
//! incarnation plugin metadata stays in session-broker's in-memory
//! state.
//!
//! ## `ON DELETE` semantics
//!
//! Inbound FKs:
//! * `tenant_id → tenant.id`     **CASCADE** — deleting a tenant wipes
//!   its agent sessions.
//! * `workspace_id → workspace.id` **CASCADE** — deleting a workspace
//!   wipes its agent sessions.
//!
//! Both CASCADE because an agent session without its tenant + workspace
//! is meaningless. The deliberate-two-step posture lives one layer up
//! (the `workspace.tenant_id` FK is RESTRICT, so deleting a tenant
//! requires dropping its workspaces first — which then cascades into
//! its agent sessions).

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Canonical wire values of [`Model::state`]. Callers MUST go through
/// these constants rather than literals so a rename / addition is a
/// compile-time fanout, not a grep-and-pray exercise.
///
/// The validator that prevents arbitrary strings landing in the column
/// lives in session-broker (the only writer). v0 has no DB-side CHECK
/// constraint — same posture as `plugin.upstream_auth`.
pub mod state {
    /// At least one plugin container is running for this agent session.
    pub const ACTIVE: &str = "active";
    /// Transport closed; container(s) still up within the grace window.
    pub const GRACE: &str = "grace";
    /// Containers torn down; workspace dir persists; reactivatable.
    pub const INACTIVE: &str = "inactive";
    /// Admin / janitor requested full removal (workspace too).
    pub const TEARDOWN_REQUESTED: &str = "teardown_requested";
    /// Workspace gone; row kept for audit. Terminal until DELETE.
    pub const PURGED: &str = "purged";

    /// All legal values, in lifecycle order. Used by tests + by the
    /// session-broker validator to bound the column.
    pub const ALL: &[&str] = &[ACTIVE, GRACE, INACTIVE, TEARDOWN_REQUESTED, PURGED];

    /// `true` when `value` is one of [`ALL`]. The DB has no CHECK
    /// constraint; this is the gate.
    pub fn is_valid(value: &str) -> bool {
        ALL.contains(&value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_session")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    /// FK → `tenant.id`. CASCADE on tenant delete.
    pub tenant_id: Uuid,
    /// FK → `workspace.id`. CASCADE on workspace delete.
    pub workspace_id: Uuid,
    /// The goose-supplied `params._meta["agent-session-id"]` string.
    /// Stored verbatim; shape-validated by session-broker on write.
    pub agent_session_id: String,
    /// One of the [`state`] constants. See module docs.
    pub state: String,
    /// First-ever spawn for this `(tenant, workspace, agent_session_id)`
    /// triple. Immutable after insert.
    pub created_at: ChronoDateTimeUtc,
    /// Most recent `tools/call` (or other steady-state activity) on
    /// behalf of this agent session. Bumped on every request by the
    /// session-broker write-through path.
    pub last_active_at: ChronoDateTimeUtc,
    /// Bumped on every `inactive → active` transition. v0 reports it
    /// for operator visibility; the janitor never reads it.
    pub reactivation_count: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::tenant::Entity",
        from = "Column::TenantId",
        to = "super::tenant::Column::Id",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    Tenant,
    #[sea_orm(
        belongs_to = "super::workspace::Entity",
        from = "Column::WorkspaceId",
        to = "super::workspace::Column::Id",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    Workspace,
}

impl Related<super::tenant::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Tenant.def()
    }
}

impl Related<super::workspace::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Workspace.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_constants_round_trip_through_is_valid() {
        for s in state::ALL {
            assert!(state::is_valid(s), "{s} must validate");
        }
        for bad in ["", "ACTIVE", "running", "foo", "active "] {
            assert!(!state::is_valid(bad), "{bad:?} must not validate");
        }
    }

    #[test]
    fn state_all_matches_documented_lifecycle() {
        // Order matters: tests + janitor walk this slice in lifecycle
        // order. A reshuffle would silently change janitor sweep
        // semantics.
        assert_eq!(
            state::ALL,
            &[
                state::ACTIVE,
                state::GRACE,
                state::INACTIVE,
                state::TEARDOWN_REQUESTED,
                state::PURGED,
            ]
        );
    }
}
