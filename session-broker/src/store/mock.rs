use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use sea_orm::DbErr;
use uuid::Uuid;

use crate::agent_session::AgentSessionWriteError;
use crate::session_worker::{LiveWorker, SessionWorkerWriteError};
use crate::store::{AgentSessionStore, SessionWorkerStore};

#[derive(Clone)]
struct AgentSessionRow {
    id: Uuid,
    state: String,
    reactivation_count: i32,
}

#[derive(Default)]
struct AgentSessionInner {
    rows: HashMap<(String, String, String), AgentSessionRow>,
    recorded_bind: Vec<(String, String, String)>,
    recorded_grace: Vec<(String, String, String)>,
    recorded_inactive: Vec<(String, String, String)>,
    recorded_touch: Vec<(String, String, String)>,
}

#[derive(Clone, Default)]
pub struct MockAgentSessionStore {
    inner: Arc<Mutex<AgentSessionInner>>,
    always_error: Option<String>,
}

impl MockAgentSessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn always_error(msg: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(AgentSessionInner::default())),
            always_error: Some(msg.into()),
        }
    }

    pub fn with_session_pk(
        self,
        tenant_name: impl Into<String>,
        workspace_name: impl Into<String>,
        agent_session_id: impl Into<String>,
        id: Uuid,
    ) -> Self {
        let mut inner = self.inner.lock().expect("lock");
        inner.rows.insert(
            (
                tenant_name.into(),
                workspace_name.into(),
                agent_session_id.into(),
            ),
            AgentSessionRow {
                id,
                state: botwork_entity::agent_session::state::ACTIVE.to_string(),
                reactivation_count: 0,
            },
        );
        drop(inner);
        self
    }

    pub fn with_session_state(
        self,
        tenant_name: impl Into<String>,
        workspace_name: impl Into<String>,
        agent_session_id: impl Into<String>,
        id: Uuid,
        state: impl Into<String>,
    ) -> Self {
        let mut inner = self.inner.lock().expect("lock");
        inner.rows.insert(
            (
                tenant_name.into(),
                workspace_name.into(),
                agent_session_id.into(),
            ),
            AgentSessionRow {
                id,
                state: state.into(),
                reactivation_count: 0,
            },
        );
        drop(inner);
        self
    }

    pub async fn drain_recorded_bind(&self) -> Vec<(String, String, String)> {
        std::mem::take(&mut self.inner.lock().expect("lock").recorded_bind)
    }

    fn maybe_err(&self) -> Option<AgentSessionWriteError> {
        self.always_error
            .as_ref()
            .map(|m| AgentSessionWriteError::Db(DbErr::Custom(m.clone())))
    }
}

