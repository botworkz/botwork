use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use botwork_api::store::sea_orm_impl::SeaOrmApiStore;
use botwork_api::store::ApiStore;
use botwork_api::{
    build_router, AppState, ControlPlaneClient, SecretStoreClient, SessionBrokerClient,
};
use botwork_entity::{agent_session, plugin, session_worker, tenant, workspace, workspace_plugin};
use chrono::{DateTime, Utc};
use sea_orm::{DatabaseBackend, DatabaseConnection, DbErr, MockDatabase, MockExecResult};
use tower::util::ServiceExt;
use uuid::Uuid;

fn app_state_with_db(db: DatabaseConnection) -> AppState {
    let db = Arc::new(db);
    AppState {
        store: Arc::new(SeaOrmApiStore::new_shared(db.clone())),
        db,
        control_plane: ControlPlaneClient::disabled(),
        secret_store: SecretStoreClient::disabled(),
        session_broker: SessionBrokerClient::disabled(),
    }
}

fn make_store(mock: MockDatabase) -> SeaOrmApiStore {
    SeaOrmApiStore::new(mock.into_connection())
}

fn tenant_row(id: Uuid, name: &str, updated_at: DateTime<Utc>) -> tenant::Model {
    tenant::Model {
        id,
        name: name.to_string(),
        created_at: updated_at,
        updated_at,
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

fn workspace_plugin_row(workspace_id: Uuid, plugin_id: Uuid) -> workspace_plugin::Model {
    workspace_plugin::Model {
        workspace_id,
        plugin_id,
        config: Some(serde_json::json!({ "k": "v" })),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn agent_session_row(id: Uuid, tenant_id: Uuid, workspace_id: Uuid) -> agent_session::Model {
    agent_session::Model {
        id,
        tenant_id,
        workspace_id,
        agent_session_id: "session-1".to_string(),
        state: agent_session::state::ACTIVE.to_string(),
        created_at: Utc::now(),
        last_active_at: Utc::now(),
        reactivation_count: 0,
    }
}

fn session_worker_row(
    id: Uuid,
    agent_session_id: Option<Uuid>,
    plugin_id: Uuid,
) -> session_worker::Model {
    session_worker::Model {
        id,
        agent_session_id,
        plugin_id,
        container_name: "mcp_session_1".to_string(),
        container_ip: "10.0.0.1".to_string(),
        mcp_session_id: "sid-1".to_string(),
        spawned_at: Utc::now(),
        reaped_at: None,
    }
}

fn count_row(cnt: i64) -> BTreeMap<String, sea_orm::Value> {
    BTreeMap::from([("cnt".to_string(), cnt.into())])
}

fn triple_row(tenant: &str, workspace: &str, plugin: &str) -> BTreeMap<String, sea_orm::Value> {
    BTreeMap::from([
        ("tenant".to_string(), tenant.to_string().into()),
        ("workspace".to_string(), workspace.to_string().into()),
        ("plugin".to_string(), plugin.to_string().into()),
    ])
}

fn admin_post(path: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("x-botwork-admin", "ops")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

fn tenant_post(path: &str, tenant: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("x-botwork-tenant", tenant)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

async fn json_body(response: axum::response::Response) -> serde_json::Value {
    serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body"),
    )
    .expect("json")
}

#[tokio::test]
async fn sea_orm_api_store_tenant_read_methods_cover_some_none_and_error() {
    let tenant_id = Uuid::new_v4();
    let when = Utc::now();

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![tenant_row(tenant_id, "phlax", when)]]),
    );
    assert_eq!(
        store.resolve_tenant_id("phlax").await.expect("query"),
        Some(tenant_id)
    );

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([Vec::<tenant::Model>::new()]),
    );
    assert_eq!(
        store.resolve_tenant_id("missing").await.expect("query"),
        None
    );

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_errors([DbErr::Custom("boom".to_string())]),
    );
    assert!(store
        .resolve_tenant_id("err")
        .await
        .expect_err("db err")
        .to_string()
        .contains("boom"));

    let rows = vec![
        tenant_row(Uuid::new_v4(), "a", when),
        tenant_row(Uuid::new_v4(), "b", when),
    ];
    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres).append_query_results([rows.clone()]),
    );
    assert_eq!(store.list_tenants().await.expect("list"), rows);

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_errors([DbErr::Custom("list boom".to_string())]),
    );
    assert!(store
        .list_tenants()
        .await
        .expect_err("db err")
        .to_string()
        .contains("list boom"));

    let row = tenant_row(tenant_id, "phlax", when);
    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres).append_query_results([vec![row.clone()]]),
    );
    assert_eq!(store.get_tenant(tenant_id).await.expect("get"), Some(row));

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([Vec::<tenant::Model>::new()]),
    );
    assert_eq!(store.get_tenant(tenant_id).await.expect("get"), None);

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_errors([DbErr::Custom("get boom".to_string())]),
    );
    assert!(store
        .get_tenant(tenant_id)
        .await
        .expect_err("db err")
        .to_string()
        .contains("get boom"));
}

