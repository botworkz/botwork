//! Create the `plugin_image_facet` table and the
//! `plugin.current_facet_id` pointer column — RFE #146 / tracking
//! design [`botworkz/space#303`].
//!
//! New table + one ALTER on `plugin`: lays down the storage shape
//! for image-borne plugin descriptors (the `org.botwork.mcp.*` OCI
//! labels declared on each `botwork/mcp-*:local` image). No reader
//! and no writer wire this up in this PR — RFE #146 is the schema
//! landing only. The catalog upserter (`botwork-image-catalog`) and
//! the config-broker resolve cutover both land as separate follow-up
//! RFEs against this same table.
//!
//! See `db/entity/src/plugin_image_facet.rs` for the entity-level
//! column semantics and the lifecycle.
//!
//! [`botworkz/space#303`]: https://github.com/botworkz/space/issues/303
//!
//! ## Column choices
//!
//! Same posture as the v0 + agent_session + session_worker + auth
//! migrations:
//!
//! * `id uuid PK DEFAULT gen_random_uuid()` — `pgcrypto` is already
//!   enabled by the v0 `create_core_tables` migration so no
//!   `CREATE EXTENSION` is needed here.
//! * `plugin_name text NOT NULL` — the `org.botwork.mcp.name` label
//!   verbatim. **Not** an FK to `plugin.name`: facets are observed
//!   off the image and may legitimately exist before a `plugin` row
//!   does (the catalog upserter ingests every loaded image; the
//!   `plugin` row only appears once an operator binds it via
//!   bootstrap / api). The (plugin_name, image_config_sha)
//!   UNIQUE keeps duplicates out; the future pointer-repoint step
//!   joins by name.
//! * `image_ref text NOT NULL` — the resolved image reference
//!   (`botwork/mcp-echo:local` etc). Captured for operator surfaces;
//!   not used in any predicate.
//! * `image_config_sha text NOT NULL` — `docker image inspect`'s
//!   `Id`/`Config.Digest`. Together with `plugin_name` this is the
//!   natural key of the row: two observations of the same image
//!   (same labels) collapse onto the same row via `ON CONFLICT DO
//!   NOTHING` at the catalog upserter level.
//! * `spec_version text NOT NULL` — `org.botwork.mcp.spec` (`"v1"`
//!   today). Stored verbatim; the catalog upserter validates against
//!   known versions on the write side.
//! * `port integer NOT NULL` — `org.botwork.mcp.port`. Matches the
//!   existing `plugin.port` type (postgres `int`). u16 range is
//!   enforced at upsert time, same as for `plugin.port`.
//! * `path text NOT NULL` — `org.botwork.mcp.path`. Must start with
//!   `/` (validator-side; no DB CHECK constraint, same as
//!   `plugin.path`).
//! * `upstream_auth text NOT NULL` — `org.botwork.mcp.upstream_auth`.
//!   Wire form (`"none"` or `"bearer/<service>"`), stored verbatim;
//!   same posture as `plugin.upstream_auth`.
//! * `egress jsonb NOT NULL` — `org.botwork.mcp.egress`. Same wire
//!   shape as `plugin.egress`.
//! * `resources jsonb NULL` — `org.botwork.mcp.resources`. Nullable
//!   for plugins that don't declare resource caps; matches
//!   `plugin.resources`.
//! * `env jsonb NOT NULL DEFAULT '[]'::jsonb` — `org.botwork.mcp.env`,
//!   pre-validated `[{name, value}, ...]` array. Default makes the
//!   ALTER-on-existing-rows posture safe even though there are no
//!   pre-existing rows in this table.
//! * `isolation text NOT NULL` — `"shared" | "per_agent_session" |
//!   "per_request"`. Stored verbatim; not a postgres enum for the
//!   same reason `agent_session.state` isn't.
//! * `capabilities jsonb NOT NULL` — `org.botwork.mcp.capabilities`,
//!   the MCP `capabilities` object from `initialize`.
//! * `tools jsonb NOT NULL DEFAULT '[]'::jsonb` — denormalised
//!   `[{name, description, title?, input_schema, output_schema?}, ...]`.
//!   Tool names are already `plugin__native`-prefixed by the catalog
//!   upserter so session-broker serves them verbatim. See RFE #146
//!   for why JSON instead of a `plugin_tool` side table.
//! * `resources_catalog jsonb NOT NULL DEFAULT '[]'::jsonb` —
//!   denormalised `[{name, uri, description?, mime_type?}, ...]`.
//!   Named `resources_catalog` (not `resources`) because the row
//!   already has a `resources` column for container resources.
//! * `prompts jsonb NOT NULL DEFAULT '[]'::jsonb` — denormalised
//!   `[{name, description?, arguments?}, ...]`.
//! * `protocol_version text NOT NULL` —
//!   `org.botwork.mcp.protocol_version`, the MCP wire-protocol pin
//!   (e.g. `"2024-11-05"`).
//! * `spill_policy jsonb NULL` — `org.botwork.mcp.spill`. Nullable
//!   for plugins that don't declare a spill policy.
//! * `observed_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP` —
//!   first time the catalog upserter saw this `(plugin_name,
//!   image_config_sha)`. The table is insert-only: a re-observation
//!   is a no-op via `ON CONFLICT DO NOTHING`, so a `created_at` +
//!   `updated_at` pair would always be equal. `observed_at` is the
//!   honest single-column name.
//!
//! ## `plugin.current_facet_id`
//!
//! `ALTER TABLE plugin ADD COLUMN current_facet_id uuid NULL` with
//! FK → `plugin_image_facet.id` ON DELETE **RESTRICT**.
//!
//! * **NULL-able** because there's a rollout window where a `plugin`
//!   row exists from the existing `bootstrap.yaml` `image:` path but
//!   no facet has been ingested yet (the catalog oneshot hasn't run,
//!   or the image legitimately lacks labels). config-broker's
//!   `/resolve` continues to read straight off the `plugin` row in
//!   that window; the next RFE flips it to join through the pointer
//!   when non-NULL and COALESCE on top.
//! * **RESTRICT** because deleting a facet a live `plugin` row points
//!   at would silently break that plugin's `/resolve`. The v1 catalog
//!   upserter is insert-only and never deletes; a future janitor that
//!   prunes old facets has to walk `plugin.current_facet_id` first
//!   and either re-point or refuse.
//!
//! ## Index design
//!
//! Three named indexes alongside the implicit PK btree:
//!
//! * `ux_plugin_image_facet_name_sha` — UNIQUE on
//!   `(plugin_name, image_config_sha)`. The natural key of the
//!   table; drives the catalog upserter's `ON CONFLICT DO NOTHING`
//!   path and rejects accidental duplicates.
//! * `ix_plugin_image_facet_name_observed` — non-unique on
//!   `(plugin_name, observed_at DESC)`. Drives the operator audit
//!   query "all facets for plugin X, newest first". Lookups bounded
//!   by `plugin_name` cardinality (≈ plugin count) so a btree is
//!   fine.
//! * `ix_plugin_current_facet` — non-unique on
//!   `plugin.current_facet_id`. Drives config-broker's `/resolve`
//!   JOIN once the resolve cutover lands; planted now so the
//!   follow-up PR doesn't have to alter the plugin table twice.
//!
//! ## Forward-only
//!
//! New table + new column on the existing `plugin` table; the
//! `down()` path drops both in reverse-create order (the column has
//! to go first because of the FK). v0 production migrations are
//! forward-only by convention; `down` is never run by
//! `botwork-migration` itself, but is kept for `Migrator::down`
//! driven by tests + future operator tooling.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // ── plugin_image_facet ───────────────────────────────────────
        manager
            .create_table(
                Table::create()
                    .table(PluginImageFacet::Table)
                    .col(
                        ColumnDef::new(PluginImageFacet::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .default(Expr::cust("gen_random_uuid()")),
                    )
                    .col(string(PluginImageFacet::PluginName))
                    .col(string(PluginImageFacet::ImageRef))
                    .col(string(PluginImageFacet::ImageConfigSha))
                    .col(string(PluginImageFacet::SpecVersion))
                    .col(integer(PluginImageFacet::Port))
                    .col(text(PluginImageFacet::Path))
                    .col(text(PluginImageFacet::UpstreamAuth))
                    .col(
                        ColumnDef::new(PluginImageFacet::Egress)
                            .json_binary()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(PluginImageFacet::Resources)
                            .json_binary()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(PluginImageFacet::Env)
                            .json_binary()
                            .not_null()
                            .default(Expr::cust("'[]'::jsonb")),
                    )
                    .col(text(PluginImageFacet::Isolation))
                    .col(
                        ColumnDef::new(PluginImageFacet::Capabilities)
                            .json_binary()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(PluginImageFacet::Tools)
                            .json_binary()
                            .not_null()
                            .default(Expr::cust("'[]'::jsonb")),
                    )
                    .col(
                        ColumnDef::new(PluginImageFacet::ResourcesCatalog)
                            .json_binary()
                            .not_null()
                            .default(Expr::cust("'[]'::jsonb")),
                    )
                    .col(
                        ColumnDef::new(PluginImageFacet::Prompts)
                            .json_binary()
                            .not_null()
                            .default(Expr::cust("'[]'::jsonb")),
                    )
                    .col(text(PluginImageFacet::ProtocolVersion))
                    .col(
                        ColumnDef::new(PluginImageFacet::SpillPolicy)
                            .json_binary()
                            .null(),
                    )
                    .col(
                        timestamp_with_time_zone(PluginImageFacet::ObservedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        // UNIQUE on (plugin_name, image_config_sha): the natural key
        // of the table. Two observations of the same image labels
        // collapse onto the same row via the catalog upserter's
        // ON CONFLICT DO NOTHING path; this index is what makes that
        // path correct.
        manager
            .create_index(
                Index::create()
                    .name("ux_plugin_image_facet_name_sha")
                    .table(PluginImageFacet::Table)
                    .col(PluginImageFacet::PluginName)
                    .col(PluginImageFacet::ImageConfigSha)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // Audit index: "all facets for plugin X, newest first".
        // SeaORM's IndexCreateStatement doesn't model per-column
        // index ordering (ASC/DESC), so we fall back to raw SQL —
        // same pattern as ux_session_worker_live_per_plugin in
        // m20260622_000002_create_session_worker.rs and ix_lease_live
        // in m20260624_000001_create_auth_tables.rs.
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE INDEX ix_plugin_image_facet_name_observed \
                 ON plugin_image_facet (plugin_name, observed_at DESC)",
            )
            .await?;

        // ── plugin.current_facet_id ──────────────────────────────────
        manager
            .alter_table(
                Table::alter()
                    .table(Plugin::Table)
                    .add_column(ColumnDef::new(Plugin::CurrentFacetId).uuid().null())
                    .add_foreign_key(
                        TableForeignKey::new()
                            .name("fk_plugin_current_facet")
                            .from_tbl(Plugin::Table)
                            .from_col(Plugin::CurrentFacetId)
                            .to_tbl(PluginImageFacet::Table)
                            .to_col(PluginImageFacet::Id)
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .to_owned(),
            )
            .await?;

        // Forward-lookup index. config-broker's /resolve JOINs
        // through this column once the resolve cutover lands; we
        // plant the index now so the follow-up PR doesn't have to
        // alter `plugin` a second time.
        manager
            .create_index(
                Index::create()
                    .name("ix_plugin_current_facet")
                    .table(Plugin::Table)
                    .col(Plugin::CurrentFacetId)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Drop the pointer column first — its FK references
        // plugin_image_facet.id, so dropping the table out from
        // under it would 23503.
        manager
            .alter_table(
                Table::alter()
                    .table(Plugin::Table)
                    .drop_foreign_key(Alias::new("fk_plugin_current_facet"))
                    .drop_column(Plugin::CurrentFacetId)
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(Table::drop().table(PluginImageFacet::Table).to_owned())
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum PluginImageFacet {
    Table,
    Id,
    PluginName,
    ImageRef,
    ImageConfigSha,
    SpecVersion,
    Port,
    Path,
    UpstreamAuth,
    Egress,
    Resources,
    Env,
    Isolation,
    Capabilities,
    Tools,
    ResourcesCatalog,
    Prompts,
    ProtocolVersion,
    SpillPolicy,
    ObservedAt,
}

// Re-declared here (not imported from the v0 migration's file) so
// this migration's `up()` keeps compiling if the v0 `enum Plugin`
// ever gets renamed. SeaORM resolves the iden via the `DeriveIden`
// impl; matching `Table` names on a fresh enum produces the same
// SQL — same posture as the agent_session, session_worker, and auth
// migrations take. The `CurrentFacetId` variant is new in this
// migration.
#[derive(DeriveIden)]
enum Plugin {
    Table,
    CurrentFacetId,
}
