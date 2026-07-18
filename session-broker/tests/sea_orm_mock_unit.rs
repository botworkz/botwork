use botwork_entity::{agent_session, plugin, session_worker, tenant, workspace};
use botwork_session_broker::agent_session::AgentSessionWriteError;
use botwork_session_broker::session_worker::{LiveWorker, SessionWorkerWriteError};
use botwork_session_broker::store::sea_orm_impl::{
    SeaOrmAgentSessionStore, SeaOrmSessionWorkerStore,
};
use botwork_session_broker::store::{AgentSessionStore, SessionWorkerStore};
use chrono::Utc;
use sea_orm::{DatabaseBackend, DbErr, MockDatabase};
use uuid::Uuid;

fn agent_store(mock: MockDatabase) -> SeaOrmAgentSessionStore {
    SeaOrmAgentSessionStore::new(mock.into_connection())
}

fn worker_store(mock: MockDatabase) -> SeaOrmSessionWorkerStore {
    SeaOrmSessionWorkerStore::new(mock.into_connection())
}

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

fn plugin_row(id: Uuid, name: &str) -> plugin::Model {
    plugin::Model {
        id,
        name: name.to_string(),
        image: "ghcr.io/example/mcp-fetch:1.0".to_string(),
        port: 8000,
        path: "/mcp".to_string(),
        upstream_auth: "none".to_string(),
        env: serde_json::json!([]),
        resources: None,
        egress: serde_json::json!({ "mode": "none" }),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        current_facet_id: None,
    }
}

fn worker_row(
    id: Uuid,
    plugin_id: Uuid,
    container_name: &str,
    mcp_session_id: &str,
    agent_session_id: Option<Uuid>,
    reaped_at: Option<chrono::DateTime<Utc>>,
) -> session_worker::Model {
    session_worker::Model {
        id,
        agent_session_id,
        plugin_id,
        container_name: container_name.to_string(),
        container_ip: "10.0.0.2".to_string(),
        mcp_session_id: mcp_session_id.to_string(),
        spawned_at: Utc::now(),
        reaped_at,
    }
}

#[tokio::test]
async fn sea_orm_agent_session_store_methods_cover_success_missing_and_error() {
    let tenant_id = Uuid::new_v4();
    let workspace_id = Uuid::new_v4();
    let existing_id = Uuid::new_v4();

    let store = agent_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![tenant_row(tenant_id, "phlax")]])
            .append_query_results([vec![workspace_row(workspace_id, tenant_id, "mcp")]])
            .append_query_results([Vec::<agent_session::Model>::new()])
            .append_query_results([vec![agent_session_row(
                existing_id,
                tenant_id,
                workspace_id,
                "agent-1",
                agent_session::state::ACTIVE,
            )]]),
    );
    store
        .record_bind_agent("phlax", "mcp", "agent-1")
        .await
        .expect("insert");

    let store = agent_store(
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
            .append_query_results([vec![agent_session_row(
                existing_id,
                tenant_id,
                workspace_id,
                "agent-1",
                agent_session::state::GRACE,
            )]]),
    );
    store
        .record_grace("phlax", "mcp", "agent-1")
        .await
        .expect("grace");

    let store = agent_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![tenant_row(tenant_id, "phlax")]])
            .append_query_results([vec![workspace_row(workspace_id, tenant_id, "mcp")]])
            .append_query_results([vec![agent_session_row(
                existing_id,
                tenant_id,
                workspace_id,
                "agent-1",
                agent_session::state::GRACE,
            )]])
            .append_query_results([vec![agent_session_row(
                existing_id,
                tenant_id,
                workspace_id,
                "agent-1",
                agent_session::state::INACTIVE,
            )]]),
    );
    store
        .record_inactive("phlax", "mcp", "agent-1")
        .await
        .expect("inactive");

    let store = agent_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![tenant_row(tenant_id, "phlax")]])
            .append_query_results([vec![workspace_row(workspace_id, tenant_id, "mcp")]])
            .append_query_results([Vec::<agent_session::Model>::new()]),
    );
    assert!(matches!(
        store.touch_last_active("phlax", "mcp", "agent-1").await,
        Err(AgentSessionWriteError::MissingRow)
    ));

    let store = agent_store(
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
    assert_eq!(
        store
            .resolve_pk("phlax", "mcp", "agent-1")
            .await
            .expect("resolve"),
        Some(existing_id)
    );
    assert_eq!(
        store
            .resolve_pk("phlax", "mcp", "agent-2")
            .await
            .expect("resolve"),
        None
    );

    let store = agent_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_errors([DbErr::Custom("boom".to_string())]),
    );
    assert!(store
        .record_bind_agent("phlax", "mcp", "agent-1")
        .await
        .expect_err("db err")
        .to_string()
        .contains("boom"));
}

