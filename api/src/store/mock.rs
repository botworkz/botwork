use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use botwork_entity::{agent_session, plugin, session_worker, tenant, workspace, workspace_plugin};
use chrono::{DateTime, Utc};
use sea_orm::DbErr;
use uuid::Uuid;

use crate::handler::{ApiError, ApiErrorExt};
use crate::store::ApiStore;
use crate::write::check_lock;

#[derive(Default)]
struct Inner {
    tenants: HashMap<Uuid, tenant::Model>,
    workspaces: HashMap<Uuid, workspace::Model>,
    plugins: HashMap<Uuid, plugin::Model>,
    workspace_plugins: HashMap<(Uuid, Uuid), workspace_plugin::Model>,
    agent_sessions: HashMap<Uuid, agent_session::Model>,
    session_workers: HashMap<Uuid, session_worker::Model>,
    created_tenants: Vec<Uuid>,
    updated_tenants: Vec<Uuid>,
    deleted_tenants: Vec<Uuid>,
}

#[derive(Clone, Default)]
pub struct MockApiStore {
    inner: Arc<Mutex<Inner>>,
    always_error: Option<String>,
}

impl MockApiStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn always_error(msg: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            always_error: Some(msg.into()),
        }
    }

    pub fn with_tenant(self, row: tenant::Model) -> Self {
        self.inner.lock().expect("lock").tenants.insert(row.id, row);
        self
    }

    pub fn with_workspace(self, row: workspace::Model) -> Self {
        self.inner
            .lock()
            .expect("lock")
            .workspaces
            .insert(row.id, row);
        self
    }

    pub fn with_plugin(self, row: plugin::Model) -> Self {
        self.inner.lock().expect("lock").plugins.insert(row.id, row);
        self
    }

    pub fn with_workspace_plugin(self, row: workspace_plugin::Model) -> Self {
        self.inner
            .lock()
            .expect("lock")
            .workspace_plugins
            .insert((row.workspace_id, row.plugin_id), row);
        self
    }

    pub fn with_agent_session(self, row: agent_session::Model) -> Self {
        self.inner
            .lock()
            .expect("lock")
            .agent_sessions
            .insert(row.id, row);
        self
    }

    pub fn with_session_worker(self, row: session_worker::Model) -> Self {
        self.inner
            .lock()
            .expect("lock")
            .session_workers
            .insert(row.id, row);
        self
    }

    pub async fn drain_created_tenants(&self) -> Vec<Uuid> {
        let mut inner = self.inner.lock().expect("lock");
        std::mem::take(&mut inner.created_tenants)
    }

    pub async fn drain_updated_tenants(&self) -> Vec<Uuid> {
        let mut inner = self.inner.lock().expect("lock");
        std::mem::take(&mut inner.updated_tenants)
    }

    pub async fn drain_deleted_tenants(&self) -> Vec<Uuid> {
        let mut inner = self.inner.lock().expect("lock");
        std::mem::take(&mut inner.deleted_tenants)
    }

    fn maybe_db_err(&self) -> Option<DbErr> {
        self.always_error
            .as_ref()
            .map(|msg| DbErr::Custom(msg.to_string()))
    }
}

#[async_trait]
impl ApiStore for MockApiStore {
    async fn resolve_tenant_id(&self, tenant_name: &str) -> Result<Option<Uuid>, DbErr> {
        if let Some(err) = self.maybe_db_err() {
            return Err(err);
        }
        let inner = self.inner.lock().expect("lock");
        Ok(inner
            .tenants
            .values()
            .find(|t| t.name == tenant_name)
            .map(|t| t.id))
    }

    async fn list_tenants(&self) -> Result<Vec<tenant::Model>, DbErr> {
        if let Some(err) = self.maybe_db_err() {
            return Err(err);
        }
        let mut items: Vec<_> = self
            .inner
            .lock()
            .expect("lock")
            .tenants
            .values()
            .cloned()
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(items)
    }

    async fn get_tenant(&self, id: Uuid) -> Result<Option<tenant::Model>, DbErr> {
        if let Some(err) = self.maybe_db_err() {
            return Err(err);
        }
        Ok(self.inner.lock().expect("lock").tenants.get(&id).cloned())
    }

