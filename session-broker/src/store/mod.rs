use async_trait::async_trait;
use sea_orm::DbErr;
use uuid::Uuid;

use crate::agent_session::AgentSessionWriteError;
use crate::session_worker::{LiveWorker, SessionWorkerWriteError};

pub mod mock;
pub mod sea_orm_impl;

#[async_trait]
pub trait AgentSessionStore: Send + Sync {
    /// Upsert bind-agent state for `(tenant, workspace, agent_session_id)`.
    ///
    /// `Ok(())` on success; `Err` when DB resolution or write fails.
    async fn record_bind_agent(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Result<(), AgentSessionWriteError>;

    /// Transition the row to `grace`.
    ///
    /// `Ok(())` on success; `Err` when lookup/write fails.
    async fn record_grace(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Result<(), AgentSessionWriteError>;

    /// Transition the row to `inactive`.
    ///
    /// `Ok(())` on success; `Err` when lookup/write fails.
    async fn record_inactive(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Result<(), AgentSessionWriteError>;

    /// Update `last_active_at` on an existing row.
    ///
    /// `Ok(())` on success; `Err` when lookup/write fails.
    async fn touch_last_active(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Result<(), AgentSessionWriteError>;

    /// Resolve the `agent_session.id` PK for the identity triple.
    ///
    /// `Ok(Some(id))` when found, `Ok(None)` when missing, `Err` on DB failures.
    async fn resolve_pk(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Result<Option<Uuid>, AgentSessionWriteError>;
}

#[async_trait]
pub trait SessionWorkerStore: Send + Sync {
    /// Insert a session-worker row when container spawn succeeds.
    async fn record_spawn(
        &self,
        plugin_name: &str,
        container_name: &str,
        container_ip: &str,
    ) -> Result<(), SessionWorkerWriteError>;

    /// Backfill row `mcp_session_id`.
    async fn record_mcp_session_id(
        &self,
        container_name: &str,
        mcp_session_id: &str,
    ) -> Result<(), SessionWorkerWriteError>;

    /// Backfill row `agent_session_id`.
    async fn record_agent_binding(
        &self,
        container_name: &str,
        agent_session_id: Uuid,
    ) -> Result<(), SessionWorkerWriteError>;

    /// Mark row reaped.
    async fn record_reap(&self, container_name: &str) -> Result<(), SessionWorkerWriteError>;

    /// Read rows believed live (`reaped_at IS NULL`).
    async fn list_live(&self) -> Result<Vec<LiveWorker>, SessionWorkerWriteError>;

    /// Resolve plugin name for recovery rehydration.
    ///
    /// `Ok(Some(name))` when found, `Ok(None)` when missing, `Err` on DB failure.
    async fn resolve_plugin_name(&self, plugin_id: Uuid) -> Result<Option<String>, DbErr>;
}
