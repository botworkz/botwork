//! `plugin` — globally-named package row.
//!
//! Identity is a globally-unique `name`. Two tenants installing
//! `mcp-bash` reference the same plugin row (RFE #101 option A); they
//! attach their own per-binding `config` via the workspace_plugin join.
//!
//! The plugin row carries the *intrinsic* shape of the package, which
//! is the full set of fields a config-broker `/resolve` response needs
//! to drive a session-broker container spawn:
//!
//! * `image` — docker image reference (text).
//! * `port` — listen port inside the container (1-65535; default 8000).
//! * `path` — HTTP base path on the plugin (must start with `/`).
//! * `upstream_auth` — wire-form text: `"none"` or `"bearer/<service>"`.
//!   Stored verbatim; parsed at the wire boundary (bootstrap on the
//!   write side, config-broker on the read side).
//! * `env` — jsonb array of `{name, value}` objects. Order preserved.
//! * `resources` — jsonb `{cpus?, memory?, pids?}` or NULL.
//! * `egress` — jsonb. Wire shape: `{ "mode": "all" }`,
//!   `{ "mode": "none" }`, or `{ "allow": [{ "host", "ports": [...] }, ...] }`.
//!   The entity layer does NOT interpret it; control-plane owns the
//!   policy decision (RFE #97 / #81).
//!
//! ## What lives where
//!
//! Validation (shape, regex, size caps, reserved env names,
//! `network:` rejection) lives in `botwork-bootstrap`, not here.
//! The entity layer trusts the DB. Round-tripping a row through SeaORM
//! is a structural operation; the bootstrap binary is what refuses to
//! write something invalid in the first place.
//!
//! `ON DELETE` semantics on the inbound FK (workspace_plugin.plugin_id):
//! **RESTRICT** — a plugin in use anywhere must be disabled (binding
//! removed) on every workspace before it can be deleted. Prevents
//! accidental "you deleted mcp-bash, every session that resolves it
//! now 404s" cascades.
//!
//! ## `current_facet_id` (RFE #146)
//!
//! The optional `current_facet_id` pointer is a forward reference to
//! the [`super::plugin_image_facet`] row that describes the image
//! this plugin is currently running. NULL during the rollout window
//! where a `plugin` row exists but no facet has been ingested yet
//! (the catalog oneshot hasn't run, or the image legitimately lacks
//! labels) — config-broker's `/resolve` continues to read straight
//! off this row in that window. Once the facet lands, the catalog
//! upserter (RFE #146 successor) repoints the pointer, and the
//! eventual `/resolve` cutover (next RFE again) joins through it.
//!
//! The outbound FK is **RESTRICT**: deleting a facet a live plugin
//! row points at would silently break that plugin's `/resolve`.

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
    /// Listen port inside the plugin container. Postgres `int`;
    /// the bootstrap validator constrains 1..=65535. v0 wire default
    /// is 8000.
    pub port: i32,
    /// HTTP base path. Starts with `/`. v0 wire default is `/`.
    pub path: String,
    /// Wire-form upstream-auth: `"none"` or `"bearer/<service>"`.
    /// Stored as text rather than tagged JSON because the wire form
    /// is the contract; bootstrap validates the shape on write.
    pub upstream_auth: String,
    /// Static env entries. `jsonb` array of `{name, value}`. Order
    /// preserved; the YAML map shape doesn't preserve order natively
    /// but the bootstrap parser captures order at parse time and
    /// writes the array verbatim.
    #[sea_orm(column_type = "JsonBinary")]
    pub env: Json,
    /// Optional `{cpus?, memory?, pids?}` blob.
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub resources: Option<Json>,
    /// `jsonb` in postgres. See module docs for shape.
    #[sea_orm(column_type = "JsonBinary")]
    pub egress: Json,
    pub created_at: ChronoDateTimeUtc,
    pub updated_at: ChronoDateTimeUtc,
    /// Optional FK → [`super::plugin_image_facet::Model::id`]. NULL
    /// during the rollout window where no facet has been ingested
    /// for this plugin yet (RFE #146); the catalog upserter
    /// repoints it once a matching `(plugin_name, image_config_sha)`
    /// observation lands. RESTRICT on the inbound FK so a facet a
    /// live plugin row points at cannot be silently dropped.
    pub current_facet_id: Option<Uuid>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::workspace_plugin::Entity")]
    WorkspacePlugin,
    /// A plugin owns zero-or-more `session_worker` rows — one per
    /// currently-or-historically spawned container of this plugin
    /// (RFE #105 round-3). Inverse side (worker → plugin) lives on
    /// `session_worker`; the FK is RESTRICT so a plugin row with
    /// live workers can't be silently dropped.
    #[sea_orm(has_many = "super::session_worker::Entity")]
    SessionWorker,
    /// `plugin.current_facet_id` → `plugin_image_facet.id`. RESTRICT
    /// on delete: a facet a live plugin row points at cannot be
    /// silently dropped (RFE #146).
    #[sea_orm(
        belongs_to = "super::plugin_image_facet::Entity",
        from = "Column::CurrentFacetId",
        to = "super::plugin_image_facet::Column::Id",
        on_update = "NoAction",
        on_delete = "Restrict"
    )]
    CurrentFacet,
}

impl Related<super::workspace_plugin::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::WorkspacePlugin.def()
    }
}

impl Related<super::session_worker::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SessionWorker.def()
    }
}

impl Related<super::plugin_image_facet::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::CurrentFacet.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