#[tokio::test]
async fn sea_orm_api_store_workspace_plugin_and_session_read_methods_cover_results_and_errors() {
    let tenant_id = Uuid::new_v4();
    let workspace_id = Uuid::new_v4();
    let plugin_id = Uuid::new_v4();
    let agent_session_id = Uuid::new_v4();
    let worker_id = Uuid::new_v4();

    let workspaces = vec![workspace_row(workspace_id, tenant_id, "mcp")];
    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres).append_query_results([workspaces.clone()]),
    );
    assert_eq!(
        store
            .list_workspaces(tenant_id, Some(workspace_id))
            .await
            .expect("list"),
        workspaces
    );

    let workspace = workspace_row(workspace_id, tenant_id, "mcp");
    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![workspace.clone()]]),
    );
    assert_eq!(
        store.get_workspace(workspace_id).await.expect("get"),
        Some(workspace)
    );

    let plugins = vec![plugin_row(plugin_id, "mcp-fetch")];
    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres).append_query_results([plugins.clone()]),
    );
    assert_eq!(store.list_plugins().await.expect("list"), plugins);

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![plugin_row(plugin_id, "mcp-fetch")]]),
    );
    assert_eq!(
        store
            .get_plugin(plugin_id)
            .await
            .expect("get")
            .expect("row")
            .id,
        plugin_id
    );

    let store =
        make_store(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([vec![workspace_row(workspace_id, tenant_id, "mcp")]]),
        );
    assert_eq!(
        store
            .list_workspace_ids_for_tenant(tenant_id)
            .await
            .expect("ids"),
        vec![workspace_id]
    );

    let bindings = vec![workspace_plugin_row(workspace_id, plugin_id)];
    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres).append_query_results([bindings.clone()]),
    );
    assert_eq!(
        store
            .list_workspace_plugins(vec![workspace_id], Some(workspace_id), Some(plugin_id))
            .await
            .expect("bindings"),
        bindings
    );

    let binding = workspace_plugin_row(workspace_id, plugin_id);
    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres).append_query_results([vec![binding.clone()]]),
    );
    assert_eq!(
        store
            .get_workspace_plugin(workspace_id, plugin_id)
            .await
            .expect("binding"),
        Some(binding)
    );

    let sessions = vec![agent_session_row(agent_session_id, tenant_id, workspace_id)];
    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres).append_query_results([sessions.clone()]),
    );
    assert_eq!(
        store
            .list_agent_sessions(
                tenant_id,
                Some(workspace_id),
                Some(agent_session::state::ACTIVE.to_string())
            )
            .await
            .expect("sessions"),
        sessions
    );

    let session = agent_session_row(agent_session_id, tenant_id, workspace_id);
    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres).append_query_results([vec![session.clone()]]),
    );
    assert_eq!(
        store
            .get_agent_session(agent_session_id)
            .await
            .expect("get"),
        Some(session)
    );

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres).append_query_results([vec![
            agent_session_row(agent_session_id, tenant_id, workspace_id),
        ]]),
    );
    assert_eq!(
        store
            .list_agent_session_ids_for_tenant(tenant_id)
            .await
            .expect("ids"),
        vec![agent_session_id]
    );

    let workers = vec![session_worker_row(
        worker_id,
        Some(agent_session_id),
        plugin_id,
    )];
    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres).append_query_results([workers.clone()]),
    );
    assert_eq!(
        store
            .list_session_workers(
                vec![agent_session_id],
                Some(agent_session_id),
                Some(plugin_id),
                Some(true)
            )
            .await
            .expect("workers"),
        workers
    );

    let worker = session_worker_row(worker_id, Some(agent_session_id), plugin_id);
    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres).append_query_results([vec![worker.clone()]]),
    );
    assert_eq!(
        store.get_session_worker(worker_id).await.expect("get"),
        Some(worker)
    );

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_errors([DbErr::Custom("worker boom".to_string())]),
    );
    assert!(store
        .get_session_worker(worker_id)
        .await
        .expect_err("db err")
        .to_string()
        .contains("worker boom"));
}

