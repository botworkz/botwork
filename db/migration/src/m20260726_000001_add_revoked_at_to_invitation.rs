//! Add `revoked_at` column to the `invitation` table.
//!
//! Extends the invitation lifecycle with an explicit revocation state so
//! admins can invalidate outstanding invitations without waiting for natural
//! expiry (botworkz/botwork#346 follow-up).
//!
//! ## Why a new column
//!
//! The existing terminal state is `consumed_at IS NOT NULL` (single-use claim).
//! Expiry-in-the-past was the alternative revocation mechanism considered, but
//! that loses the audit distinction between "expired naturally" and
//! "admin-killed". A separate `revoked_at` column is consistent with the
//! `consumed_at`-retained-for-audit posture: revoked rows accumulate like
//! consumed ones until the (deferred) GC reaper sweeps them.
//!
//! ## Effect on active-invitation semantics
//!
//! After this migration, an invitation is "active" iff:
//!   `consumed_at IS NULL AND expires_at > now AND revoked_at IS NULL`
//!
//! `verify_and_consume` treats `revoked_at IS NOT NULL` as invalid (same as
//! no-match) so a revoked OTP cannot be used to register.
//!
//! ## Forward-only
//!
//! The `down()` path drops the column cleanly for tests and future operator
//! tooling. v0 production migrations are forward-only by convention.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Invitation::Table)
                    .add_column(
                        ColumnDef::new(Invitation::RevokedAt)
                            .timestamp_with_time_zone()
                            .null(),
                    )
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Invitation::Table)
                    .drop_column(Invitation::RevokedAt)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }
}

#[derive(DeriveIden)]
enum Invitation {
    Table,
    RevokedAt,
}