#[async_trait]
impl AgentSessionStore for MockAgentSessionStore {
    async fn record_bind_agent(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Result<(), AgentSessionWriteError> {
        if let Some(err) = self.maybe_err() {
            return Err(err);
        }
        let mut inner = self.inner.lock().expect("lock");
        let key = (
            tenant_name.to_string(),
            workspace_name.to_string(),
            agent_session_id.to_string(),
        );
        inner.recorded_bind.push(key.clone());
        if let Some(row) = inner.rows.get_mut(&key) {
            if matches!(
                row.state.as_str(),
                botwork_entity::agent_session::state::INACTIVE
                    | botwork_entity::agent_session::state::GRACE
            ) {
                row.reactivation_count = row.reactivation_count.saturating_add(1);
            }
            row.state = botwork_entity::agent_session::state::ACTIVE.to_string();
        } else {
            inner.rows.insert(
                key,
                AgentSessionRow {
                    id: Uuid::new_v4(),
                    state: botwork_entity::agent_session::state::ACTIVE.to_string(),
                    reactivation_count: 0,
                },
            );
        }
        Ok(())
    }

    async fn record_grace(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Result<(), AgentSessionWriteError> {
        if let Some(err) = self.maybe_err() {
            return Err(err);
        }
        let mut inner = self.inner.lock().expect("lock");
        let key = (
            tenant_name.to_string(),
            workspace_name.to_string(),
            agent_session_id.to_string(),
        );
        inner.recorded_grace.push(key.clone());
        let Some(row) = inner.rows.get_mut(&key) else {
            return Err(AgentSessionWriteError::MissingRow);
        };
        row.state = botwork_entity::agent_session::state::GRACE.to_string();
        Ok(())
    }

    async fn record_inactive(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Result<(), AgentSessionWriteError> {
        if let Some(err) = self.maybe_err() {
            return Err(err);
        }
        let mut inner = self.inner.lock().expect("lock");
        let key = (
            tenant_name.to_string(),
            workspace_name.to_string(),
            agent_session_id.to_string(),
        );
        inner.recorded_inactive.push(key.clone());
        let Some(row) = inner.rows.get_mut(&key) else {
            return Err(AgentSessionWriteError::MissingRow);
        };
        row.state = botwork_entity::agent_session::state::INACTIVE.to_string();
        Ok(())
    }

    async fn touch_last_active(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Result<(), AgentSessionWriteError> {
        if let Some(err) = self.maybe_err() {
            return Err(err);
        }
        let mut inner = self.inner.lock().expect("lock");
        let key = (
            tenant_name.to_string(),
            workspace_name.to_string(),
            agent_session_id.to_string(),
        );
        inner.recorded_touch.push(key.clone());
        if inner.rows.contains_key(&key) {
            Ok(())
        } else {
            Err(AgentSessionWriteError::MissingRow)
        }
    }

    async fn resolve_pk(
        &self,
        tenant_name: &str,
        workspace_name: &str,
        agent_session_id: &str,
    ) -> Result<Option<Uuid>, AgentSessionWriteError> {
        if let Some(err) = self.maybe_err() {
            return Err(err);
        }
        Ok(self
            .inner
            .lock()
            .expect("lock")
            .rows
            .get(&(
                tenant_name.to_string(),
                workspace_name.to_string(),
                agent_session_id.to_string(),
            ))
            .map(|row| row.id))
    }
}

#[derive(Clone)]
struct SessionWorkerRow {
    container_name: String,
    container_ip: String,
    mcp_session_id: String,
    plugin_id: Uuid,
    agent_session_id: Option<Uuid>,
    reaped_at: Option<chrono::DateTime<Utc>>,
}

#[derive(Default)]
struct SessionWorkerInner {
    workers: HashMap<String, SessionWorkerRow>,
    plugin_ids_by_name: HashMap<String, Uuid>,
    plugin_names_by_id: HashMap<Uuid, String>,
    recorded_spawns: Vec<(String, String, String)>,
    recorded_mcp_backfills: Vec<(String, String)>,
    recorded_agent_backfills: Vec<(String, Uuid)>,
    recorded_reaps: Vec<String>,
}

#[derive(Clone, Default)]
pub struct MockSessionWorkerStore {
    inner: Arc<Mutex<SessionWorkerInner>>,
    always_error: Option<String>,
}

impl MockSessionWorkerStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn always_error(msg: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SessionWorkerInner::default())),
            always_error: Some(msg.into()),
        }
    }

    pub fn with_plugin(self, id: Uuid, name: impl Into<String>) -> Self {
        let mut inner = self.inner.lock().expect("lock");
        let name = name.into();
        inner.plugin_ids_by_name.insert(name.clone(), id);
        inner.plugin_names_by_id.insert(id, name);
        drop(inner);
        self
    }

    pub fn with_live_worker(
        self,
        container_name: impl Into<String>,
        container_ip: impl Into<String>,
        mcp_session_id: impl Into<String>,
        plugin_id: Uuid,
    ) -> Self {
        let mut inner = self.inner.lock().expect("lock");
        let container_name = container_name.into();
        inner.workers.insert(
            container_name.clone(),
            SessionWorkerRow {
                container_name,
                container_ip: container_ip.into(),
                mcp_session_id: mcp_session_id.into(),
                plugin_id,
                agent_session_id: None,
                reaped_at: None,
            },
        );
        drop(inner);
        self
    }

    pub async fn drain_recorded_reaps(&self) -> Vec<String> {
        std::mem::take(&mut self.inner.lock().expect("lock").recorded_reaps)
    }