    async fn list_workspaces(
        &self,
        tenant_id: Uuid,
        workspace_id: Option<Uuid>,
    ) -> Result<Vec<workspace::Model>, DbErr> {
        if let Some(err) = self.maybe_db_err() {
            return Err(err);
        }
        let mut items: Vec<_> = self
            .inner
            .lock()
            .expect("lock")
            .workspaces
            .values()
            .filter(|w| w.tenant_id == tenant_id && workspace_id.is_none_or(|id| w.id == id))
            .cloned()
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(items)
    }

    async fn get_workspace(&self, id: Uuid) -> Result<Option<workspace::Model>, DbErr> {
        if let Some(err) = self.maybe_db_err() {
            return Err(err);
        }
        Ok(self
            .inner
            .lock()
            .expect("lock")
            .workspaces
            .get(&id)
            .cloned())
    }

    async fn list_plugins(&self) -> Result<Vec<plugin::Model>, DbErr> {
        if let Some(err) = self.maybe_db_err() {
            return Err(err);
        }
        let mut items: Vec<_> = self
            .inner
            .lock()
            .expect("lock")
            .plugins
            .values()
            .cloned()
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(items)
    }

    async fn get_plugin(&self, id: Uuid) -> Result<Option<plugin::Model>, DbErr> {
        if let Some(err) = self.maybe_db_err() {
            return Err(err);
        }
        Ok(self.inner.lock().expect("lock").plugins.get(&id).cloned())
    }

    async fn list_workspace_ids_for_tenant(&self, tenant_id: Uuid) -> Result<Vec<Uuid>, DbErr> {
        if let Some(err) = self.maybe_db_err() {
            return Err(err);
        }
        Ok(self
            .inner
            .lock()
            .expect("lock")
            .workspaces
            .values()
            .filter(|w| w.tenant_id == tenant_id)
            .map(|w| w.id)
            .collect())
    }

    async fn list_workspace_plugins(
        &self,
        workspace_ids: Vec<Uuid>,
        workspace_id: Option<Uuid>,
        plugin_id: Option<Uuid>,
    ) -> Result<Vec<workspace_plugin::Model>, DbErr> {
        if let Some(err) = self.maybe_db_err() {
            return Err(err);
        }
        let mut items: Vec<_> = self
            .inner
            .lock()
            .expect("lock")
            .workspace_plugins
            .values()
            .filter(|wp| workspace_ids.contains(&wp.workspace_id))
            .filter(|wp| workspace_id.is_none_or(|id| wp.workspace_id == id))
            .filter(|wp| plugin_id.is_none_or(|id| wp.plugin_id == id))
            .cloned()
            .collect();
        items.sort_by_key(|wp| (wp.workspace_id, wp.plugin_id));
        Ok(items)
    }

    async fn get_workspace_plugin(
        &self,
        workspace_id: Uuid,
        plugin_id: Uuid,
    ) -> Result<Option<workspace_plugin::Model>, DbErr> {
        if let Some(err) = self.maybe_db_err() {
            return Err(err);
        }
        Ok(self
            .inner
            .lock()
            .expect("lock")
            .workspace_plugins
            .get(&(workspace_id, plugin_id))
            .cloned())
    }

    async fn list_agent_sessions(
        &self,
        tenant_id: Uuid,
        workspace_id: Option<Uuid>,
        state: Option<String>,
    ) -> Result<Vec<agent_session::Model>, DbErr> {
        if let Some(err) = self.maybe_db_err() {
            return Err(err);
        }
        let mut items: Vec<_> = self
            .inner
            .lock()
            .expect("lock")
            .agent_sessions
            .values()
            .filter(|s| s.tenant_id == tenant_id)
            .filter(|s| workspace_id.is_none_or(|id| s.workspace_id == id))
            .filter(|s| state.as_ref().is_none_or(|v| &s.state == v))
            .cloned()
            .collect();
        items.sort_by_key(|s| (std::cmp::Reverse(s.last_active_at), s.id));
        Ok(items)
    }

    async fn get_agent_session(&self, id: Uuid) -> Result<Option<agent_session::Model>, DbErr> {
        if let Some(err) = self.maybe_db_err() {
            return Err(err);
        }
        Ok(self
            .inner
            .lock()
            .expect("lock")
            .agent_sessions
            .get(&id)
            .cloned())
    }

    async fn list_agent_session_ids_for_tenant(&self, tenant_id: Uuid) -> Result<Vec<Uuid>, DbErr> {
        if let Some(err) = self.maybe_db_err() {
            return Err(err);
        }
        Ok(self
            .inner
            .lock()
            .expect("lock")
            .agent_sessions
            .values()
            .filter(|s| s.tenant_id == tenant_id)
            .map(|s| s.id)
            .collect())
    }

