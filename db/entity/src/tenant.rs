//! `tenant` — top-level account row.
//!
//! Identity is a globally-unique `name`. `name` is the slug the operator
//! types (`phlax`, in the dev deployment) and the join key used by
//! `/resolve`. Surrogate `id` is the FK target for [`workspace`].
//!
//! `ON DELETE` semantics on the inbound FKs:
//! * `workspace.tenant_id` → **RESTRICT** — deleting a tenant must be a
//!   deliberate two-step operation (drop workspaces first, then the
//!   tenant). The day a stray `DELETE FROM tenant WHERE name = 'phlax'`
//!   slips into a migration during admin-api bring-up, we want it to
//!   fail loudly rather than cascade-delete every binding.
//! * `agent_session.tenant_id` → **CASCADE** — agent sessions are a
//!   secondary projection of the tenant; they have no value once the
//!   parent is gone (RFE #105). The RESTRICT on workspace still
//!   enforces the two-step posture, since workspaces have to drop
//!   first.
//! * `opaque_password_file.tenant_id` → **CASCADE** — same reason as
//!   agent_session: the OPAQUE registration blob has no meaning
//!   without the tenant it authenticates (botworkz/botwork#141).
//! * `lease.tenant_id` → **CASCADE** — same posture; a lease without
//!   a tenant is meaningless (botworkz/botwork#141).
//!
//! [`workspace`]: super::workspace

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "tenant")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    #[sea_orm(unique)]
    pub name: String,
    pub created_at: ChronoDateTimeUtc,
    pub updated_at: ChronoDateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    /// A tenant has many workspaces. The inverse side (workspace → tenant)
    /// is defined on the workspace entity.
    #[sea_orm(has_many = "super::workspace::Entity")]
    Workspace,
    /// A tenant has many agent sessions (RFE #105). Inverse side
    /// (agent_session → tenant) is defined on agent_session.
    #[sea_orm(has_many = "super::agent_session::Entity")]
    AgentSession,
    /// A tenant has zero-or-one OPAQUE password file rows
    /// (botworkz/botwork#141). v0 ships UNIQUE on `tenant_id` so the
    /// cardinality is enforced; `has_many` is the SeaORM idiom for
    /// the inverse side regardless (the typed wrapper crate in
    /// botworkz/botwork-extra#123 collapses it back to a single row
    /// at the application layer).
    #[sea_orm(has_many = "super::opaque_password_file::Entity")]
    OpaquePasswordFile,
    /// A tenant has many leases (botworkz/botwork#141). Inverse side
    /// (lease → tenant) is defined on lease.
    #[sea_orm(has_many = "super::lease::Entity")]
    Lease,
}

impl Related<super::workspace::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Workspace.def()
    }
}

impl Related<super::agent_session::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::AgentSession.def()
    }
}

impl Related<super::opaque_password_file::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::OpaquePasswordFile.def()
    }
}

impl Related<super::lease::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Lease.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