#[tokio::test]
async fn sea_orm_session_worker_store_methods_cover_success_none_and_error() {
    let plugin_id = Uuid::new_v4();
    let worker_id = Uuid::new_v4();
    let agent_session_id = Uuid::new_v4();

    let store = worker_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![plugin_row(plugin_id, "mcp-fetch")]])
            .append_query_results([vec![worker_row(
                worker_id,
                plugin_id,
                "mcp_session_1",
                "",
                None,
                None,
            )]]),
    );
    store
        .record_spawn("mcp-fetch", "mcp_session_1", "10.0.0.1")
        .await
        .expect("spawn");

    let store = worker_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![worker_row(
                worker_id,
                plugin_id,
                "mcp_session_1",
                "",
                None,
                None,
            )]])
            .append_query_results([vec![worker_row(
                worker_id,
                plugin_id,
                "mcp_session_1",
                "sid-1",
                None,
                None,
            )]]),
    );
    store
        .record_mcp_session_id("mcp_session_1", "sid-1")
        .await
        .expect("mcp sid");

    let store = worker_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![worker_row(
                worker_id,
                plugin_id,
                "mcp_session_1",
                "sid-1",
                None,
                None,
            )]])
            .append_query_results([vec![worker_row(
                worker_id,
                plugin_id,
                "mcp_session_1",
                "sid-1",
                Some(agent_session_id),
                None,
            )]]),
    );
    store
        .record_agent_binding("mcp_session_1", agent_session_id)
        .await
        .expect("binding");

    let store = worker_store(
        MockDatabase::new(DatabaseBackend::Postgres).append_query_results([vec![worker_row(
            worker_id,
            plugin_id,
            "mcp_session_1",
            "sid-1",
            Some(agent_session_id),
            Some(Utc::now()),
        )]]),
    );
    store
        .record_reap("mcp_session_1")
        .await
        .expect("already reaped noop");

    let live_rows = vec![
        worker_row(
            worker_id,
            plugin_id,
            "mcp_session_1",
            "sid-1",
            Some(agent_session_id),
            None,
        ),
        worker_row(Uuid::new_v4(), plugin_id, "mcp_session_2", "", None, None),
    ];
    let store = worker_store(
        MockDatabase::new(DatabaseBackend::Postgres).append_query_results([live_rows.clone()]),
    );
    assert_eq!(
        store.list_live().await.expect("live"),
        vec![
            LiveWorker {
                container_name: live_rows[0].container_name.clone(),
                container_ip: live_rows[0].container_ip.clone(),
                mcp_session_id: live_rows[0].mcp_session_id.clone(),
                plugin_id: live_rows[0].plugin_id,
                agent_session_id: live_rows[0].agent_session_id,
            },
            LiveWorker {
                container_name: live_rows[1].container_name.clone(),
                container_ip: live_rows[1].container_ip.clone(),
                mcp_session_id: live_rows[1].mcp_session_id.clone(),
                plugin_id: live_rows[1].plugin_id,
                agent_session_id: live_rows[1].agent_session_id,
            },
        ]
    );

    let store = worker_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![plugin_row(plugin_id, "mcp-fetch")]]),
    );
    assert_eq!(
        store.resolve_plugin_name(plugin_id).await.expect("name"),
        Some("mcp-fetch".to_string())
    );

    let store = worker_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([Vec::<plugin::Model>::new()]),
    );
    assert_eq!(
        store.resolve_plugin_name(plugin_id).await.expect("name"),
        None
    );

    let store = worker_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([Vec::<session_worker::Model>::new()]),
    );
    assert!(matches!(
        store.record_reap("missing").await,
        Err(SessionWorkerWriteError::UnknownContainer(name)) if name == "missing"
    ));

    let store = worker_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_errors([DbErr::Custom("boom".to_string())]),
    );
    assert!(store
        .record_spawn("mcp-fetch", "mcp_session_1", "10.0.0.1")
        .await
        .expect_err("db err")
        .to_string()
        .contains("boom"));

    let store = worker_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_errors([DbErr::Custom("resolve boom".to_string())]),
    );
    assert!(store
        .resolve_plugin_name(plugin_id)
        .await
        .expect_err("db err")
        .to_string()
        .contains("resolve boom"));
}
