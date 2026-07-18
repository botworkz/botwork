//! `agent_session` write-through path for session-broker.
//!
//! RFE #105 PR2 makes session-broker a *dual* writer: every state
//! transition that mutates `sessions.json` ALSO writes a corresponding
//! row in the postgres `agent_session` table. The JSON path is left in
//! place because:
//!
//! 1. Round-3 (the cutover) is where it gets deleted, and the
//!    deployment cycle benefits from a release that runs both stores
//!    in lockstep so we can compare them before flipping the read
//!    side over.
//! 2. (historic) control-plane used to read `GET /control-plane/sessions`
//!    from the broker for its cold-start recovery sync; the round-3
//!    follow-up moved that read straight to postgres (`session_worker`
//!    JOIN `agent_session`) and the admin endpoint is gone. The
//!    write-through path documented in this file is still what
//!    populates the rows control-plane now reads.
//!
//! ## Failure model
//!
//! Both the DB and the JSON file can fail independently. We choose:
//!
//! * **JSON write failure** → existing behaviour (`log_info` + carry
//!   on). Unchanged from pre-PR2.
//! * **DB write failure** → `warn!` and carry on. The JSON path is
//!   still the authoritative recovery store in this PR; a DB outage
//!   degrades us to "no write-through observability" but does NOT
//!   take down session-broker. This is deliberate: if DB writes
//!   blocked, then the same DB outage that breaks config-broker's
//!   `/resolve` would also block session-broker's spawn path — and we
//!   want spawn to fail cleanly via the existing config-broker 5xx,
//!   not because of an unrelated agent_session UPDATE that came
//!   later.
//! * **Missing DB connection at construction time** → every method
//!   is a quiet no-op. Production always wires the DB
//!   (`AppState::agent_session_writer = Some(_)`); tests that don't
//!   care about the DB pass `None`. This keeps the existing
//!   `tests/ext_proc_test.rs` / `tests/integration_tests.rs` builders
//!   working without forcing a testcontainers postgres on every
//!   unrelated test.
//!
//! ## Wire model
//!
//! One row per `(tenant_id, workspace_id, agent_session_id)` triple,
//! keyed by the natural-key UNIQUE index. session-broker is the only
//! writer; the row is created on first `/bind-agent` and the state
//! column is bumped through the lifecycle by every transition.
//!
//! Container metadata (name, IP, mcp_session_id) is **not** stored on
//! the row — those stay in session-broker's in-memory
//! `transport_sessions` map. The row tracks the *agent session*, not
//! the container incarnation; multiple containers can come and go
//! under one row across reconnects.
//!
//! ## In-memory tenant/workspace name → id cache
//!
//! Every transition needs to resolve `tenant_name` and `workspace_name`
//! (string slugs) to their `uuid` primary keys before it can address
//! `agent_session` rows. Looking those up on every write is wasteful;
//! the names are immutable (bootstrap-owned identity), so we cache
//! them in-process.
//!
//! Cache invalidation is "process restart" — the same shape config-
//! broker uses for its plugin descriptors. If a tenant is deleted out
//! from under us (api, eventually), the cache returns stale ids
//! that the FK constraint will reject; we surface that as a `warn!`
//! and the operator either bounces session-broker or waits for the
//! janitor to age out the inactive row.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
};
use tokio::sync::Mutex;
use tracing::warn;
use uuid::Uuid;

use botwork_entity::{agent_session, tenant, workspace};

const PREFIX: &str = "[session-broker]";

/// Write-through handle to the `agent_session` table.
///
/// Cheap to clone (everything is behind an `Arc`); the cache mutex is
/// only ever held across `SELECT id FROM tenant WHERE name = $1` style
/// reads, which are short.
#[derive(Clone)]
pub struct AgentSessionWriter {
    db: Arc<DatabaseConnection>,
    tenant_ids: Arc<Mutex<HashMap<String, Uuid>>>,
    workspace_ids: Arc<Mutex<HashMap<(Uuid, String), Uuid>>>,
}

