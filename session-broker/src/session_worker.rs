//! `session_worker` write-through path for session-broker.
//!
//! RFE #105 round-3 PR2 makes session-broker the single writer of the
//! `session_worker` table that landed in #113. The table tracks one
//! row per spawned plugin container, keyed on `container_name`. See
//! `db/entity/src/session_worker.rs` for the lifecycle and
//! constraint rationale.
//!
//! ## Lifecycle as the broker sees it
//!
//! ```text
//!   spawn_new_container                      ext_proc::handle_response_headers
//!     │                                        │
//!     ▼                                        ▼
//!   INSERT row                              backfill mcp_session_id
//!     (agent_session_id NULL,                 (after the upstream's first
//!      mcp_session_id "")                      response with Mcp-Session-Id)
//!
//!   ext_proc::request_body /bind-agent
//!     │
//!     ▼
//!   backfill agent_session_id
//!     (after the first non-init JSON-RPC call surfaces the
//!      goose agent-session-id)
//!
//!   teardown_session / evict_dead_session /
//!   exit_listener::handle_container_exit
//!     │
//!     ▼
//!   UPDATE reaped_at = now()
//!     (row stays for audit + cost; janitor sweeps later)
//! ```
//!
//! ## Failure posture
//!
//! Same shape `AgentSessionWriter` uses: every method calls into the
//! DB and converts errors into a `warn!`-and-carry-on. The JSON
//! registry is gone in this round, so DB write failures DO mean
//! "we've lost the routing fact for this container until the next
//! cold-start recovery cycle reconstructs it" — that's expected and
//! recoverable. The thing we never do is take session-broker down
//! because postgres flapped.
//!
//! The exception is the INSERT at spawn: if that fails we still
//! return Ok to the caller (the container is up; control-plane has
//! been told; routing the user's first request is more important
//! than the audit row landing). On the next cold-start recovery the
//! container will be reaped because there's no matching row —
//! which IS the agreed posture for "live container with no DB row"
//! (reap-immediately per the design call).

use std::sync::Arc;

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
};
use tracing::warn;
use uuid::Uuid;

use botwork_entity::{agent_session, plugin, session_worker};

const PREFIX: &str = "[session-broker]";

/// Shape of a row we hand to the cold-start recovery path. Mirrors the
/// columns the broker actually needs for in-memory state — the full
/// `session_worker::Model` carries timestamps the recovery path
/// doesn't read.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveWorker {
    pub container_name: String,
    pub container_ip: String,
    pub mcp_session_id: String,
    pub plugin_id: Uuid,
    pub agent_session_id: Option<Uuid>,
}

#[derive(Clone)]
pub struct SessionWorkerWriter {
    db: Arc<DatabaseConnection>,
}

impl SessionWorkerWriter {
    pub fn new(db: Arc<DatabaseConnection>) -> Self {
        Self { db }
    }

    /// INSERT a fresh row at container-spawn time.
    ///
    /// `mcp_session_id` is the empty string here (the upstream's
    /// initialize response hasn't landed yet); we backfill via
    /// [`Self::record_mcp_session_id`] when it does.
    ///
    /// `agent_session_id` is `None` (the agent identity arrives one
    /// round-trip later on the first non-init call); we backfill via
    /// [`Self::record_agent_binding`] when it does.
    pub async fn record_spawn(&self, plugin_name: &str, container_name: &str, container_ip: &str) {
        if let Err(err) = self
            .record_spawn_inner(plugin_name, container_name, container_ip)
            .await
        {
            warn!(
                "{PREFIX} session_worker spawn write failed for \
                 plugin={plugin_name} container={container_name}: {err}"
            );
        }
    }

    async fn record_spawn_inner(
        &self,
        plugin_name: &str,
        container_name: &str,
        container_ip: &str,
    ) -> Result<(), SessionWorkerWriteError> {
        let plugin_id = self.plugin_id(plugin_name).await?;
        session_worker::ActiveModel {
            id: Set(Uuid::new_v4()),
            agent_session_id: Set(None),
            plugin_id: Set(plugin_id),
            container_name: Set(container_name.to_owned()),
            container_ip: Set(container_ip.to_owned()),
            mcp_session_id: Set(String::new()),
            spawned_at: Set(Utc::now()),
            reaped_at: Set(None),
        }
        .insert(self.db.as_ref())
        .await?;
        Ok(())
    }

    /// Backfill `mcp_session_id` after the upstream's initialize
    /// response surfaces it.
    pub async fn record_mcp_session_id(&self, container_name: &str, mcp_session_id: &str) {
        if let Err(err) = self
            .record_mcp_session_id_inner(container_name, mcp_session_id)
            .await
        {
            warn!(
                "{PREFIX} session_worker mcp_session_id backfill failed for \
                 container={container_name} mcp_session_id={mcp_session_id}: {err}"
            );
        }
    }

    async fn record_mcp_session_id_inner(
        &self,
        container_name: &str,
        mcp_session_id: &str,
    ) -> Result<(), SessionWorkerWriteError> {
        let row = self.find_by_container_name(container_name).await?;
        let mut active: session_worker::ActiveModel = row.into();
        active.mcp_session_id = Set(mcp_session_id.to_owned());
        active.update(self.db.as_ref()).await?;
        Ok(())
    }

    /// Backfill `agent_session_id` once goose's first non-init call
    /// surfaces the agent-session-id. The caller has already upserted
    /// the `agent_session` row via [`AgentSessionWriter::record_bind_agent`]
    /// and is handing us the resulting primary key.
    pub async fn record_agent_binding(&self, container_name: &str, agent_session_id: Uuid) {
        if let Err(err) = self
            .record_agent_binding_inner(container_name, agent_session_id)
            .await
        {
            warn!(
                "{PREFIX} session_worker agent_session_id backfill failed for \
                 container={container_name} agent_session_id={agent_session_id}: {err}"
            );
        }
    }

