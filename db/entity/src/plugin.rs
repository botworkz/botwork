//! `plugin` — globally-named package row.
//!
//! Identity is a globally-unique `name`. Two tenants installing
//! `mcp-bash` reference the same plugin row (RFE #101 option A); they
//! attach their own per-binding `config` via the workspace_plugin join.
//!
//! The plugin row carries the *intrinsic* shape of the package:
//!
//! * `image` — docker image reference (text). Validated by the
//!   bootstrap loader before insertion; the entity layer stores it
//!   verbatim.
//! * `egress` — jsonb. Wire shape today is
//!   `{ "mode": "all" }`, `{ "mode": "none" }`, or
//!   `{ "allow": [{ "host": ..., "ports": [...] }, ...] }`.
//!   The entity layer does NOT interpret it; control-plane owns the
//!   policy decision (RFE #97 / #81).
//!
//! `ON DELETE` semantics on the inbound FK (workspace_plugin.plugin_id):
//! **RESTRICT** — a plugin in use anywhere must be disabled (binding
//! removed) on every workspace before it can be deleted. Prevents
//! accidental "you deleted mcp-bash, every session that resolves it
//! now 404s" cascades.
//!
//! ## JSONB vs. structured `egress` (RFE #101)
//!
//! The egress block has a strict-enough wire shape today that we could
//! normalise it into columns. We don't, for the same reason `config`
//! stays JSONB: it's faithfully modelling what `plugins.yaml` carries,
//! and a follow-up migration moves it to typed storage once a real
//! query forces the choice. Storing it as `jsonb` (not `json`) keeps
//! the door open for `egress @> '{"mode":"all"}'` style predicates
//! without rewriting the column type.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "plugin")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    #[sea_orm(unique)]
    pub name: String,
    pub image: String,
    /// `jsonb` in postgres. SeaORM exposes both `json` and `json_binary`
    /// column types; we use `json_binary` to materialise as `jsonb`.
    /// See the migration for the precise DDL.
    #[sea_orm(column_type = "JsonBinary")]
    pub egress: Json,
    pub created_at: ChronoDateTimeUtc,
    pub updated_at: ChronoDateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::workspace_plugin::Entity")]
    WorkspacePlugin,
}

impl Related<super::workspace_plugin::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::WorkspacePlugin.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