impl AgentSessionWriter {
    pub fn new(db: Arc<DatabaseConnection>) -> Self {
        Self {
            db,
            tenant_ids: Arc::new(Mutex::new(HashMap::new())),
            workspace_ids: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Upsert the `(tenant, workspace, agent_session_id)` row in
    /// `state = 'active'` and bump `last_active_at`. Returns silently
    /// on success; logs a `warn!` on any DB failure and carries on
    /// (see module docs for the failure-model rationale).
    ///
    /// Called on every `/bind-agent` from `ext_proc.rs`.
    pub async fn record_bind_agent(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) {
        if let Err(err) = self
            .record_bind_agent_inner(tenant_name, workspace_name, agent_session_id)
            .await
        {
            warn!(
                "{PREFIX} agent_session write-through failed for \
                 tenant={tenant_name} workspace={workspace_name} \
                 agent_session_id={agent_session_id}: {err}"
            );
        }
    }

    async fn record_bind_agent_inner(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Result<(), AgentSessionWriteError> {
        let tenant_id = self.tenant_id(tenant_name).await?;
        let workspace_id = self.workspace_id(tenant_id, workspace_name).await?;
        let now = Utc::now();

        // Natural-key lookup. The UNIQUE index from PR1
        // (`ux_agent_session_natural_key`) keeps us from racing into a
        // duplicate row; this find-then-INSERT-or-UPDATE is the same
        // shape `bootstrap/src/runner.rs::upsert_*` uses.
        let existing = agent_session::Entity::find()
            .filter(agent_session::Column::TenantId.eq(tenant_id))
            .filter(agent_session::Column::WorkspaceId.eq(workspace_id))
            .filter(agent_session::Column::AgentSessionId.eq(agent_session_id))
            .one(self.db.as_ref())
            .await?;

        match existing {
            Some(row) => {
                // Reactivation iff the row was previously inactive /
                // grace. Bumping `reactivation_count` on
                // teardown_requested / purged is meaningless (the row
                // is dead-walking) but harmless; we still update
                // last_active_at so the janitor can observe staleness
                // even after a stuck teardown.
                let is_reactivation = matches!(
                    row.state.as_str(),
                    agent_session::state::INACTIVE | agent_session::state::GRACE
                );
                let mut active: agent_session::ActiveModel = row.clone().into();
                active.state = Set(agent_session::state::ACTIVE.to_owned());
                active.last_active_at = Set(now);
                if is_reactivation {
                    active.reactivation_count = Set(row.reactivation_count.saturating_add(1));
                }
                active.update(self.db.as_ref()).await?;
            }
            None => {
                agent_session::ActiveModel {
                    id: Set(Uuid::new_v4()),
                    tenant_id: Set(tenant_id),
                    workspace_id: Set(workspace_id),
                    agent_session_id: Set(agent_session_id.to_owned()),
                    state: Set(agent_session::state::ACTIVE.to_owned()),
                    created_at: Set(now),
                    last_active_at: Set(now),
                    reactivation_count: Set(0),
                }
                .insert(self.db.as_ref())
                .await?;
            }
        }
        Ok(())
    }

    /// Move the row to `state = 'grace'` and bump `last_active_at`.
    /// Called from the liveness path when the last ext_proc stream
    /// for a session closes and the grace timer arms.
    ///
    /// No-op (with a `warn!`) if the row doesn't exist — that means
    /// the broker came up after a DB outage and the bind-agent write
    /// was the one that dropped. The container itself is fine and
    /// the next `/bind-agent` will create the row.
    pub async fn record_grace(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) {
        self.transition_state(
            tenant_name,
            workspace_name,
            agent_session_id,
            agent_session::state::GRACE,
            "record_grace",
        )
        .await;
    }

    /// Move the row to `state = 'inactive'`. Called by the reap path
    /// once the grace timer fires and the container is torn down.
    pub async fn record_inactive(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) {
        self.transition_state(
            tenant_name,
            workspace_name,
            agent_session_id,
            agent_session::state::INACTIVE,
            "record_inactive",
        )
        .await;
    }

    /// Bump `last_active_at` without changing `state`. Hot-path
    /// observability surface for steady-state `tools/call` traffic;
    /// the janitor needs `last_active_at` to be fresh to apply
    /// `inactive` correctly when a session goes quiet.
    ///
    /// Note this is called on every request body — keep it cheap. A
    /// single `UPDATE ... WHERE ux_agent_session_natural_key` hits
    /// the index, so it's one round-trip.
    pub async fn touch_last_active(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) {
        if let Err(err) = self
            .touch_last_active_inner(tenant_name, workspace_name, agent_session_id)
            .await
        {
            warn!(
                "{PREFIX} agent_session touch_last_active failed for \
                 tenant={tenant_name} workspace={workspace_name} \
                 agent_session_id={agent_session_id}: {err}"
            );
        }
    }

    async fn touch_last_active_inner(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Result<(), AgentSessionWriteError> {
        let tenant_id = self.tenant_id(tenant_name).await?;
        let workspace_id = self.workspace_id(tenant_id, workspace_name).await?;
        let now = Utc::now();
        let existing = agent_session::Entity::find()
            .filter(agent_session::Column::TenantId.eq(tenant_id))
            .filter(agent_session::Column::WorkspaceId.eq(workspace_id))
            .filter(agent_session::Column::AgentSessionId.eq(agent_session_id))
            .one(self.db.as_ref())
            .await?;
        let Some(row) = existing else {
            return Err(AgentSessionWriteError::MissingRow);
        };
        let mut active: agent_session::ActiveModel = row.into();
        active.last_active_at = Set(now);
        active.update(self.db.as_ref()).await?;
        Ok(())
    }

    async fn transition_state(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
        new_state: &str,
        op: &'static str,
    ) {
        if let Err(err) = self
            .transition_state_inner(tenant_name, workspace_name, agent_session_id, new_state)
            .await
        {
            warn!(
                "{PREFIX} agent_session {op} failed for \
                 tenant={tenant_name} workspace={workspace_name} \
                 agent_session_id={agent_session_id}: {err}"
            );
        }
    }

    async fn transition_state_inner(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
        new_state: &str,
    ) -> Result<(), AgentSessionWriteError> {
        let tenant_id = self.tenant_id(tenant_name).await?;
        let workspace_id = self.workspace_id(tenant_id, workspace_name).await?;
        let now = Utc::now();
        let existing = agent_session::Entity::find()
            .filter(agent_session::Column::TenantId.eq(tenant_id))
            .filter(agent_session::Column::WorkspaceId.eq(workspace_id))
            .filter(agent_session::Column::AgentSessionId.eq(agent_session_id))
            .one(self.db.as_ref())
            .await?;
        let Some(row) = existing else {
            return Err(AgentSessionWriteError::MissingRow);
        };
        let mut active: agent_session::ActiveModel = row.into();
        active.state = Set(new_state.to_owned());
        active.last_active_at = Set(now);
        active.update(self.db.as_ref()).await?;
        Ok(())
    }

    async fn tenant_id(&self, name: &str) -> Result<Uuid, AgentSessionWriteError> {
        {
            let cache = self.tenant_ids.lock().await;
            if let Some(id) = cache.get(name) {
                return Ok(*id);
            }
        }
        let row = tenant::Entity::find()
            .filter(tenant::Column::Name.eq(name))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| AgentSessionWriteError::UnknownTenant(name.to_owned()))?;
        self.tenant_ids.lock().await.insert(name.to_owned(), row.id);
        Ok(row.id)
    }

    /// RFE #105 round-3 PR2: expose the `agent_session.id` PK for
    /// the `(tenant, workspace, agent_session_id)` triple after
    /// `record_bind_agent` has run. session-broker uses this to
    /// backfill `session_worker.agent_session_id` without bypassing
    /// the writer's tenant/workspace id cache.
    ///
    /// Returns `None` if any of the rows are missing — caller
    /// `warn!`s + carries on, same shape as every other DB write
    /// in this module.
    pub async fn resolve_pk(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Option<Uuid> {
        let tenant_id = match self.tenant_id(tenant_name).await {
            Ok(id) => id,
            Err(err) => {
                warn!(
                    "{PREFIX} resolve_pk tenant lookup failed for \
                     tenant={tenant_name}: {err}"
                );
                return None;
            }
        };
        let workspace_id = match self.workspace_id(tenant_id, workspace_name).await {
            Ok(id) => id,
            Err(err) => {
                warn!(
                    "{PREFIX} resolve_pk workspace lookup failed for \
                     tenant={tenant_name} workspace={workspace_name}: {err}"
                );
                return None;
            }
        };
        match agent_session::Entity::find()
            .filter(agent_session::Column::TenantId.eq(tenant_id))
            .filter(agent_session::Column::WorkspaceId.eq(workspace_id))
            .filter(agent_session::Column::AgentSessionId.eq(agent_session_id))
            .one(self.db.as_ref())
            .await
        {
            Ok(Some(row)) => Some(row.id),
            Ok(None) => {
                warn!(
                    "{PREFIX} resolve_pk found no agent_session row for \
                     tenant={tenant_name} workspace={workspace_name} \
                     agent_session_id={agent_session_id}"
                );
                None
            }
            Err(err) => {
                warn!(
                    "{PREFIX} resolve_pk DB query failed for \
                     tenant={tenant_name} workspace={workspace_name} \
                     agent_session_id={agent_session_id}: {err}"
                );
                None
            }
        }
    }

    async fn workspace_id(
        &self,
        tenant_id: Uuid,
        name: &str,
    ) -> Result<Uuid, AgentSessionWriteError> {
        let key = (tenant_id, name.to_owned());
        {
            let cache = self.workspace_ids.lock().await;
            if let Some(id) = cache.get(&key) {
                return Ok(*id);
            }
        }
        let row = workspace::Entity::find()
            .filter(workspace::Column::TenantId.eq(tenant_id))
            .filter(workspace::Column::Name.eq(name))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| AgentSessionWriteError::UnknownWorkspace {
                tenant_id,
                name: name.to_owned(),
            })?;
        self.workspace_ids.lock().await.insert(key, row.id);
        Ok(row.id)
    }
}

#[derive(Debug, thiserror::Error)]
enum AgentSessionWriteError {
    #[error("unknown tenant: {0}")]
    UnknownTenant(String),
    #[error("unknown workspace under tenant {tenant_id}: {name}")]
    UnknownWorkspace { tenant_id: Uuid, name: String },
    /// Row was expected to exist for a transition (record_grace /
    /// record_inactive / touch_last_active) but didn't. Most likely
    /// the bind-agent write that creates the row dropped during a
    /// transient DB outage. Surfaced as a warn upstream; the next
    /// bind-agent on this session will recreate the row.
    #[error("agent_session row not found")]
    MissingRow,
    #[error("db error: {0}")]
    Db(#[from] sea_orm::DbErr),
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sea_orm::{DatabaseBackend, DbErr, MockDatabase};

    use super::*;
    use botwork_entity::{agent_session, tenant, workspace};

    fn tenant_row(id: Uuid, name: &str) -> tenant::Model {
        tenant::Model {
            id,
            name: name.to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn workspace_row(id: Uuid, tenant_id: Uuid, name: &str) -> workspace::Model {
        workspace::Model {
            id,
            tenant_id,
            name: name.to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn agent_session_row(
        id: Uuid,
        tenant_id: Uuid,
        workspace_id: Uuid,
        agent_session_id: &str,
        state: &str,
    ) -> agent_session::Model {
        agent_session::Model {
            id,
            tenant_id,
            workspace_id,
            agent_session_id: agent_session_id.to_string(),
            state: state.to_string(),
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            reactivation_count: 0,
        }
    }

    #[test]
    fn write_error_display_includes_context() {
        let err = AgentSessionWriteError::UnknownTenant("phlax".into());
        assert!(
            err.to_string().contains("phlax"),
            "unknown-tenant error should name the missing tenant"
        );

        let id = Uuid::nil();
        let err = AgentSessionWriteError::UnknownWorkspace {
            tenant_id: id,
            name: "mcp".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("mcp"), "workspace error should name the slug");
        assert!(msg.contains(&id.to_string()), "and the tenant uuid");

        let err = AgentSessionWriteError::MissingRow;
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn record_bind_agent_swallows_db_error() {
        let writer = crate::test_support::mock_agent_session_writer(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_errors([DbErr::Custom("boom".to_string())]),
        );

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            writer.record_bind_agent("phlax", "mcp", "agent-1"),
        )
        .await
        .expect("writer should warn-and-carry-on on db errors");
    }

    #[tokio::test]
    async fn record_bind_agent_inserts_when_row_missing() {
        let tenant_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let writer = crate::test_support::mock_agent_session_writer(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![tenant_row(tenant_id, "phlax")]])
                .append_query_results([vec![workspace_row(workspace_id, tenant_id, "mcp")]])
                .append_query_results([Vec::<agent_session::Model>::new()]),
        );

        writer.record_bind_agent("phlax", "mcp", "agent-1").await;
    }

    #[tokio::test]
    async fn record_bind_agent_updates_existing_row() {
        let tenant_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let existing_id = Uuid::new_v4();
        let writer = crate::test_support::mock_agent_session_writer(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![tenant_row(tenant_id, "phlax")]])
                .append_query_results([vec![workspace_row(workspace_id, tenant_id, "mcp")]])
                .append_query_results([vec![agent_session_row(
                    existing_id,
                    tenant_id,
                    workspace_id,
                    "agent-1",
                    agent_session::state::INACTIVE,
                )]]),
        );

        writer.record_bind_agent("phlax", "mcp", "agent-1").await;
    }

    #[tokio::test]
    async fn record_grace_swallows_missing_row() {
        let tenant_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let writer = crate::test_support::mock_agent_session_writer(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![tenant_row(tenant_id, "phlax")]])
                .append_query_results([vec![workspace_row(workspace_id, tenant_id, "mcp")]])
                .append_query_results([Vec::<agent_session::Model>::new()]),
        );

        writer.record_grace("phlax", "mcp", "agent-1").await;
    }

    #[tokio::test]
    async fn record_inactive_updates_existing_row() {
        let tenant_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let existing_id = Uuid::new_v4();
        let writer = crate::test_support::mock_agent_session_writer(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![tenant_row(tenant_id, "phlax")]])
                .append_query_results([vec![workspace_row(workspace_id, tenant_id, "mcp")]])
                .append_query_results([vec![agent_session_row(
                    existing_id,
                    tenant_id,
                    workspace_id,
                    "agent-1",
                    agent_session::state::GRACE,
                )]]),
        );

        writer.record_inactive("phlax", "mcp", "agent-1").await;
    }

