//! `workspace_plugin` — the binding row.
//!
//! Composite PK on `(workspace_id, plugin_id)`. One row per
//! "this workspace exposes this plugin". The per-binding `config` JSON
//! blob lives here.
//!
//! `config` is nullable. Today every plugins.yaml entry that carries
//! a `config:` field will have it populated; entries without one will
//! be `NULL`. The plugin row does NOT carry a default config in v0
//! (RFE #101 § "no default_config column for v0"); that columns gets
//! added later as a pure-additive migration and resolve picks it up
//! via `COALESCE(wp.config, p.default_config)`. The decision was
//! "model what we have, don't speculate on inheritance".
//!
//! `ON DELETE` semantics:
//! * from workspace (`workspace_id`): **CASCADE** — drop the workspace,
//!   drop its bindings.
//! * from plugin (`plugin_id`): **RESTRICT** — a plugin in use anywhere
//!   cannot be deleted until every binding row is removed.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "workspace_plugin")]
pub struct Model {
    /// Composite PK part 1. References `workspace.id`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub workspace_id: Uuid,
    /// Composite PK part 2. References `plugin.id`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_id: Uuid,
    /// Per-binding config. Nullable — see entity-level docs.
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub config: Option<Json>,
    pub created_at: ChronoDateTimeUtc,
    pub updated_at: ChronoDateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::workspace::Entity",
        from = "Column::WorkspaceId",
        to = "super::workspace::Column::Id",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    Workspace,
    #[sea_orm(
        belongs_to = "super::plugin::Entity",
        from = "Column::PluginId",
        to = "super::plugin::Column::Id",
        on_update = "NoAction",
        on_delete = "Restrict"
    )]
    Plugin,
}

impl Related<super::workspace::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Workspace.def()
    }
}

impl Related<super::plugin::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Plugin.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
