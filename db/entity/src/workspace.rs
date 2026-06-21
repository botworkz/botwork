//! `workspace` — user-visible binding unit scoped to a tenant.
//!
//! Naming caveat (RFE #101): "workspace" is the user-facing term but
//! sessions also have their own per-session workspaces on disk
//! (`/var/lib/botwork/tenants/<tenant>/<workspace>/<session>/`). The
//! collision is acknowledged and not fixed in v0; the database
//! workspace is the *binding* unit (which plugins are visible),
//! the on-disk per-session workspace is the *execution* unit.
//!
//! Identity is `(tenant_id, name)`. The default workspace name is
//! `mcp`; new tenants always start with one workspace under that name.
//! Because `(tenant_id, name)` is the business key — not `name` alone —
//! many tenants can have a workspace called `mcp` without collision.
//!
//! `ON DELETE` semantics:
//! * inbound from tenant: **RESTRICT** (see tenant.rs).
//! * outbound to workspace_plugin: **CASCADE** — deleting a workspace
//!   tears down its bindings as part of the same statement, since a
//!   binding without a workspace is meaningless.
//! * outbound to agent_session: **CASCADE** — RFE #105. Same reason
//!   as workspace_plugin: an agent session pinned to a non-existent
//!   workspace has no meaning, and we want the FK to enforce the
//!   invariant rather than leave dangling rows for the janitor to
//!   reconcile.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "workspace")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub created_at: ChronoDateTimeUtc,
    pub updated_at: ChronoDateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    /// Belongs to a tenant. RESTRICT on delete (see tenant.rs).
    #[sea_orm(
        belongs_to = "super::tenant::Entity",
        from = "Column::TenantId",
        to = "super::tenant::Column::Id",
        on_update = "NoAction",
        on_delete = "Restrict"
    )]
    Tenant,
    /// Owns the workspace_plugin binding rows. CASCADE on workspace delete.
    #[sea_orm(has_many = "super::workspace_plugin::Entity")]
    WorkspacePlugin,
    /// Owns the agent_session rows (RFE #105). CASCADE on workspace delete.
    #[sea_orm(has_many = "super::agent_session::Entity")]
    AgentSession,
}

impl Related<super::tenant::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Tenant.def()
    }
}

impl Related<super::workspace_plugin::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::WorkspacePlugin.def()
    }
}

impl Related<super::agent_session::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::AgentSession.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
