//! Create the `agent_session` table — RFE #105 PR1.
//!
//! New, additive: no existing table touched. Lands the durable
//! identity for goose agent sessions (one row per
//! `(tenant_id, workspace_id, agent_session_id)` triple) plus the
//! indexes session-broker + the future janitor need to read it
//! efficiently.
//!
//! See `db/entity/src/agent_session.rs` for the entity-level column
//! semantics and the lifecycle.
//!
//! ## Column choices
//!
//! * `id uuid PK DEFAULT gen_random_uuid()` — same pattern as every
//!   other v0 table; `pgcrypto` is already enabled by the v0
//!   `create_core_tables` migration so no `CREATE EXTENSION` is
//!   needed here.
//! * `tenant_id uuid NOT NULL` FK → `tenant.id` ON DELETE **CASCADE**.
//!   Agent sessions are a secondary projection of the tenant; deleting
//!   the tenant must wipe them. (The two-step "deliberate tenant
//!   delete" posture is enforced at the workspace layer, see
//!   `tenant.rs`.)
//! * `workspace_id uuid NOT NULL` FK → `workspace.id` ON DELETE
//!   **CASCADE**. Same reason — the workspace owns the on-disk
//!   `/workspace` directory, the row mirrors it.
//! * `agent_session_id text NOT NULL` — the goose-supplied identity.
//!   Stored verbatim; shape-validated at the wire boundary
//!   (session-broker) before insert.
//! * `state text NOT NULL` — one of `agent_session::state::ALL`. v0
//!   declines a `CHECK` constraint for the same reason
//!   `plugin.upstream_auth` does: the writer (session-broker, single
//!   producer) is the gate, and a future migration that grows the
//!   value set is easier without a constraint to drop+recreate.
//! * `created_at timestamptz DEFAULT CURRENT_TIMESTAMP` — first spawn.
//!   Immutable after insert.
//! * `last_active_at timestamptz DEFAULT CURRENT_TIMESTAMP` — bumped
//!   on every steady-state request.
//! * `reactivation_count int NOT NULL DEFAULT 0` — bumped on every
//!   `inactive → active`. Operator-visible only in v0.
//!
//! ## Index design
//!
//! Two named indexes alongside the implicit PK btree:
//!
//! * `ux_agent_session_natural_key` — UNIQUE on
//!   `(tenant_id, workspace_id, agent_session_id)`. This is the
//!   natural key session-broker reads on every `/bind-agent` to
//!   answer "have I seen this triple before?" The unique index also
//!   enforces the invariant that two rows cannot describe the same
//!   logical agent session.
//! * `ix_agent_session_state_last_active` — non-unique on
//!   `(state, last_active_at)`. Drives the future janitor's
//!   "`state IN ('grace','inactive') AND last_active_at < now() - $idle`"
//!   sweep, and the control-plane "`state IN ('active','grace')`"
//!   read at startup (RFE #105 PR2). Lookups on this index are
//!   bounded by `state` cardinality (5 values), so a btree is fine.
//!
//! ## Forward-only
//!
//! New table; the `down()` path drops it (clean for tests). v0
//! production migrations are forward-only by convention; `down` is
//! never run by `botwork-migration` itself, but kept for
//! `Migrator::down` driven by tests + future operator tooling.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(AgentSession::Table)
                    .col(
                        ColumnDef::new(AgentSession::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .default(Expr::cust("gen_random_uuid()")),
                    )
                    .col(uuid(AgentSession::TenantId))
                    .col(uuid(AgentSession::WorkspaceId))
                    .col(string(AgentSession::AgentSessionId))
                    .col(string(AgentSession::State))
                    .col(
                        timestamp_with_time_zone(AgentSession::CreatedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        timestamp_with_time_zone(AgentSession::LastActiveAt)
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        ColumnDef::new(AgentSession::ReactivationCount)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_agent_session_tenant")
                            .from(AgentSession::Table, AgentSession::TenantId)
                            .to(Tenant::Table, Tenant::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_agent_session_workspace")
                            .from(AgentSession::Table, AgentSession::WorkspaceId)
                            .to(Workspace::Table, Workspace::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // Natural-key UNIQUE. session-broker reads this on every
        // /bind-agent; the unique constraint is also the invariant
        // that prevents duplicate logical sessions.
        manager
            .create_index(
                Index::create()
                    .name("ux_agent_session_natural_key")
                    .table(AgentSession::Table)
                    .col(AgentSession::TenantId)
                    .col(AgentSession::WorkspaceId)
                    .col(AgentSession::AgentSessionId)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // GC sweep + control-plane "alive sessions" read. See module
        // docs for the queries this serves.
        manager
            .create_index(
                Index::create()
                    .name("ix_agent_session_state_last_active")
                    .table(AgentSession::Table)
                    .col(AgentSession::State)
                    .col(AgentSession::LastActiveAt)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(AgentSession::Table).to_owned())
            .await?;
        Ok(())
    }
}

// Clippy fires `enum_variant_names` here because `AgentSessionId`
// starts with the enum name `AgentSession`. The variant name is
// load-bearing: `DeriveIden` maps it to the column name verbatim
// (`AgentSessionId` -> `agent_session_id`), and `agent_session_id`
// is the column name pinned by RFE #105. Renaming the variant would
// force a `#[sea_orm(iden = "agent_session_id")]` override for no
// semantic benefit — the allow expresses the intent directly.
#[allow(clippy::enum_variant_names)]
#[derive(DeriveIden)]
enum AgentSession {
    Table,
    Id,
    TenantId,
    WorkspaceId,
    AgentSessionId,
    State,
    CreatedAt,
    LastActiveAt,
    ReactivationCount,
}

// Re-declared here (not imported from the previous migration's file) so
// the constraint references in this migration's `up()` survive even if
// the v0 migration's `enum Tenant` ever gets renamed. SeaORM resolves
// the iden via the `DeriveIden` impl, and matching `Table` / `Id`
// names on a fresh enum produces the same SQL.
#[derive(DeriveIden)]
enum Tenant {
    Table,
    Id,
}

#[derive(DeriveIden)]
enum Workspace {
    Table,
    Id,
}
