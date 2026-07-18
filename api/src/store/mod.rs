use async_trait::async_trait;
use botwork_entity::{agent_session, plugin, session_worker, tenant, workspace, workspace_plugin};
use chrono::{DateTime, Utc};
use sea_orm::DbErr;
use uuid::Uuid;

use crate::handler::ApiError;

pub mod mock;
pub mod sea_orm_impl;

#[async_trait]
pub trait ApiStore: Send + Sync {
    /// Resolve a tenant name to its UUID.
    ///
    /// `Ok(Some(id))` when tenant exists, `Ok(None)` when missing, `Err` on DB failure.
    async fn resolve_tenant_id(&self, tenant_name: &str) -> Result<Option<Uuid>, DbErr>;

    /// List all tenants ordered by name.
    ///
    /// `Ok(items)` on success, `Err` on DB failure.
    async fn list_tenants(&self) -> Result<Vec<tenant::Model>, DbErr>;

    /// Get a tenant by UUID.
    ///
    /// `Ok(Some(row))` when found, `Ok(None)` when missing, `Err` on DB failure.
    async fn get_tenant(&self, id: Uuid) -> Result<Option<tenant::Model>, DbErr>;

    /// List workspaces for a tenant UUID with optional workspace-id filter.
    async fn list_workspaces(
        &self,
        tenant_id: Uuid,
        workspace_id: Option<Uuid>,
    ) -> Result<Vec<workspace::Model>, DbErr>;

    /// Get a workspace by UUID.
    async fn get_workspace(&self, id: Uuid) -> Result<Option<workspace::Model>, DbErr>;

    /// List plugins ordered by name.
    async fn list_plugins(&self) -> Result<Vec<plugin::Model>, DbErr>;

    /// Get plugin by UUID.
    async fn get_plugin(&self, id: Uuid) -> Result<Option<plugin::Model>, DbErr>;

    /// List workspace IDs owned by a tenant UUID.
    async fn list_workspace_ids_for_tenant(&self, tenant_id: Uuid) -> Result<Vec<Uuid>, DbErr>;

    /// List workspace-plugin bindings for workspace IDs with optional filters.
    async fn list_workspace_plugins(
        &self,
        workspace_ids: Vec<Uuid>,
        workspace_id: Option<Uuid>,
        plugin_id: Option<Uuid>,
    ) -> Result<Vec<workspace_plugin::Model>, DbErr>;

    /// Get workspace-plugin by composite key.
    async fn get_workspace_plugin(
        &self,
        workspace_id: Uuid,
        plugin_id: Uuid,
    ) -> Result<Option<workspace_plugin::Model>, DbErr>;

    /// List agent sessions for tenant UUID with optional filters.
    async fn list_agent_sessions(
        &self,
        tenant_id: Uuid,
        workspace_id: Option<Uuid>,
        state: Option<String>,
    ) -> Result<Vec<agent_session::Model>, DbErr>;

    /// Get agent session by UUID.
    async fn get_agent_session(&self, id: Uuid) -> Result<Option<agent_session::Model>, DbErr>;

    /// List agent-session IDs owned by tenant UUID.
    async fn list_agent_session_ids_for_tenant(&self, tenant_id: Uuid) -> Result<Vec<Uuid>, DbErr>;

    /// List session workers for session IDs with optional filters.
    async fn list_session_workers(
        &self,
        session_ids: Vec<Uuid>,
        agent_session_id: Option<Uuid>,
        plugin_id: Option<Uuid>,
        live: Option<bool>,
    ) -> Result<Vec<session_worker::Model>, DbErr>;

    /// Get session worker by UUID.
    async fn get_session_worker(&self, id: Uuid) -> Result<Option<session_worker::Model>, DbErr>;

    /// Create tenant with case-insensitive uniqueness checks.
    async fn create_tenant(&self, name: String) -> Result<tenant::Model, ApiError>;

    /// Update tenant with optimistic lock and case-insensitive uniqueness checks.
    async fn update_tenant(
        &self,
        id: Uuid,
        name: String,
        if_unmodified_since: DateTime<Utc>,
    ) -> Result<tenant::Model, ApiError>;

    /// Delete tenant with optional optimistic lock and dependency preflight.
    ///
    /// Returns deleted live row for auditing.
    async fn delete_tenant(
        &self,
        id: Uuid,
        if_unmodified_since: Option<DateTime<Utc>>,
    ) -> Result<tenant::Model, ApiError>;
}
