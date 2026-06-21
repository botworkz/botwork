//! Create the `session_worker` table — RFE #105 round-3 / PR3.
//!
//! New, additive: no existing table touched. Lands the per-container
//! incarnation surface that lets session-broker recover routing state
//! after a restart without `/var/lib/botwork/sessions.json` (which
//! goes away in the same round-3 PR on the broker + vm side).
//!
//! See `db/entity/src/session_worker.rs` for the entity-level column
//! semantics and the rationale for why this is a separate table from
//! `agent_session` rather than extra columns on it.
//!
//! ## Column choices
//!
//! Identical posture to the v0 + agent_session migrations:
//!
//! * `id uuid PK DEFAULT gen_random_uuid()` — `pgcrypto` already
//!   enabled by the v0 migration.
//! * `agent_session_id uuid NULL` FK → `agent_session.id` ON DELETE
//!   **CASCADE**. NULL during the spawn-to-first-bind window: the
//!   row is INSERTed when control-plane gates the container in, and
//!   the `agent-session-id` arrives one round-trip later on the first
//!   non-init request. CASCADE because workers are a per-incarnation
//!   projection of the session — deleting the parent session sweeps
//!   the audit history alongside it.
//! * `plugin_id uuid NOT NULL` FK → `plugin.id` ON DELETE
//!   **RESTRICT** — same posture as `workspace_plugin.plugin_id`: a
//!   plugin row with live workers must be admin-explicitly drained
//!   first.
//! * `container_name text NOT NULL` — `mcp_session_<token>`. Globally
//!   unique on a single docker host; we lift that property into a
//!   UNIQUE constraint so cold-start recovery can join `docker ps`
//!   output back to row state with one indexed lookup per container.
//! * `container_ip text NOT NULL` — IPv4 captured from `docker
//!   inspect` after IPAM assignment. session-broker forwards it to
//!   control-plane on the spawn-time gate POST (botwork #81).
//! * `mcp_session_id text NOT NULL DEFAULT ''` — empty string in the
//!   spawn-to-initialize-response window; populated after the
//!   upstream's initialize reply lands. Non-NULL with empty default
//!   (rather than nullable) so equality lookups stay cheap and the
//!   common indexed-lookup path doesn't trip NULL semantics.
//! * `spawned_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP` —
//!   first-ever `docker run` completion. Immutable.
//! * `reaped_at timestamptz NULL` — NULL while the container is live
//!   (or believed-live). Set to a wall-clock timestamp on teardown;
//!   the row then sits for the janitor to retain for an
//!   operator-configurable window before DELETE.
//!
//! ## Index design
//!
//! Three named indexes alongside the implicit PK btree:
//!
//! * `ux_session_worker_container_name` — UNIQUE on `container_name`.
//!   Drives the cold-start `docker ps`→DB reconciliation.
//! * `ux_session_worker_live_per_plugin` — partial UNIQUE on
//!   `(agent_session_id, plugin_id) WHERE reaped_at IS NULL` AND
//!   `agent_session_id IS NOT NULL`. Enforces the "one live container
//!   per agent × plugin" invariant: a second live worker for the
//!   same `(agent_session, plugin)` pair would be unreachable
//!   (session-broker routes by `(plugin, mcp_session_id)`, not by
//!   container name) and would leak. Partial because the audit
//!   history needs multiple incarnations across a session's lifetime
//!   to be legal, and pre-bind rows have `agent_session_id IS NULL`.
//! * `ix_session_worker_live` — non-unique on `reaped_at` where it
//!   IS NULL. The cold-start recovery query
//!   `SELECT … WHERE reaped_at IS NULL` is the only hot path against
//!   the table; this lets it skip the audit row tail without a seq
//!   scan once the table grows.
//!
//! ## Forward-only
//!
//! New table; the `down()` path drops it (clean for tests). v0
//! production migrations are forward-only by convention.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(SessionWorker::Table)
                    .col(
                        ColumnDef::new(SessionWorker::Id)
                            .uuid()
                            .not_null()
                            .primary_key()
                            .default(Expr::cust("gen_random_uuid()")),
                    )
                    .col(ColumnDef::new(SessionWorker::AgentSessionId).uuid().null())
                    .col(uuid(SessionWorker::PluginId))
                    .col(string(SessionWorker::ContainerName))
                    .col(string(SessionWorker::ContainerIp))
                    .col(
                        ColumnDef::new(SessionWorker::McpSessionId)
                            .text()
                            .not_null()
                            .default(""),
                    )
                    .col(
                        timestamp_with_time_zone(SessionWorker::SpawnedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        ColumnDef::new(SessionWorker::ReapedAt)
                            .timestamp_with_time_zone()
                            .null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_session_worker_agent_session")
                            .from(SessionWorker::Table, SessionWorker::AgentSessionId)
                            .to(AgentSession::Table, AgentSession::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_session_worker_plugin")
                            .from(SessionWorker::Table, SessionWorker::PluginId)
                            .to(Plugin::Table, Plugin::Id)
                            .on_delete(ForeignKeyAction::Restrict),
                    )
                    .to_owned(),
            )
            .await?;

        // container_name UNIQUE: cold-start recovery joins docker ps
        // output back to row state via this index.
        manager
            .create_index(
                Index::create()
                    .name("ux_session_worker_container_name")
                    .table(SessionWorker::Table)
                    .col(SessionWorker::ContainerName)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // Partial UNIQUE: one live worker per (agent_session, plugin).
        // SeaORM's IndexCreateStatement doesn't model partial-index
        // predicates, so we fall back to raw SQL via the connection
        // backend. The predicate is `reaped_at IS NULL AND
        // agent_session_id IS NOT NULL` so the constraint only fires
        // for fully-bound, live rows — pre-bind window (agent_session
        // NULL) and audit rows (reaped_at NOT NULL) are exempt.
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE UNIQUE INDEX ux_session_worker_live_per_plugin \
                 ON session_worker (agent_session_id, plugin_id) \
                 WHERE reaped_at IS NULL AND agent_session_id IS NOT NULL",
            )
            .await?;

        // Cold-start recovery query: `WHERE reaped_at IS NULL`.
        // Partial index keeps it cheap as the audit row tail grows.
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE INDEX ix_session_worker_live \
                 ON session_worker (reaped_at) \
                 WHERE reaped_at IS NULL",
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(SessionWorker::Table).to_owned())
            .await?;
        Ok(())
    }
}

// Clippy fires `enum_variant_names` here for the same reason it does
// on the agent_session iden enum: `AgentSessionId` starts with
// `AgentSession`, and `McpSessionId` starts with `SessionWorker`'s
// implied prefix. The variant names are load-bearing — `DeriveIden`
// lowercases them verbatim into column names (`agent_session_id`,
// `mcp_session_id`), which are the column names this PR pins. The
// per-variant override (`#[sea_orm(iden = "…")]`) would only push the
// same name into a decoration string.
#[allow(clippy::enum_variant_names)]
#[derive(DeriveIden)]
enum SessionWorker {
    Table,
    Id,
    AgentSessionId,
    PluginId,
    ContainerName,
    ContainerIp,
    McpSessionId,
    SpawnedAt,
    ReapedAt,
}

// Re-declared here (not imported from the agent_session migration's
// file) so this migration's `up()` keeps compiling if the
// agent_session iden ever moves. SeaORM resolves the iden via the
// `DeriveIden` impl; matching `Table` / `Id` names on a fresh enum
// produces the same SQL.
#[derive(DeriveIden)]
enum AgentSession {
    Table,
    Id,
}

#[derive(DeriveIden)]
enum Plugin {
    Table,
    Id,
}
