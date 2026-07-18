use std::sync::Arc;

use async_trait::async_trait;
use botwork_entity::{agent_session, plugin, session_worker, tenant, workspace, workspace_plugin};
use chrono::{DateTime, Utc};
use sea_orm::{DatabaseConnection, DbErr};
use uuid::Uuid;

use crate::handler::ApiError;
use crate::store::ApiStore;
use crate::{read, write};

#[derive(Clone)]
pub struct SeaOrmApiStore {
    pub db: Arc<DatabaseConnection>,
}

impl SeaOrmApiStore {
    pub fn new(db: Arc<DatabaseConnection>) -> Self {
        Self { db }
    }
}

#[async_trait]
impl ApiStore for SeaOrmApiStore {
    async fn resolve_tenant_id(&self, tenant_name: &str) -> Result<Option<Uuid>, DbErr> {
        read::db_resolve_tenant_id(&self.db, tenant_name).await
    }

    async fn list_tenants(&self) -> Result<Vec<tenant::Model>, DbErr> {
        read::db_list_tenants(&self.db).await
    }

    async fn get_tenant(&self, id: Uuid) -> Result<Option<tenant::Model>, DbErr> {
        read::db_get_tenant(&self.db, id).await
    }

    async fn list_workspaces(
        &self,
        tenant_id: Uuid,
        workspace_id: Option<Uuid>,
    ) -> Result<Vec<workspace::Model>, DbErr> {
        read::db_list_workspaces(&self.db, tenant_id, workspace_id).await
    }

    async fn get_workspace(&self, id: Uuid) -> Result<Option<workspace::Model>, DbErr> {
        read::db_get_workspace(&self.db, id).await
    }

    async fn list_plugins(&self) -> Result<Vec<plugin::Model>, DbErr> {
        read::db_list_plugins(&self.db).await
    }

    async fn get_plugin(&self, id: Uuid) -> Result<Option<plugin::Model>, DbErr> {
        read::db_get_plugin(&self.db, id).await
    }

    async fn list_workspace_ids_for_tenant(&self, tenant_id: Uuid) -> Result<Vec<Uuid>, DbErr> {
        read::db_list_workspace_ids_for_tenant(&self.db, tenant_id).await
    }

    async fn list_workspace_plugins(
        &self,
        workspace_ids: Vec<Uuid>,
        workspace_id: Option<Uuid>,
        plugin_id: Option<Uuid>,
    ) -> Result<Vec<workspace_plugin::Model>, DbErr> {
        read::db_list_workspace_plugins(&self.db, workspace_ids, workspace_id, plugin_id).await
    }

    async fn get_workspace_plugin(
        &self,
        workspace_id: Uuid,
        plugin_id: Uuid,
    ) -> Result<Option<workspace_plugin::Model>, DbErr> {
        read::db_get_workspace_plugin(&self.db, workspace_id, plugin_id).await
    }

    async fn list_agent_sessions(
        &self,
        tenant_id: Uuid,
        workspace_id: Option<Uuid>,
        state: Option<String>,
    ) -> Result<Vec<agent_session::Model>, DbErr> {
        read::db_list_agent_sessions(&self.db, tenant_id, workspace_id, state).await
    }

    async fn get_agent_session(&self, id: Uuid) -> Result<Option<agent_session::Model>, DbErr> {
        read::db_get_agent_session(&self.db, id).await
    }

    async fn list_agent_session_ids_for_tenant(&self, tenant_id: Uuid) -> Result<Vec<Uuid>, DbErr> {
        read::db_list_agent_session_ids_for_tenant(&self.db, tenant_id).await
    }

    async fn list_session_workers(
        &self,
        session_ids: Vec<Uuid>,
        agent_session_id: Option<Uuid>,
        plugin_id: Option<Uuid>,
        live: Option<bool>,
    ) -> Result<Vec<session_worker::Model>, DbErr> {
        read::db_list_session_workers(&self.db, session_ids, agent_session_id, plugin_id, live)
            .await
    }

    async fn get_session_worker(&self, id: Uuid) -> Result<Option<session_worker::Model>, DbErr> {
        read::db_get_session_worker(&self.db, id).await
    }

    async fn create_tenant(&self, name: String) -> Result<tenant::Model, ApiError> {
        write::db_create_tenant(&self.db, name).await
    }

    async fn update_tenant(
        &self,
        id: Uuid,
        name: String,
        if_unmodified_since: DateTime<Utc>,
    ) -> Result<tenant::Model, ApiError> {
        write::db_update_tenant(&self.db, id, name, if_unmodified_since).await
    }

    async fn delete_tenant(
        &self,
        id: Uuid,
        if_unmodified_since: Option<DateTime<Utc>>,
    ) -> Result<tenant::Model, ApiError> {
        write::db_delete_tenant(&self.db, id, if_unmodified_since).await
    }
}