    async fn record_agent_binding_inner(
        &self,
        container_name: &str,
        agent_session_id: Uuid,
    ) -> Result<(), SessionWorkerWriteError> {
        let row = self.find_by_container_name(container_name).await?;
        let mut active: session_worker::ActiveModel = row.into();
        active.agent_session_id = Set(Some(agent_session_id));
        active.update(self.db.as_ref()).await?;
        Ok(())
    }

    /// Mark a container as reaped — UPDATE `reaped_at = now()`. The
    /// row sticks for audit; the janitor (separate RFE) sweeps later.
    pub async fn record_reap(&self, container_name: &str) {
        if let Err(err) = self.record_reap_inner(container_name).await {
            warn!(
                "{PREFIX} session_worker reap write failed for \
                 container={container_name}: {err}"
            );
        }
    }

    async fn record_reap_inner(&self, container_name: &str) -> Result<(), SessionWorkerWriteError> {
        let row = self.find_by_container_name(container_name).await?;
        if row.reaped_at.is_some() {
            // Already reaped — UPDATE would be a no-op and we don't
            // want to bump the timestamp on a duplicate-fire path
            // (the exit listener + teardown grace timer can both
            // converge on the same container).
            return Ok(());
        }
        let mut active: session_worker::ActiveModel = row.into();
        active.reaped_at = Set(Some(Utc::now()));
        active.update(self.db.as_ref()).await?;
        Ok(())
    }

    /// Cold-start recovery query: every row that is currently believed
    /// live (`reaped_at IS NULL`). The broker walks this against
    /// `docker ps --filter name=mcp_session_*` to reconstruct the
    /// in-memory `transport_sessions` map after a restart.
    ///
    /// Rows whose container is no longer running get reaped (see
    /// `recover_live_workers` in `lib.rs`); rows that have a running
    /// container without a matching DB row get the docker container
    /// stopped (the "reap-immediately" posture).
    pub async fn list_live(&self) -> Result<Vec<LiveWorker>, SessionWorkerWriteError> {
        let rows = session_worker::Entity::find()
            .filter(session_worker::Column::ReapedAt.is_null())
            .all(self.db.as_ref())
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| LiveWorker {
                container_name: row.container_name,
                container_ip: row.container_ip,
                mcp_session_id: row.mcp_session_id,
                plugin_id: row.plugin_id,
                agent_session_id: row.agent_session_id,
            })
            .collect())
    }

    /// Resolve `plugin_id` for the broker's spawn-time UPDATE.
    /// Mirrors the agent_session/agent_session_id resolver pattern but
    /// is uncached: plugin rows churn (api can delete one) and
    /// the spawn path is rare enough that one extra SELECT per spawn
    /// isn't on a hot path.
    async fn plugin_id(&self, name: &str) -> Result<Uuid, SessionWorkerWriteError> {
        let row = plugin::Entity::find()
            .filter(plugin::Column::Name.eq(name))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| SessionWorkerWriteError::UnknownPlugin(name.to_owned()))?;
        Ok(row.id)
    }

    async fn find_by_container_name(
        &self,
        container_name: &str,
    ) -> Result<session_worker::Model, SessionWorkerWriteError> {
        session_worker::Entity::find()
            .filter(session_worker::Column::ContainerName.eq(container_name))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| SessionWorkerWriteError::UnknownContainer(container_name.to_owned()))
    }
}

/// Resolve the `agent_session.id` PK for the
/// `(tenant_id, workspace_id, agent_session_id)` triple — the broker
/// uses this to bridge between `AgentSessionWriter::record_bind_agent`
/// (which writes the row keyed on names) and
/// `SessionWorkerWriter::record_agent_binding` (which needs the uuid).
///
/// Returns the row's `id` if found, `None` otherwise. Errors propagate
/// the underlying DB failure for the caller's `warn!`.
pub async fn resolve_agent_session_pk(
    db: &DatabaseConnection,
    tenant_id: Uuid,
    workspace_id: Uuid,
    agent_session_id: &str,
) -> Result<Option<Uuid>, sea_orm::DbErr> {
    Ok(agent_session::Entity::find()
        .filter(agent_session::Column::TenantId.eq(tenant_id))
        .filter(agent_session::Column::WorkspaceId.eq(workspace_id))
        .filter(agent_session::Column::AgentSessionId.eq(agent_session_id))
        .one(db)
        .await?
        .map(|row| row.id))
}

#[derive(Debug, thiserror::Error)]
pub enum SessionWorkerWriteError {
    #[error("unknown plugin: {0}")]
    UnknownPlugin(String),
    #[error("session_worker row not found for container: {0}")]
    UnknownContainer(String),
    #[error("db error: {0}")]
    Db(#[from] sea_orm::DbErr),
}

#[cfg(test)]
mod tests {
    use sea_orm::{DatabaseBackend, DbErr, MockDatabase};

    use super::*;

    #[test]
    fn write_error_display_includes_context() {
        let err = SessionWorkerWriteError::UnknownPlugin("mcp-bash".into());
        assert!(err.to_string().contains("mcp-bash"));

        let err = SessionWorkerWriteError::UnknownContainer("mcp_session_abc".into());
        assert!(err.to_string().contains("mcp_session_abc"));
    }

    #[tokio::test]
    async fn record_spawn_swallows_db_error() {
        let writer = crate::test_support::mock_session_worker_writer(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_errors([DbErr::Custom("boom".to_string())]),
        );

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            writer.record_spawn("mcp-bash", "mcp_session_1", "10.0.0.1"),
        )
        .await
        .expect("writer should warn-and-carry-on on db errors");
    }
}
