//! Create the four core tables: `tenant`, `workspace`, `plugin`,
//! `workspace_plugin`.
//!
//! Single migration because the four tables are inseparable: every
//! consumer that touches one needs all four to make sense of it, and
//! splitting "add tenant, then workspace, then plugin, ..." across
//! several migrations gives us partial-schema states that no consumer
//! actually wants to handle.
//!
//! Postgres specifics:
//!
//! * UUIDs are server-generated via `gen_random_uuid()`. Postgres 13+
//!   ships this in `pgcrypto`'s built-in path (no `CREATE EXTENSION`
//!   required on 13+). The migration enables it defensively for
//!   completeness; the operation is a no-op on a fresh image where the
//!   default search_path resolves the function from `pg_catalog`.
//! * `timestamptz` (`timestamp_with_time_zone`) instead of `timestamp`:
//!   postgres stores UTC internally, the `tz` annotation makes that
//!   explicit on the wire and lets SeaORM round-trip `DateTime<Utc>`
//!   without dropping the offset.
//! * `JsonBinary` materialises as `jsonb` on postgres. Storing as
//!   `jsonb` (not `json`) keeps `@>`/`?` predicates and GIN indexes
//!   available without rewriting the column type later.
//! * `created_at` / `updated_at` defaults are `CURRENT_TIMESTAMP`
//!   server-side; nothing rust-side has to populate them. `updated_at`
//!   triggers are not added in v0 — the only writers are bootstrap +
//!   future admin-api, both can set it explicitly.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Belt-and-braces: pgcrypto is bundled into postgres 13+ but
        // `gen_random_uuid` lives in `pg_catalog` only by default. The
        // `IF NOT EXISTS` guard makes this idempotent.
        manager
            .get_connection()
            .execute_unprepared("CREATE EXTENSION IF NOT EXISTS pgcrypto")
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(Tenant::Table)
                    .col(
                        ColumnDef::new(Tenant::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .default(Expr::cust("gen_random_uuid()")),
                    )
                    .col(string_uniq(Tenant::Name))
                    .col(
                        timestamp_with_time_zone(Tenant::CreatedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        timestamp_with_time_zone(Tenant::UpdatedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(Workspace::Table)
                    .col(
                        ColumnDef::new(Workspace::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .default(Expr::cust("gen_random_uuid()")),
                    )
                    .col(uuid(Workspace::TenantId))
                    .col(string(Workspace::Name))
                    .col(
                        timestamp_with_time_zone(Workspace::CreatedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        timestamp_with_time_zone(Workspace::UpdatedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_workspace_tenant")
                            .from(Workspace::Table, Workspace::TenantId)
                            .to(Tenant::Table, Tenant::Id)
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .to_owned(),
            )
            .await?;
        // (tenant_id, name) is the actual business key for workspaces:
        // every tenant has a `mcp` workspace, so `name` alone is NOT
        // unique. The unique index also gives us a fast index for the
        // resolve hot path (t.id × w.name).
        manager
            .create_index(
                Index::create()
                    .name("ux_workspace_tenant_name")
                    .table(Workspace::Table)
                    .col(Workspace::TenantId)
                    .col(Workspace::Name)
                    .unique()
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(Plugin::Table)
                    .col(
                        ColumnDef::new(Plugin::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .default(Expr::cust("gen_random_uuid()")),
                    )
                    .col(string_uniq(Plugin::Name))
                    .col(string(Plugin::Image))
                    .col(ColumnDef::new(Plugin::Egress).json_binary().not_null())
                    .col(
                        timestamp_with_time_zone(Plugin::CreatedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        timestamp_with_time_zone(Plugin::UpdatedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(WorkspacePlugin::Table)
                    .col(uuid(WorkspacePlugin::WorkspaceId))
                    .col(uuid(WorkspacePlugin::PluginId))
                    .col(ColumnDef::new(WorkspacePlugin::Config).json_binary().null())
                    .col(
                        timestamp_with_time_zone(WorkspacePlugin::CreatedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        timestamp_with_time_zone(WorkspacePlugin::UpdatedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .primary_key(
                        Index::create()
                            .name("pk_workspace_plugin")
                            .col(WorkspacePlugin::WorkspaceId)
                            .col(WorkspacePlugin::PluginId),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_workspace_plugin_workspace")
                            .from(WorkspacePlugin::Table, WorkspacePlugin::WorkspaceId)
                            .to(Workspace::Table, Workspace::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_workspace_plugin_plugin")
                            .from(WorkspacePlugin::Table, WorkspacePlugin::PluginId)
                            .to(Plugin::Table, Plugin::Id)
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .to_owned(),
            )
            .await?;

        // Reverse-direction lookup: "where is plugin X used?" used by
        // future admin-api delete-guards. PK gives us (workspace_id, ...)
        // for free; this index gives us (plugin_id, ...) for free.
        manager
            .create_index(
                Index::create()
                    .name("ix_workspace_plugin_plugin")
                    .table(WorkspacePlugin::Table)
                    .col(WorkspacePlugin::PluginId)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(WorkspacePlugin::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Plugin::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Workspace::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Tenant::Table).to_owned())
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Tenant {
    Table,
    Id,
    Name,
    CreatedAt,
    UpdatedAt,
}

#[derive(DeriveIden)]
enum Workspace {
    Table,
    Id,
    TenantId,
    Name,
    CreatedAt,
    UpdatedAt,
}

#[derive(DeriveIden)]
enum Plugin {
    Table,
    Id,
    Name,
    Image,
    Egress,
    CreatedAt,
    UpdatedAt,
}

#[derive(DeriveIden)]
enum WorkspacePlugin {
    Table,
    WorkspaceId,
    PluginId,
    Config,
    CreatedAt,
    UpdatedAt,
}
