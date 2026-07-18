use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::{DatabaseConnection, DbErr};
use uuid::Uuid;

use crate::agent_session::{AgentSessionWriteError, AgentSessionWriter};
use crate::session_worker::{LiveWorker, SessionWorkerWriteError, SessionWorkerWriter};
use crate::store::{AgentSessionStore, SessionWorkerStore};

#[derive(Clone)]
pub struct SeaOrmAgentSessionStore {
    writer: AgentSessionWriter,
}

impl SeaOrmAgentSessionStore {
    /// Create a SeaORM-backed [`AgentSessionStore`] delegating to
    /// [`AgentSessionWriter`].
    pub fn new(db: Arc<DatabaseConnection>) -> Self {
        Self {
            writer: AgentSessionWriter::new(db),
        }
    }
}

#[async_trait]
impl AgentSessionStore for SeaOrmAgentSessionStore {
    async fn record_bind_agent(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Result<(), AgentSessionWriteError> {
        self.writer
            .try_record_bind_agent(tenant_name, workspace_name, agent_session_id)
            .await
    }

    async fn record_grace(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Result<(), AgentSessionWriteError> {
        self.writer
            .try_record_grace(tenant_name, workspace_name, agent_session_id)
            .await
    }

    async fn record_inactive(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Result<(), AgentSessionWriteError> {
        self.writer
            .try_record_inactive(tenant_name, workspace_name, agent_session_id)
            .await
    }

    async fn touch_last_active(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Result<(), AgentSessionWriteError> {
        self.writer
            .try_touch_last_active(tenant_name, workspace_name, agent_session_id)
            .await
    }

    async fn resolve_pk(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Result<Option<Uuid>, AgentSessionWriteError> {
        self.writer
            .try_resolve_pk(tenant_name, workspace_name, agent_session_id)
            .await
    }
}

#[derive(Clone)]
pub struct SeaOrmSessionWorkerStore {
    writer: SessionWorkerWriter,
}

impl SeaOrmSessionWorkerStore {
    /// Create a SeaORM-backed [`SessionWorkerStore`] delegating to
    /// [`SessionWorkerWriter`].
    pub fn new(db: Arc<DatabaseConnection>) -> Self {
        Self {
            writer: SessionWorkerWriter::new(db),
        }
    }
}

#[async_trait]
impl SessionWorkerStore for SeaOrmSessionWorkerStore {
    async fn record_spawn(
        &self,
        plugin_name: &str,
        container_name: &str,
        container_ip: &str,
    ) -> Result<(), SessionWorkerWriteError> {
        self.writer
            .try_record_spawn(plugin_name, container_name, container_ip)
            .await
    }

    async fn record_mcp_session_id(
        &self,
        container_name: &str,
        mcp_session_id: &str,
    ) -> Result<(), SessionWorkerWriteError> {
        self.writer
            .try_record_mcp_session_id(container_name, mcp_session_id)
            .await
    }

    async fn record_agent_binding(
        &self,
        container_name: &str,
        agent_session_id: Uuid,
    ) -> Result<(), SessionWorkerWriteError> {
        self.writer
            .try_record_agent_binding(container_name, agent_session_id)
            .await
    }

    async fn record_reap(&self, container_name: &str) -> Result<(), SessionWorkerWriteError> {
        self.writer.try_record_reap(container_name).await
    }

    async fn list_live(&self) -> Result<Vec<LiveWorker>, SessionWorkerWriteError> {
        self.writer.list_live().await
    }

    async fn resolve_plugin_name(&self, plugin_id: Uuid) -> Result<Option<String>, DbErr> {
        self.writer.resolve_plugin_name(plugin_id).await
    }
}
