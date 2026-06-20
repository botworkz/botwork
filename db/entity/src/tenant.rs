//! `tenant` — top-level account row.
//!
//! Identity is a globally-unique `name`. `name` is the slug the operator
//! types (`phlax`, in the dev deployment) and the join key used by
//! `/resolve`. Surrogate `id` is the FK target for [`workspace`].
//!
//! `ON DELETE` semantics on the inbound FK (workspace.tenant_id):
//! **RESTRICT** — deleting a tenant must be a deliberate two-step
//! operation (drop workspaces first, then the tenant). The day a stray
//! `DELETE FROM tenant WHERE name = 'phlax'` slips into a migration
//! during admin-api bring-up, we want it to fail loudly rather than
//! cascade-delete every binding.
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
}

impl Related<super::workspace::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Workspace.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