    async fn list_session_workers(
        &self,
        session_ids: Vec<Uuid>,
        agent_session_id: Option<Uuid>,
        plugin_id: Option<Uuid>,
        live: Option<bool>,
    ) -> Result<Vec<session_worker::Model>, DbErr> {
        if let Some(err) = self.maybe_db_err() {
            return Err(err);
        }
        let mut items: Vec<_> = self
            .inner
            .lock()
            .expect("lock")
            .session_workers
            .values()
            .filter(|w| {
                w.agent_session_id
                    .is_some_and(|id| session_ids.contains(&id))
            })
            .filter(|w| agent_session_id.is_none_or(|id| w.agent_session_id == Some(id)))
            .filter(|w| plugin_id.is_none_or(|id| w.plugin_id == id))
            .filter(|w| {
                live.is_none_or(|want_live| {
                    if want_live {
                        w.reaped_at.is_none()
                    } else {
                        w.reaped_at.is_some()
                    }
                })
            })
            .cloned()
            .collect();
        items.sort_by_key(|w| (std::cmp::Reverse(w.spawned_at), w.id));
        Ok(items)
    }

    async fn get_session_worker(&self, id: Uuid) -> Result<Option<session_worker::Model>, DbErr> {
        if let Some(err) = self.maybe_db_err() {
            return Err(err);
        }
        Ok(self
            .inner
            .lock()
            .expect("lock")
            .session_workers
            .get(&id)
            .cloned())
    }

    async fn create_tenant(&self, name: String) -> Result<tenant::Model, ApiError> {
        if let Some(err) = self.maybe_db_err() {
            return Err(ApiError::from(err));
        }
        let mut inner = self.inner.lock().expect("lock");
        if inner
            .tenants
            .values()
            .any(|t| t.name.eq_ignore_ascii_case(&name))
        {
            return Err(ApiError::already_exists(format!(
                "tenant with name {name:?} already exists (case-insensitive)"
            )));
        }
        let id = Uuid::new_v4();
        let now = Utc::now();
        let row = tenant::Model {
            id,
            name,
            created_at: now,
            updated_at: now,
        };
        inner.created_tenants.push(id);
        inner.tenants.insert(id, row.clone());
        Ok(row)
    }

    async fn update_tenant(
        &self,
        id: Uuid,
        name: String,
        if_unmodified_since: DateTime<Utc>,
    ) -> Result<tenant::Model, ApiError> {
        if let Some(err) = self.maybe_db_err() {
            return Err(ApiError::from(err));
        }
        let mut inner = self.inner.lock().expect("lock");
        let live = inner
            .tenants
            .get(&id)
            .cloned()
            .or_not_found("tenant", format!("no tenant with id {id}"))?;
        check_lock(&if_unmodified_since, &live.updated_at, "tenant")?;
        if inner
            .tenants
            .values()
            .any(|t| t.id != id && t.name.eq_ignore_ascii_case(&name))
        {
            return Err(ApiError::already_exists(format!(
                "tenant with name {name:?} already exists (case-insensitive)"
            )));
        }
        let mut updated = live;
        updated.name = name;
        updated.updated_at = Utc::now();
        inner.updated_tenants.push(id);
        inner.tenants.insert(id, updated.clone());
        Ok(updated)
    }

    async fn delete_tenant(
        &self,
        id: Uuid,
        if_unmodified_since: Option<DateTime<Utc>>,
    ) -> Result<tenant::Model, ApiError> {
        if let Some(err) = self.maybe_db_err() {
            return Err(ApiError::from(err));
        }
        let mut inner = self.inner.lock().expect("lock");
        let live = inner
            .tenants
            .get(&id)
            .cloned()
            .or_not_found("tenant", format!("no tenant with id {id}"))?;
        if let Some(lock) = if_unmodified_since {
            check_lock(&lock, &live.updated_at, "tenant")?;
        }
        let dependents: Vec<_> = inner
            .workspaces
            .values()
            .filter(|w| w.tenant_id == id)
            .cloned()
            .collect();
        if !dependents.is_empty() {
            let names: Vec<String> = dependents.iter().map(|w| w.name.clone()).collect();
            return Err(ApiError::has_dependents(format!(
                "tenant '{}' still has {} workspace(s): {:?}",
                live.name,
                dependents.len(),
                names
            )));
        }
        inner.deleted_tenants.push(id);
        inner.tenants.remove(&id);
        Ok(live)
    }
}
