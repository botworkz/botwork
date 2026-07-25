//! Create the `invitation` table — OTP-gated tenant claim credentials
//! (botworkz/botwork#346).
//!
//! New, additive: no existing table touched. Lands the table that
//! auth-broker INSERTs when `POST /api/tenants` fires and SELECTs+UPDATEs
//! at `POST /auth/register/finish`.
//!
//! See `db/entity/src/invitation.rs` for the entity-level column semantics.
//!
//! ## Column choices
//!
//! Same posture as the v0 + agent_session + session_worker + auth migrations:
//!
//! * `id uuid PK DEFAULT gen_random_uuid()` — `pgcrypto` is already
//!   enabled by the v0 `create_core_tables` migration so no
//!   `CREATE EXTENSION` is needed here.
//! * `tenant_id uuid NOT NULL` FK → `tenant.id` ON DELETE **CASCADE**.
//!   An invitation without a tenant is meaningless; the two-step
//!   "deliberate tenant delete" posture is still enforced by the
//!   `workspace.tenant_id` RESTRICT FK.
//! * `otp_hash text NOT NULL` — SHA-256 of the normalised plaintext OTP
//!   (dashes stripped, uppercased), hex-encoded. Plaintext never stored.
//!   Text (not bytea) so the hash is human-readable in `psql` introspection.
//! * `expires_at timestamptz NOT NULL` — verification rejects if
//!   `now >= expires_at`.
//! * `consumed_at timestamptz NULL` — `NULL` while unclaimed. The
//!   verify+consume step conditionally UPDATEs to non-NULL; a non-NULL
//!   value is terminal (invitation inert but retained for audit).
//! * `created_at timestamptz DEFAULT CURRENT_TIMESTAMP` — immutable;
//!   useful for operator introspection and future GC reaper.
//!
//! ## Index design
//!
//! One named index alongside the implicit PK btree:
//!
//! * `ix_invitation_tenant_id` — btree on `tenant_id`. Drives the
//!   verify+consume lookup ("which invitations belong to this tenant?")
//!   without a seqscan. Not UNIQUE because an admin may re-issue after
//!   the first expires — multiple rows per tenant are valid.
//!
//! ## Forward-only
//!
//! New table only; the `down()` path drops it cleanly for tests and
//! future operator tooling. v0 production migrations are forward-only
//! by convention; `down` is never run by `botwork-migration` itself.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Invitation::Table)
                    .col(
                        ColumnDef::new(Invitation::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .default(Expr::cust("gen_random_uuid()")),
                    )
                    .col(uuid(Invitation::TenantId))
                    .col(string(Invitation::OtpHash))
                    .col(timestamp_with_time_zone(Invitation::ExpiresAt))
                    .col(
                        ColumnDef::new(Invitation::ConsumedAt)
                            .timestamp_with_time_zone()
                            .null(),
                    )
                    .col(
                        timestamp_with_time_zone(Invitation::CreatedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_invitation_tenant")
                            .from(Invitation::Table, Invitation::TenantId)
                            .to(Tenant::Table, Tenant::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // Btree on tenant_id: drives the verify+consume lookup (which
        // invitations belong to this tenant?) without a seqscan. Not UNIQUE
        // because admins may re-issue after expiry.
        manager
            .create_index(
                Index::create()
                    .name("ix_invitation_tenant_id")
                    .table(Invitation::Table)
                    .col(Invitation::TenantId)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Invitation::Table).to_owned())
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Invitation {
    Table,
    Id,
    TenantId,
    OtpHash,
    ExpiresAt,
    ConsumedAt,
    CreatedAt,
}

// Re-declared here (not imported from the v0 migration's file) so this
// migration's `up()` keeps compiling if the v0 `enum Tenant` ever gets
// renamed. SeaORM resolves the iden via the `DeriveIden` impl; matching
// `Table` / `Id` names on a fresh enum produces the same SQL — same
// posture as the auth-tables and session-worker migrations.
#[derive(DeriveIden)]
enum Tenant {
    Table,
    Id,
}
