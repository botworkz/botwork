//! `plugin_image_facet` — one row per `(plugin_name, image_config_sha)`
//! observation of an image-borne plugin descriptor.
//!
//! Identity is the natural key `(plugin_name, image_config_sha)`.
//! Rows are insert-only: a re-observation of the same image labels is
//! a no-op via the catalog upserter's `ON CONFLICT DO NOTHING` path
//! (against the `ux_plugin_image_facet_name_sha` UNIQUE). Each row is
//! the projection of one `botwork/mcp-*:local` image's
//! `org.botwork.mcp.*` OCI labels at the moment the catalog upserter
//! saw it.
//!
//! See [RFE #146](https://github.com/botworkz/botwork/issues/146) for
//! the schema rationale and the tracking design at
//! [`botworkz/space#303`](https://github.com/botworkz/space/issues/303)
//! for the label contract.
//!
//! ## Why a separate table from `plugin`
//!
//! The existing `plugin` row carries operator intent: which image to
//! run, what binding overrides to apply, what egress posture to
//! enforce. That row is single-version (updates clobber) and is
//! written by `botwork-bootstrap` from `bootstrap.yaml` today, by the
//! future admin-api tomorrow.
//!
//! `plugin_image_facet` is the producer-side surface — what the
//! plugin's image *declares* about itself (`tools`, `capabilities`,
//! `protocol_version`, the descriptor fields that used to live in
//! the README + `tools/list` output). It's multi-versioned (one row
//! per distinct image config SHA, full history kept) and is written
//! by the future `botwork-image-catalog` oneshot.
//!
//! Splitting them keeps:
//!
//! * the operator-override semantics clean (the `plugin` row stays
//!   the single point of intent, the facet stays the read-only
//!   projection of the image),
//! * the audit history available (every image the catalog has ever
//!   observed remains queryable, even after the live pointer moves),
//! * `/resolve` cheap (one extra JOIN, not a join-of-three over
//!   tool / resource / prompt side tables — see RFE #146 for the
//!   "JSON columns vs side tables" rationale).
//!
//! ## Lifecycle
//!
//! ```text
//!   (catalog upserter observes image)
//!   INSERT … ON CONFLICT DO NOTHING ──► row exists forever
//!                                       │
//!                                       └─ pointed at by plugin.current_facet_id
//!                                          once the operator-override-merge step
//!                                          repoints
//! ```
//!
//! v1 has no DELETE path. A future janitor that prunes old facets
//! has to walk `plugin.current_facet_id` first (the FK is RESTRICT)
//! and either re-point or refuse.
//!
//! ## Why no `created_at` + `updated_at`
//!
//! The pair would always be equal — re-observations are no-ops, so
//! there's no `updated_at` to bump. `observed_at` is the honest
//! single-column name for "when did the catalog first see this
//! `(plugin_name, image_config_sha)`".
//!
//! ## Why JSON for `tools` / `resources_catalog` / `prompts`
//!
//! No predicate runs against tool/resource/prompt names. They're
//! served whole on `/resolve` and aggregated whole by
//! `tools/list` synthesis. Side tables would force a join-of-three
//! on every resolve and gain nothing. See RFE #146 for the full
//! reasoning. A future GIN index on `tools` can land without a
//! column-type rewrite if a per-tool predicate ever shows up.
//!
//! Tool names land here pre-prefixed (`plugin__native`) — the
//! catalog upserter prefixes at write time so session-broker serves
//! the column verbatim. See RFE #146 for why prefix-at-write.
//!
//! ## `plugin_name`, not `plugin_id` FK
//!
//! The catalog upserter observes the image before the `plugin` row
//! necessarily exists — the row is operator intent, the facet is
//! producer-side observation. The natural-key UNIQUE is on
//! `(plugin_name, image_config_sha)`; the future
//! `plugin.current_facet_id` repoint joins by name.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "plugin_image_facet")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    /// `org.botwork.mcp.name`. Not an FK to `plugin.name` — facets
    /// are observed off the image and may legitimately exist before
    /// the operator-intent `plugin` row does.
    pub plugin_name: String,
    /// Resolved image reference (`botwork/mcp-echo:local`,
    /// `ghcr.io/example/mcp-foo@sha256:…`). Captured for operator
    /// surfaces; not used in any predicate.
    pub image_ref: String,
    /// `docker image inspect`'s `Id` / `Config.Digest`. Together
    /// with [`Self::plugin_name`] this is the natural key of the row;
    /// the UNIQUE on `(plugin_name, image_config_sha)` is what makes
    /// the catalog upserter's `ON CONFLICT DO NOTHING` posture
    /// correct.
    pub image_config_sha: String,
    /// `org.botwork.mcp.spec` — `"v1"` today. Catalog upserter
    /// validates against known versions on the write side.
    pub spec_version: String,
    /// `org.botwork.mcp.port`. u16 range enforced at upsert time;
    /// the DB column is wide enough (postgres `int`) for the full
    /// range. Matches [`super::plugin::Model::port`].
    pub port: i32,
    /// `org.botwork.mcp.path`. Starts with `/`.
    pub path: String,
    /// `org.botwork.mcp.upstream_auth`. Wire form (`"none"` or
    /// `"bearer/<service>"`), stored verbatim. Matches
    /// [`super::plugin::Model::upstream_auth`].
    pub upstream_auth: String,
    /// `org.botwork.mcp.egress`. Same wire shape as
    /// [`super::plugin::Model::egress`] (`{mode: all|none}` or
    /// `{allow: [{host, ports: [...]}]}`).
    #[sea_orm(column_type = "JsonBinary")]
    pub egress: Json,
    /// `org.botwork.mcp.resources`. Optional
    /// `{cpus?, memory?, pids?}` blob. Nullable for plugins that
    /// don't declare resource caps.
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub resources: Option<Json>,
    /// `org.botwork.mcp.env`. Pre-validated `[{name, value}, ...]`
    /// array. Same wire shape as [`super::plugin::Model::env`].
    #[sea_orm(column_type = "JsonBinary")]
    pub env: Json,
    /// `org.botwork.mcp.isolation`. One of `"shared"`,
    /// `"per_agent_session"`, `"per_request"`. Stored as text rather
    /// than a postgres enum for the same reason
    /// [`super::agent_session::Model::state`] is: the wire form is
    /// the contract, a future migration that grows the value set is
    /// `ALTER COLUMN ... TYPE text` -> nothing.
    pub isolation: String,
    /// `org.botwork.mcp.capabilities`. The MCP `capabilities` object
    /// from `initialize`, served verbatim on `/resolve`.
    #[sea_orm(column_type = "JsonBinary")]
    pub capabilities: Json,
    /// Denormalised `[{name, description, title?, input_schema,
    /// output_schema?}, ...]`. Tool names are
    /// `plugin__native`-prefixed by the catalog upserter at write
    /// time; session-broker serves them verbatim.
    #[sea_orm(column_type = "JsonBinary")]
    pub tools: Json,
    /// Denormalised `[{name, uri, description?, mime_type?}, ...]`.
    /// Named `resources_catalog` (not `resources`) because the
    /// row already has a `resources` column for container resources.
    #[sea_orm(column_type = "JsonBinary")]
    pub resources_catalog: Json,
    /// Denormalised `[{name, description?, arguments?}, ...]`.
    #[sea_orm(column_type = "JsonBinary")]
    pub prompts: Json,
    /// `org.botwork.mcp.protocol_version`. MCP wire-protocol pin
    /// (e.g. `"2024-11-05"`).
    pub protocol_version: String,
    /// `org.botwork.mcp.spill`. Optional
    /// `{mode, threshold_bytes, include_methods, include_tools}`
    /// blob. Nullable for plugins that don't declare a spill policy.
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub spill_policy: Option<Json>,
    /// First time the catalog upserter saw this `(plugin_name,
    /// image_config_sha)`. Immutable after insert — re-observations
    /// are no-ops via `ON CONFLICT DO NOTHING`.
    pub observed_at: ChronoDateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    /// Zero-or-more `plugin` rows point at this facet via
    /// `plugin.current_facet_id`. The inverse side (plugin → facet)
    /// lives on [`super::plugin`]. RESTRICT on the inbound FK so a
    /// facet a live `plugin` row points at cannot be silently
    /// dropped.
    #[sea_orm(has_many = "super::plugin::Entity")]
    Plugin,
}

impl Related<super::plugin::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Plugin.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
