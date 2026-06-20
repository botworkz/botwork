//! End-to-end bootstrap apply against a real postgres.
//!
//! Spins a throwaway postgres + runs `botwork-migration` to land the
//! schema, then exercises the bootstrap binary's library entry point
//! ([`botwork_bootstrap::apply`]) and asserts:
//!
//! 1. A fresh apply against an empty DB inserts the expected counts.
//! 2. A second apply with the same config is a complete no-op
//!    (idempotency — boot can restart safely).
//! 3. A mutated config produces the expected per-table mutation counts.
//! 4. The resolve-shape query (RFE #101 hot path) returns the row we
//!    just bootstrapped — proving the schema + apply are wired together.
//!
//! Gated on docker the same way `migrate_smoke.rs` is.

use std::time::Duration;

use botwork_bootstrap::{
    apply, BootstrapConfig, PluginEntry, TenantEntry, WorkspaceEntry, WorkspacePluginEntry,
};
use botwork_entity::connection::connect;
use botwork_migration::Migrator;
use sea_orm::{ConnectionTrait, DatabaseConnection, FromQueryResult, Statement};
use sea_orm_migration::MigratorTrait;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const POSTGRES_TAG: &str = "16-alpine";

async fn start_postgres() -> Result<(testcontainers::ContainerAsync<Postgres>, String), String> {
    use testcontainers::ImageExt;

    let image = Postgres::default()
        .with_db_name("botwork")
        .with_user("botwork")
        .with_password("test")
        .with_tag(POSTGRES_TAG);

    let container = image
        .start()
        .await
        .map_err(|err| format!("start container: {err}"))?;
    let host = container
        .get_host()
        .await
        .map_err(|err| format!("get_host: {err}"))?;
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .map_err(|err| format!("get_host_port_ipv4: {err}"))?;
    let url = format!("postgres://botwork:test@{host}:{port}/botwork");
    Ok((container, url))
}

async fn connect_with_retry(url: &str) -> Result<DatabaseConnection, sea_orm::DbErr> {
    let mut last = None;
    for attempt in 0..10u32 {
        match connect(url).await {
            Ok(db) => return Ok(db),
            Err(err) => {
                last = Some(err);
                tokio::time::sleep(Duration::from_millis(200 * (1 + u64::from(attempt)))).await;
            }
        }
    }
    Err(last.expect("at least one error after retry loop"))
}

async fn docker_available() -> bool {
    use testcontainers::core::WaitFor;
    use testcontainers::GenericImage;
    let probe =
        GenericImage::new("testcontainers/helloworld", "1.3.0").with_wait_for(WaitFor::seconds(1));
    match tokio::time::timeout(Duration::from_secs(5), probe.start()).await {
        Ok(Ok(container)) => {
            let _ = container.rm().await;
            true
        }
        _ => false,
    }
}