#[tokio::test]
async fn sea_orm_api_store_tenant_write_methods_cover_free_taken_not_found_stale_dependents_and_errors(
) {
    let tenant_id = Uuid::new_v4();
    let lock = Utc::now();

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![count_row(0)]])
            .append_query_results([vec![tenant_row(Uuid::new_v4(), "phlax", Utc::now())]]),
    );
    assert_eq!(
        store
            .create_tenant("phlax".to_string())
            .await
            .expect("create")
            .name,
        "phlax"
    );

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres).append_query_results([vec![count_row(1)]]),
    );
    assert_eq!(
        store
            .create_tenant("phlax".to_string())
            .await
            .expect_err("taken")
            .into_response()
            .status(),
        StatusCode::CONFLICT
    );

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_errors([DbErr::Custom("count boom".to_string())]),
    );
    assert_eq!(
        store
            .create_tenant("phlax".to_string())
            .await
            .expect_err("db err")
            .into_response()
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );

    let live = tenant_row(tenant_id, "phlax", lock);
    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![live.clone()]])
            .append_query_results([vec![count_row(0)]])
            .append_query_results([vec![tenant_row(tenant_id, "renamed", Utc::now())]]),
    );
    assert_eq!(
        store
            .update_tenant(tenant_id, "renamed".to_string(), lock)
            .await
            .expect("update")
            .name,
        "renamed"
    );

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([Vec::<tenant::Model>::new()]),
    );
    assert_eq!(
        store
            .update_tenant(tenant_id, "renamed".to_string(), lock)
            .await
            .expect_err("missing")
            .into_response()
            .status(),
        StatusCode::NOT_FOUND
    );

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres).append_query_results([vec![live.clone()]]),
    );
    assert_eq!(
        store
            .update_tenant(
                tenant_id,
                "renamed".to_string(),
                lock - chrono::TimeDelta::seconds(1)
            )
            .await
            .expect_err("stale")
            .into_response()
            .status(),
        StatusCode::CONFLICT
    );

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![live.clone()]])
            .append_query_results([vec![count_row(1)]]),
    );
    assert_eq!(
        store
            .update_tenant(tenant_id, "renamed".to_string(), lock)
            .await
            .expect_err("taken")
            .into_response()
            .status(),
        StatusCode::CONFLICT
    );

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_errors([DbErr::Custom("update boom".to_string())]),
    );
    assert_eq!(
        store
            .update_tenant(tenant_id, "renamed".to_string(), lock)
            .await
            .expect_err("db err")
            .into_response()
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![live.clone()]])
            .append_query_results([Vec::<workspace::Model>::new()])
            .append_exec_results([MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }]),
    );
    assert_eq!(
        store
            .delete_tenant(tenant_id, Some(lock))
            .await
            .expect("delete")
            .id,
        tenant_id
    );

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![live.clone()]])
            .append_query_results([vec![workspace_row(Uuid::new_v4(), tenant_id, "mcp")]]),
    );
    assert_eq!(
        store
            .delete_tenant(tenant_id, Some(lock))
            .await
            .expect_err("dependents")
            .into_response()
            .status(),
        StatusCode::CONFLICT
    );

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([Vec::<tenant::Model>::new()]),
    );
    assert_eq!(
        store
            .delete_tenant(tenant_id, Some(lock))
            .await
            .expect_err("missing")
            .into_response()
            .status(),
        StatusCode::NOT_FOUND
    );

    let store =
        make_store(MockDatabase::new(DatabaseBackend::Postgres).append_query_results([vec![live]]));
    assert_eq!(
        store
            .delete_tenant(tenant_id, Some(lock - chrono::TimeDelta::seconds(1)))
            .await
            .expect_err("stale")
            .into_response()
            .status(),
        StatusCode::CONFLICT
    );

    let store = make_store(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_errors([DbErr::Custom("delete boom".to_string())]),
    );
    assert_eq!(
        store
            .delete_tenant(tenant_id, Some(lock))
            .await
            .expect_err("db err")
            .into_response()
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn workspace_and_plugin_name_count_queries_cover_free_taken_and_error_via_handlers() {
    let tenant_id = Uuid::new_v4();
    let workspace_id = Uuid::new_v4();
    let plugin_id = Uuid::new_v4();

    let app = build_router(app_state_with_db(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![tenant_row(tenant_id, "phlax", Utc::now())]])
            .append_query_results([vec![count_row(0)]])
            .append_query_results([vec![workspace_row(workspace_id, tenant_id, "mcp")]])
            .into_connection(),
    ));
    let response = app
        .oneshot(tenant_post(
            "/api/tenant/phlax/workspaces",
            "phlax",
            serde_json::json!({ "name": "mcp" }),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::CREATED);

    let app = build_router(app_state_with_db(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![tenant_row(tenant_id, "phlax", Utc::now())]])
            .append_query_results([vec![count_row(1)]])
            .into_connection(),
    ));
    let response = app
        .oneshot(tenant_post(
            "/api/tenant/phlax/workspaces",
            "phlax",
            serde_json::json!({ "name": "mcp" }),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::CONFLICT);

    let app = build_router(app_state_with_db(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![tenant_row(tenant_id, "phlax", Utc::now())]])
            .append_query_errors([DbErr::Custom("workspace count boom".to_string())])
            .into_connection(),
    ));
    let response = app
        .oneshot(tenant_post(
            "/api/tenant/phlax/workspaces",
            "phlax",
            serde_json::json!({ "name": "mcp" }),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let app = build_router(app_state_with_db(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![count_row(0)]])
            .append_query_results([vec![plugin_row(plugin_id, "mcp-fetch")]])
            .into_connection(),
    ));
    let response = app
        .oneshot(admin_post(
            "/api/plugins",
            serde_json::json!({
                "name": "mcp-fetch",
                "image": "ghcr.io/example/mcp-fetch:1.0",
                "egress": "none"
            }),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::CREATED);

    let app = build_router(app_state_with_db(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![count_row(1)]])
            .into_connection(),
    ));
    let response = app
        .oneshot(admin_post(
            "/api/plugins",
            serde_json::json!({
                "name": "mcp-fetch",
                "image": "ghcr.io/example/mcp-fetch:1.0",
                "egress": "none"
            }),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::CONFLICT);

    let app = build_router(app_state_with_db(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_errors([DbErr::Custom("plugin count boom".to_string())])
            .into_connection(),
    ));
    let response = app
        .oneshot(admin_post(
            "/api/plugins",
            serde_json::json!({
                "name": "mcp-fetch",
                "image": "ghcr.io/example/mcp-fetch:1.0",
                "egress": "none"
            }),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let app = build_router(app_state_with_db(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![triple_row("phlax", "mcp", "mcp-fetch")]])
            .append_query_results([Vec::<workspace_plugin::Model>::new()])
            .append_query_results([vec![workspace_plugin_row(workspace_id, plugin_id)]])
            .into_connection(),
    ));
    let response = app
        .oneshot(tenant_post(
            "/api/tenant/phlax/workspace_plugins",
            "phlax",
            serde_json::json!({
                "workspace_id": workspace_id,
                "plugin_id": plugin_id,
                "config": { "k": "v" }
            }),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        json_body(response).await["workspace_id"],
        workspace_id.to_string()
    );
}

#[tokio::test]
async fn resolve_triple_query_body_covers_missing_and_error_paths_via_binding_create() {
    let _tenant_id = Uuid::new_v4();
    let workspace_id = Uuid::new_v4();
    let plugin_id = Uuid::new_v4();

    let app = build_router(app_state_with_db(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([Vec::<BTreeMap<String, sea_orm::Value>>::new()])
            .into_connection(),
    ));
    let response = app
        .oneshot(tenant_post(
            "/api/tenant/phlax/workspace_plugins",
            "phlax",
            serde_json::json!({
                "workspace_id": workspace_id,
                "plugin_id": plugin_id,
                "config": { "k": "v" }
            }),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let app = build_router(app_state_with_db(
        MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_errors([DbErr::Custom("triple boom".to_string())])
            .into_connection(),
    ));
    let response = app
        .oneshot(tenant_post(
            "/api/tenant/phlax/workspace_plugins",
            "phlax",
            serde_json::json!({
                "workspace_id": workspace_id,
                "plugin_id": plugin_id,
                "config": { "k": "v" }
            }),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