    #[tokio::test]
    async fn touch_last_active_swallows_missing_row() {
        let tenant_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let writer = crate::test_support::mock_agent_session_writer(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![tenant_row(tenant_id, "phlax")]])
                .append_query_results([vec![workspace_row(workspace_id, tenant_id, "mcp")]])
                .append_query_results([Vec::<agent_session::Model>::new()]),
        );

        writer.touch_last_active("phlax", "mcp", "agent-1").await;
    }

    #[tokio::test]
    async fn resolve_pk_success_and_none_paths() {
        let tenant_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let existing_id = Uuid::new_v4();
        let writer = crate::test_support::mock_agent_session_writer(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![tenant_row(tenant_id, "phlax")]])
                .append_query_results([vec![workspace_row(workspace_id, tenant_id, "mcp")]])
                .append_query_results([vec![agent_session_row(
                    existing_id,
                    tenant_id,
                    workspace_id,
                    "agent-1",
                    agent_session::state::ACTIVE,
                )]])
                .append_query_results([Vec::<agent_session::Model>::new()]),
        );

        let found = writer.resolve_pk("phlax", "mcp", "agent-1").await;
        assert_eq!(found, Some(existing_id));

        let missing = writer.resolve_pk("phlax", "mcp", "agent-2").await;
        assert_eq!(missing, None);
    }

    #[tokio::test]
    async fn resolve_pk_returns_none_on_db_error() {
        let writer = crate::test_support::mock_agent_session_writer(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_errors([DbErr::Custom("boom".to_string())]),
        );

        let resolved = writer.resolve_pk("phlax", "mcp", "agent-1").await;
        assert_eq!(resolved, None);
    }
}