    fn maybe_err(&self) -> Option<SessionWorkerWriteError> {
        self.always_error
            .as_ref()
            .map(|m| SessionWorkerWriteError::Db(DbErr::Custom(m.clone())))
    }
}

#[async_trait]
impl SessionWorkerStore for MockSessionWorkerStore {
    async fn record_spawn(
        &self,
        plugin_name: &str,
        container_name: &str,
        container_ip: &str,
    ) -> Result<(), SessionWorkerWriteError> {
        if let Some(err) = self.maybe_err() {
            return Err(err);
        }
        let mut inner = self.inner.lock().expect("lock");
        inner.recorded_spawns.push((
            plugin_name.to_string(),
            container_name.to_string(),
            container_ip.to_string(),
        ));
        let plugin_id = inner
            .plugin_ids_by_name
            .get(plugin_name)
            .copied()
            .ok_or_else(|| SessionWorkerWriteError::UnknownPlugin(plugin_name.to_string()))?;
        inner.workers.insert(
            container_name.to_string(),
            SessionWorkerRow {
                container_name: container_name.to_string(),
                container_ip: container_ip.to_string(),
                mcp_session_id: String::new(),
                plugin_id,
                agent_session_id: None,
                reaped_at: None,
            },
        );
        Ok(())
    }

    async fn record_mcp_session_id(
        &self,
        container_name: &str,
        mcp_session_id: &str,
    ) -> Result<(), SessionWorkerWriteError> {
        if let Some(err) = self.maybe_err() {
            return Err(err);
        }
        let mut inner = self.inner.lock().expect("lock");
        inner
            .recorded_mcp_backfills
            .push((container_name.to_string(), mcp_session_id.to_string()));
        let Some(row) = inner.workers.get_mut(container_name) else {
            return Err(SessionWorkerWriteError::UnknownContainer(
                container_name.to_string(),
            ));
        };
        row.mcp_session_id = mcp_session_id.to_string();
        Ok(())
    }

    async fn record_agent_binding(
        &self,
        container_name: &str,
        agent_session_id: Uuid,
    ) -> Result<(), SessionWorkerWriteError> {
        if let Some(err) = self.maybe_err() {
            return Err(err);
        }
        let mut inner = self.inner.lock().expect("lock");
        inner
            .recorded_agent_backfills
            .push((container_name.to_string(), agent_session_id));
        let Some(row) = inner.workers.get_mut(container_name) else {
            return Err(SessionWorkerWriteError::UnknownContainer(
                container_name.to_string(),
            ));
        };
        row.agent_session_id = Some(agent_session_id);
        Ok(())
    }

    async fn record_reap(&self, container_name: &str) -> Result<(), SessionWorkerWriteError> {
        if let Some(err) = self.maybe_err() {
            return Err(err);
        }
        let mut inner = self.inner.lock().expect("lock");
        inner.recorded_reaps.push(container_name.to_string());
        let Some(row) = inner.workers.get_mut(container_name) else {
            return Err(SessionWorkerWriteError::UnknownContainer(
                container_name.to_string(),
            ));
        };
        if row.reaped_at.is_none() {
            row.reaped_at = Some(Utc::now());
        }
        Ok(())
    }

    async fn list_live(&self) -> Result<Vec<LiveWorker>, SessionWorkerWriteError> {
        if let Some(err) = self.maybe_err() {
            return Err(err);
        }
        let rows = self.inner.lock().expect("lock");
        Ok(rows
            .workers
            .values()
            .filter(|row| row.reaped_at.is_none())
            .map(|row| LiveWorker {
                container_name: row.container_name.clone(),
                container_ip: row.container_ip.clone(),
                mcp_session_id: row.mcp_session_id.clone(),
                plugin_id: row.plugin_id,
                agent_session_id: row.agent_session_id,
            })
            .collect())
    }

    async fn resolve_plugin_name(&self, plugin_id: Uuid) -> Result<Option<String>, DbErr> {
        if let Some(msg) = &self.always_error {
            return Err(DbErr::Custom(msg.clone()));
        }
        Ok(self
            .inner
            .lock()
            .expect("lock")
            .plugin_names_by_id
            .get(&plugin_id)
            .cloned())
    }
}