/// Build a `BootstrapConfig` shaped like a minimal real `bootstrap.yaml`:
/// tenant `phlax`, workspace `mcp`, two plugins (one with a per-binding
/// config blob, one without).
fn sample_config() -> BootstrapConfig {
    BootstrapConfig {
        tenants: vec![TenantEntry {
            name: "phlax".to_owned(),
            workspaces: vec![WorkspaceEntry {
                name: "mcp".to_owned(),
                plugins: vec![
                    WorkspacePluginEntry {
                        name: "mcp-bash".to_owned(),
                        config: None,
                    },
                    WorkspacePluginEntry {
                        name: "mcp-fetch".to_owned(),
                        config: Some(serde_json::json!({"url": "https://example.com"})),
                    },
                ],
            }],
        }],
        plugins: vec![
            PluginEntry {
                name: "mcp-bash".to_owned(),
                image: "ghcr.io/example/mcp-bash:1.0".to_owned(),
                egress: serde_json::json!({"mode": "none"}),
            },
            PluginEntry {
                name: "mcp-fetch".to_owned(),
                image: "ghcr.io/example/mcp-fetch:1.0".to_owned(),
                egress: serde_json::json!({"allow": [{"host": "example.com", "ports": [443]}]}),
            },
        ],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_inserts_idempotently_then_resolves() {
    if !docker_available().await {
        eprintln!(
            "IGNORED bootstrap_inserts_idempotently_then_resolves: \
             docker not reachable; full proof runs in containers.yml smoke"
        );
        return;
    }

    let (_pg, url) = start_postgres()
        .await
        .expect("postgres container must start");
    let db = connect_with_retry(&url)
        .await
        .expect("connect to ephemeral postgres");
    Migrator::up(&db, None)
        .await
        .expect("migrations must apply before bootstrap runs");

    // First apply: inserts everything.
    let cfg = sample_config();
    let stats = apply(&db, &cfg).await.expect("first apply");
    assert_eq!(stats.tenants_inserted, 1);
    assert_eq!(stats.workspaces_inserted, 1);
    assert_eq!(stats.plugins_inserted, 2);
    assert_eq!(stats.bindings_inserted, 2);
    assert_eq!(stats.tenants_updated, 0);
    assert_eq!(stats.workspaces_updated, 0);
    assert_eq!(stats.plugins_updated, 0);
    assert_eq!(stats.bindings_updated, 0);

    // Second apply with the same config: every row already exists with
    // matching content, so no UPDATE should fire (counted columns stay 0).
    // tenant/workspace report `updated=1` because they have no mutable
    // shape and we count the "row matches" branch as updated.
    let stats = apply(&db, &cfg).await.expect("second apply");
    assert_eq!(stats.tenants_inserted, 0);
    assert_eq!(stats.workspaces_inserted, 0);
    assert_eq!(stats.plugins_inserted, 0);
    assert_eq!(stats.bindings_inserted, 0);
    assert_eq!(
        stats.plugins_updated, 0,
        "unchanged plugins must not UPDATE"
    );
    assert_eq!(
        stats.bindings_updated, 0,
        "unchanged bindings must not UPDATE"
    );

    // Resolve hot-path: prove the join chain returns what config-broker
    // will read post-cutover. We assert on the binding for (phlax, mcp,
    // mcp-fetch) because it carries a config blob — the resolve path
    // we'll be exercising once config-broker switches.
    let resolved = resolve(&db, "phlax", "mcp", "mcp-fetch").await;
    assert_eq!(resolved.image, "ghcr.io/example/mcp-fetch:1.0");
    assert_eq!(
        resolved.config.as_ref().and_then(|c| c.get("url")),
        Some(&serde_json::Value::String("https://example.com".to_owned())),
    );

    // Mutate the config: change one plugin's image + drop the per-binding
    // config on mcp-fetch. Assert the updates land.
    let mut mutated = sample_config();
    mutated.plugins[0].image = "ghcr.io/example/mcp-bash:2.0".to_owned();
    mutated.tenants[0].workspaces[0].plugins[1].config = None;
    let stats = apply(&db, &mutated).await.expect("third apply (mutated)");
    assert_eq!(stats.plugins_updated, 1, "image change must UPDATE plugin");
    assert_eq!(
        stats.bindings_updated, 1,
        "config-blob change must UPDATE binding"
    );
    assert_eq!(stats.plugins_inserted, 0);
    assert_eq!(stats.bindings_inserted, 0);

    let resolved = resolve(&db, "phlax", "mcp", "mcp-bash").await;
    assert_eq!(resolved.image, "ghcr.io/example/mcp-bash:2.0");
    let resolved = resolve(&db, "phlax", "mcp", "mcp-fetch").await;
    assert!(
        resolved.config.is_none(),
        "binding config should be cleared after mutation"
    );
}

#[derive(Debug, FromQueryResult)]
struct ResolveRow {
    image: String,
    config: Option<serde_json::Value>,
}

/// The hot-path resolve query, verbatim from the RFE. config-broker will
/// be calling this exact shape post-cutover (modulo prepared-statement
/// parameter mechanics).
async fn resolve(
    db: &DatabaseConnection,
    tenant: &str,
    workspace: &str,
    plugin: &str,
) -> ResolveRow {
    let backend = db.get_database_backend();
    let stmt = Statement::from_sql_and_values(
        backend,
        "SELECT p.image, wp.config \
         FROM plugin p \
         JOIN workspace_plugin wp ON wp.plugin_id = p.id \
         JOIN workspace w ON w.id = wp.workspace_id \
         JOIN tenant t ON t.id = w.tenant_id \
         WHERE t.name = $1 AND w.name = $2 AND p.name = $3",
        [tenant.into(), workspace.into(), plugin.into()],
    );
    ResolveRow::find_by_statement(stmt)
        .one(db)
        .await
        .expect("resolve query must succeed")
        .expect("resolve query must return a row for the seeded binding")
}
