//! Extend `plugin` with the full set of fields the wire `/resolve`
//! response carries: `port`, `path`, `upstream_auth`, `env`,
//! `resources`. RFE #101 PR2.
//!
//! Pre-cutover the plugin row carried only `image` + `egress` because
//! nothing read from the DB. Post-cutover config-broker resolves
//! straight off the row, which means every field session-broker uses
//! to spawn a container has to live here.
//!
//! ## Column choices
//!
//! * `port int NOT NULL DEFAULT 8000` — matches the YAML default.
//!   1..=65535 is enforced at write time by the bootstrap validator;
//!   the DB column is wide enough for the full range and not
//!   constrained further (no CHECK constraint) because the validator
//!   is the gate.
//! * `path text NOT NULL DEFAULT '/'` — same posture, same validator.
//! * `upstream_auth text NOT NULL DEFAULT 'none'` — stores the wire
//!   form verbatim (`"none"` or `"bearer/<service>"`). Bootstrap
//!   parses the YAML shape (`none` / `bearer/<service>`), validates,
//!   and writes the string. config-broker reads the string and re-
//!   parses on `/resolve`. The alternative (`jsonb` `{kind, service}`)
//!   carries no information the text form doesn't and gives nothing
//!   for the cost of an extra parser pair.
//! * `env jsonb NOT NULL DEFAULT '[]'::jsonb` — array of
//!   `{name, value}` objects. Matches what config-broker emits on
//!   the wire today; preserves order across writes (the YAML map
//!   form doesn't, but the validator captures order at parse time).
//! * `resources jsonb NULL` — small `{cpus?, memory?, pids?}` blob.
//!   Nullable rather than `'{}'::jsonb` so an absent block reads as
//!   `None` end-to-end without round-tripping through "is the
//!   object empty".
//!
//! ## Why ALTER, not a fresh CREATE
//!
//! PR1 already shipped to dev; the table holds rows. Even on a
//! deployment where the DB has nothing useful in it yet, a follow-up
//! "create a new schema and migrate the rows over" would be the
//! wrong pattern for a *forward-only* additive schema change.
//!
//! ## Defaults
//!
//! `DEFAULT` clauses cover the column-add for any pre-existing rows.
//! Bootstrap writes every column on every row regardless; the
//! defaults exist for the migration itself, not for ongoing inserts.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Plugin::Table)
                    .add_column(
                        ColumnDef::new(Plugin::Port)
                            .integer()
                            .not_null()
                            .default(8000),
                    )
                    .add_column(ColumnDef::new(Plugin::Path).text().not_null().default("/"))
                    .add_column(
                        ColumnDef::new(Plugin::UpstreamAuth)
                            .text()
                            .not_null()
                            .default("none"),
                    )
                    .add_column(
                        ColumnDef::new(Plugin::Env)
                            .json_binary()
                            .not_null()
                            .default(Expr::cust("'[]'::jsonb")),
                    )
                    .add_column(ColumnDef::new(Plugin::Resources).json_binary().null())
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Plugin::Table)
                    .drop_column(Plugin::Resources)
                    .drop_column(Plugin::Env)
                    .drop_column(Plugin::UpstreamAuth)
                    .drop_column(Plugin::Path)
                    .drop_column(Plugin::Port)
                    .to_owned(),
            )
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Plugin {
    Table,
    Port,
    Path,
    UpstreamAuth,
    Env,
    Resources,
}
