//! Create the `opaque_password_file` and `lease` tables — auth-broker
//! persistence (botworkz/botwork#141 / botworkz/botwork-extra#123).
//!
//! New, additive: no existing table touched. Lands the two tables the
//! future auth-broker will INSERT/SELECT on every OPAQUE login and
//! `/auth/check`. Auth-broker continues to use raw-password bearers
//! throughout round 1a of the parent RFE — these tables sit
//! unused-but-present until round 1b (the single breaking PR) flips
//! `/auth/check` to lease-only.
//!
//! See `db/entity/src/opaque_password_file.rs` and
//! `db/entity/src/lease.rs` for the entity-level column semantics.
//!
//! ## Column choices
//!
//! Identical posture to the v0 + agent_session + session_worker
//! migrations:
//!
//! * `id uuid PK DEFAULT gen_random_uuid()` — `pgcrypto` is already
//!   enabled by the v0 `create_core_tables` migration so no
//!   `CREATE EXTENSION` is needed here.
//! * `tenant_id uuid NOT NULL` FK → `tenant.id` ON DELETE **CASCADE**
//!   on both tables. The password file and any leases are meaningless
//!   without the tenant; the two-step "deliberate tenant delete"
//!   posture is still enforced one layer up at the
//!   `workspace.tenant_id` RESTRICT FK.
//! * `bytea` (sea-query `binary()` materialises as postgres `bytea`)
//!   for `password_file`, `bearer_hash`, `wrapped_export_key` — every
//!   blob is binary, no encoding overhead, no UTF-8 validation on
//!   every read.
//! * `suite_version integer NOT NULL DEFAULT 1` on
//!   `opaque_password_file`. v0 has one current OPAQUE cipher-suite
//!   per tenant; the column exists from day 1 so a future
//!   suite-rotation migration is ALTER-only.
//! * `timestamptz` everywhere a timestamp lands (postgres stores UTC
//!   internally; the `tz` annotation makes that explicit on the wire
//!   and lets SeaORM round-trip `DateTime<Utc>` without dropping the
//!   offset).
//! * `revoked_at timestamptz NULL` on `lease`: NULL = live, non-NULL
//!   = terminal audit state. Same pattern as
//!   `session_worker.reaped_at`.
//!
//! ## Index design
//!
//! Three named indexes alongside the implicit PK btrees:
//!
//! * `ux_opaque_password_file_tenant` — UNIQUE on
//!   `opaque_password_file.tenant_id`. The login-handshake fetch
//!   ("which password file does this tenant own?"). v0 enforces "one
//!   current suite per tenant" with this index.
//! * `ux_lease_bearer_hash` — UNIQUE on `lease.bearer_hash`. The
//!   per-request validate path — auth-broker hashes the incoming
//!   `Authorization: Bearer <token>` and looks the row up by hash.
//! * `ix_lease_live` — partial index on
//!   `(tenant_id, expires_at) WHERE revoked_at IS NULL`. Drives
//!   "operator: list my active leases" and "janitor: sweep expired
//!   live rows" without scanning the revoked audit tail. Partial
//!   keeps it cheap as that tail grows. SeaORM's
//!   `IndexCreateStatement` doesn't model partial-index predicates,
//!   so this falls back to raw SQL via the connection backend —
//!   same pattern as `ux_session_worker_live_per_plugin` in
//!   `m20260622_000002_create_session_worker.rs`.
//!
//! ## Forward-only
//!
//! Two new tables; the `down()` path drops them in reverse-create
//! order (clean for tests). v0 production migrations are
//! forward-only by convention; `down` is never run by
//! `botwork-migration` itself, but is kept for `Migrator::down`
//! driven by tests + future operator tooling.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // ── opaque_password_file ─────────────────────────────────────
        manager
            .create_table(
                Table::create()
                    .table(OpaquePasswordFile::Table)
                    .col(
                        ColumnDef::new(OpaquePasswordFile::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .default(Expr::cust("gen_random_uuid()")),
                    )
                    .col(uuid(OpaquePasswordFile::TenantId))
                    .col(binary(OpaquePasswordFile::PasswordFile))
                    .col(
                        ColumnDef::new(OpaquePasswordFile::SuiteVersion)
                            .integer()
                            .not_null()
                            .default(1),
                    )
                    .col(
                        timestamp_with_time_zone(OpaquePasswordFile::CreatedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        timestamp_with_time_zone(OpaquePasswordFile::UpdatedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_opaque_password_file_tenant")
                            .from(OpaquePasswordFile::Table, OpaquePasswordFile::TenantId)
                            .to(Tenant::Table, Tenant::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // UNIQUE on tenant_id: v0 has one current OPAQUE suite per
        // tenant; this index enforces it. A future suite-rotation
        // migration will likely replace this with UNIQUE on
        // (tenant_id, suite_version) + a current_suite_version
        // pointer.
        manager
            .create_index(
                Index::create()
                    .name("ux_opaque_password_file_tenant")
                    .table(OpaquePasswordFile::Table)
                    .col(OpaquePasswordFile::TenantId)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // ── lease ────────────────────────────────────────────────────
        manager
            .create_table(
                Table::create()
                    .table(Lease::Table)
                    .col(
                        ColumnDef::new(Lease::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .default(Expr::cust("gen_random_uuid()")),
                    )
                    .col(uuid(Lease::TenantId))
                    .col(binary(Lease::BearerHash))
                    .col(binary(Lease::WrappedExportKey))
                    .col(
                        timestamp_with_time_zone(Lease::IssuedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .col(timestamp_with_time_zone(Lease::ExpiresAt))
                    .col(timestamp_with_time_zone(Lease::IdleExtendsTo))
                    .col(
                        ColumnDef::new(Lease::RevokedAt)
                            .timestamp_with_time_zone()
                            .null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_lease_tenant")
                            .from(Lease::Table, Lease::TenantId)
                            .to(Tenant::Table, Tenant::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // UNIQUE on bearer_hash: the per-request validate path.
        // bearer plaintext never lands in postgres; the hash is what
        // auth-broker keys on.
        manager
            .create_index(
                Index::create()
                    .name("ux_lease_bearer_hash")
                    .table(Lease::Table)
                    .col(Lease::BearerHash)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // Partial index: "live" rows only. Drives the operator
        // "list my active leases" surface and the janitor's
        // "sweep expired-but-not-yet-deleted" sweep without
        // scanning the revoked audit tail. SeaORM's
        // IndexCreateStatement doesn't model partial-index
        // predicates, so we fall back to raw SQL — same pattern
        // as ux_session_worker_live_per_plugin in
        // m20260622_000002_create_session_worker.rs.
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE INDEX ix_lease_live \
                 ON lease (tenant_id, expires_at) \
                 WHERE revoked_at IS NULL",
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Lease::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(OpaquePasswordFile::Table).to_owned())
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum OpaquePasswordFile {
    Table,
    Id,
    TenantId,
    PasswordFile,
    SuiteVersion,
    CreatedAt,
    UpdatedAt,
}

#[derive(DeriveIden)]
enum Lease {
    Table,
    Id,
    TenantId,
    BearerHash,
    WrappedExportKey,
    IssuedAt,
    ExpiresAt,
    IdleExtendsTo,
    RevokedAt,
}

// Re-declared here (not imported from the v0 migration's file) so
// this migration's `up()` keeps compiling if the v0 `enum Tenant`
// ever gets renamed. SeaORM resolves the iden via the
// `DeriveIden` impl; matching `Table` / `Id` names on a fresh enum
// produces the same SQL — same posture as the agent_session and
// session_worker migrations take.
#[derive(DeriveIden)]
enum Tenant {
    Table,
    Id,
}
