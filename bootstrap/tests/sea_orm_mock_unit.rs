use botwork_api_core::plugin_spec::ValidatedPlugin;
use botwork_bootstrap::store::sea_orm_impl::SeaOrmBootstrapStore;
use botwork_bootstrap::store::{BootstrapStore, UpsertOutcome};
use botwork_entity::{plugin, tenant, workspace, workspace_plugin};
use chrono::Utc;
use sea_orm::{DatabaseBackend, DbErr, MockDatabase, TransactionTrait};
use uuid::Uuid;

fn validated_plugin(name: &str) -> ValidatedPlugin {
    ValidatedPlugin {
        name: name.to_string(),
        image: "ghcr.io/example/mcp-fetch:1.0".to_string(),
        port: 8000,
        path: "/mcp".to_string(),
        upstream_auth: "none".to_string(),
        env: serde_json::json!([]),
        resources: None,
        egress: serde_json::json!({ "mode": "none" }),
    }
}

fn plugin_row(id: Uuid, entry: &ValidatedPlugin) -> plugin::Model {
    plugin::Model {
        id,
        name: entry.name.clone(),
        image: entry.image.clone(),
        port: i32::from(entry.port),
        path: entry.path.clone(),
        upstream_auth: entry.upstream_auth.clone(),
        env: entry.env.clone(),
        resources: entry.resources.clone(),
        egress: entry.egress.clone(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        current_facet_id: None,
    }
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

fn binding_row(
    workspace_id: Uuid,
    plugin_id: Uuid,
    config: Option<serde_json::Value>,
) -> workspace_plugin::Model {
    workspace_plugin::Model {
        workspace_id,
        plugin_id,
        config,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

#[tokio::test]
async fn sea_orm_bootstrap_store_upsert_plugin_covers_insert_update_unchanged_and_error() {
    let entry = validated_plugin("mcp-fetch");
    let plugin_id = Uuid::new_v4();

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results([Vec::<plugin::Model>::new()])
        .append_query_results([vec![plugin_row(plugin_id, &entry)]])
        .into_connection();
    let tx = db.begin().await.expect("begin");
    let store = SeaOrmBootstrapStore::new(&tx);
    let (id, outcome) = store.upsert_plugin(&entry).await.expect("insert");
    assert_ne!(id, Uuid::nil());
    assert_eq!(outcome, UpsertOutcome::Inserted);

    let mut changed = validated_plugin("mcp-fetch");
    changed.image = "ghcr.io/example/mcp-fetch:2.0".to_string();
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results([vec![plugin_row(plugin_id, &entry)]])
        .append_query_results([vec![plugin_row(plugin_id, &changed)]])
        .into_connection();
    let tx = db.begin().await.expect("begin");
    let store = SeaOrmBootstrapStore::new(&tx);
    let (id, outcome) = store.upsert_plugin(&changed).await.expect("update");
    assert_eq!(id, plugin_id);
    assert_eq!(outcome, UpsertOutcome::Updated);

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results([vec![plugin_row(plugin_id, &entry)]])
        .into_connection();
    let tx = db.begin().await.expect("begin");
    let store = SeaOrmBootstrapStore::new(&tx);
    let (id, outcome) = store.upsert_plugin(&entry).await.expect("unchanged");
    assert_eq!(id, plugin_id);
    assert_eq!(outcome, UpsertOutcome::Unchanged);

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_errors([DbErr::Custom("boom".to_string())])
        .into_connection();
    let tx = db.begin().await.expect("begin");
    let store = SeaOrmBootstrapStore::new(&tx);
    assert!(store
        .upsert_plugin(&entry)
        .await
        .expect_err("db err")
        .to_string()
        .contains("boom"));
}

#[tokio::test]
async fn sea_orm_bootstrap_store_upsert_tenant_covers_insert_found_and_error() {
    let tenant_id = Uuid::new_v4();

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results([Vec::<tenant::Model>::new()])
        .append_query_results([vec![tenant_row(tenant_id, "phlax")]])
        .into_connection();
    let tx = db.begin().await.expect("begin");
    let store = SeaOrmBootstrapStore::new(&tx);
    let (_, outcome) = store.upsert_tenant("phlax").await.expect("insert");
    assert_eq!(outcome, UpsertOutcome::Inserted);

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results([vec![tenant_row(tenant_id, "phlax")]])
        .into_connection();
    let tx = db.begin().await.expect("begin");
    let store = SeaOrmBootstrapStore::new(&tx);
    let (id, outcome) = store.upsert_tenant("phlax").await.expect("found");
    assert_eq!(id, tenant_id);
    assert_eq!(outcome, UpsertOutcome::Updated);

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_errors([DbErr::Custom("boom".to_string())])
        .into_connection();
    let tx = db.begin().await.expect("begin");
    let store = SeaOrmBootstrapStore::new(&tx);
    assert!(store
        .upsert_tenant("phlax")
        .await
        .expect_err("db err")
        .to_string()
        .contains("boom"));
}

#[tokio::test]
async fn sea_orm_bootstrap_store_upsert_workspace_covers_insert_found_and_error() {
    let tenant_id = Uuid::new_v4();
    let workspace_id = Uuid::new_v4();

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results([Vec::<workspace::Model>::new()])
        .append_query_results([vec![workspace_row(workspace_id, tenant_id, "mcp")]])
        .into_connection();
    let tx = db.begin().await.expect("begin");
    let store = SeaOrmBootstrapStore::new(&tx);
    let (_, outcome) = store
        .upsert_workspace(tenant_id, "mcp")
        .await
        .expect("insert");
    assert_eq!(outcome, UpsertOutcome::Inserted);

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results([vec![workspace_row(workspace_id, tenant_id, "mcp")]])
        .into_connection();
    let tx = db.begin().await.expect("begin");
    let store = SeaOrmBootstrapStore::new(&tx);
    let (id, outcome) = store
        .upsert_workspace(tenant_id, "mcp")
        .await
        .expect("found");
    assert_eq!(id, workspace_id);
    assert_eq!(outcome, UpsertOutcome::Updated);

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_errors([DbErr::Custom("boom".to_string())])
        .into_connection();
    let tx = db.begin().await.expect("begin");
    let store = SeaOrmBootstrapStore::new(&tx);
    assert!(store
        .upsert_workspace(tenant_id, "mcp")
        .await
        .expect_err("db err")
        .to_string()
        .contains("boom"));
}

#[tokio::test]
async fn sea_orm_bootstrap_store_upsert_binding_covers_insert_update_unchanged_and_error() {
    let workspace_id = Uuid::new_v4();
    let plugin_id = Uuid::new_v4();
    let original = Some(serde_json::json!({ "a": 1 }));
    let changed = Some(serde_json::json!({ "a": 2 }));

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results([Vec::<workspace_plugin::Model>::new()])
        .append_query_results([vec![binding_row(workspace_id, plugin_id, original.clone())]])
        .into_connection();
    let tx = db.begin().await.expect("begin");
    let store = SeaOrmBootstrapStore::new(&tx);
    assert_eq!(
        store
            .upsert_binding(workspace_id, plugin_id, original.as_ref())
            .await
            .expect("insert"),
        UpsertOutcome::Inserted
    );

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results([vec![binding_row(workspace_id, plugin_id, original.clone())]])
        .append_query_results([vec![binding_row(workspace_id, plugin_id, changed.clone())]])
        .into_connection();
    let tx = db.begin().await.expect("begin");
    let store = SeaOrmBootstrapStore::new(&tx);
    assert_eq!(
        store
            .upsert_binding(workspace_id, plugin_id, changed.as_ref())
            .await
            .expect("update"),
        UpsertOutcome::Updated
    );

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results([vec![binding_row(workspace_id, plugin_id, original.clone())]])
        .into_connection();
    let tx = db.begin().await.expect("begin");
    let store = SeaOrmBootstrapStore::new(&tx);
    assert_eq!(
        store
            .upsert_binding(workspace_id, plugin_id, original.as_ref())
            .await
            .expect("unchanged"),
        UpsertOutcome::Unchanged
    );

    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_errors([DbErr::Custom("boom".to_string())])
        .into_connection();
    let tx = db.begin().await.expect("begin");
    let store = SeaOrmBootstrapStore::new(&tx);
    assert!(store
        .upsert_binding(workspace_id, plugin_id, original.as_ref())
        .await
        .expect_err("db err")
        .to_string()
        .contains("boom"));
}
