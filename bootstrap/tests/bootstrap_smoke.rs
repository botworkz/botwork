//! End-to-end bootstrap apply against a real postgres.
//!
//! Spins a throwaway postgres + runs `botwork-migration` to land the
//! schema, then exercises `botwork_bootstrap::apply` against a
//! validated config and asserts:
//!
//! 1. Fresh apply against an empty DB inserts the expected counts.
//! 2. Second apply with the same config is a no-op (idempotency).
//! 3. Mutated config produces the expected per-table update counts.
//! 4. The resolve-shape JOIN returns the row we just bootstrapped.
//!
//! Gated on docker the same way `migrate_smoke.rs` is.

use std::time::Duration;

use botwork_bootstrap::plugin_spec::RawPluginEntry;
use botwork_bootstrap::{apply, BootstrapConfig, BootstrapConfigRaw};
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
/// config blob, one without). Routed through the raw->validated pipeline
/// so the smoke exercises the validator too.
fn sample_config() -> BootstrapConfig {
    let yaml = r#"
tenants:
- name: phlax
  workspaces:
  - name: mcp
    plugins:
    - name: mcp-bash
    - name: mcp-fetch
      config:
        url: https://example.com

plugins:
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:1.0
  egress: none
- name: mcp-fetch
  image: ghcr.io/example/mcp-fetch:1.0
  egress:
    allow:
    - host: example.com
      ports: [443]
"#;
    let raw: BootstrapConfigRaw = serde_yaml::from_str(yaml).expect("parse bootstrap yaml");
    BootstrapConfig::from_raw(raw).expect("validate")
}

/// Same as `sample_config` but with `image` mutated on `mcp-bash` and
/// the per-binding `config` removed on `mcp-fetch`.
fn mutated_config() -> BootstrapConfig {
    let yaml = r#"
tenants:
- name: phlax
  workspaces:
  - name: mcp
    plugins:
    - name: mcp-bash
    - name: mcp-fetch

plugins:
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:2.0
  egress: none
- name: mcp-fetch
  image: ghcr.io/example/mcp-fetch:1.0
  egress:
    allow:
    - host: example.com
      ports: [443]
"#;
    let raw: BootstrapConfigRaw = serde_yaml::from_str(yaml).expect("parse bootstrap yaml");
    BootstrapConfig::from_raw(raw).expect("validate")
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
    assert_eq!(stats.plugins_updated, 0);
    assert_eq!(stats.bindings_updated, 0);

    // Second apply with the same config: zero mutations.
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

    // Resolve hot-path: prove the join chain returns what
    // config-broker will read post-cutover.
    let resolved = resolve(&db, "phlax", "mcp", "mcp-fetch").await;
    assert_eq!(resolved.image, "ghcr.io/example/mcp-fetch:1.0");
    assert_eq!(
        resolved.config.as_ref().and_then(|c| c.get("url")),
        Some(&serde_json::Value::String("https://example.com".to_owned())),
    );

    // Mutate: change one plugin's image + drop the per-binding config
    // on mcp-fetch.
    let stats = apply(&db, &mutated_config())
        .await
        .expect("third apply (mutated)");
    assert_eq!(stats.plugins_updated, 1, "image change must UPDATE plugin");
    assert_eq!(
        stats.bindings_updated, 1,
        "config-blob removal must UPDATE binding"
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

/// The hot-path resolve query, verbatim from the RFE. config-broker
/// will be calling this exact shape post-cutover.
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

/// Tests the raw→validated lowering rejects what bootstrap rejects.
#[test]
fn raw_to_validated_rejects_unknown_plugin_ref() {
    let raw: BootstrapConfigRaw = serde_yaml::from_str(
        r#"
tenants:
- name: phlax
  workspaces:
  - name: mcp
    plugins:
    - name: does-not-exist

plugins:
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:1.0
  egress: none
"#,
    )
    .unwrap();
    assert!(BootstrapConfig::from_raw(raw).is_err());
}

#[test]
fn raw_to_validated_accepts_full_plugin_shape() {
    let raw: BootstrapConfigRaw = serde_yaml::from_str(
        r#"
tenants: []
plugins:
- name: mcp-fetch
  image: ghcr.io/example/mcp-fetch:1.0
  port: 8000
  path: /
  upstream_auth: bearer/example.com
  env:
    LOG_LEVEL: info
  resources:
    memory: 4g
    pids: 1024
  egress:
    allow:
    - host: example.com
      ports: [443]
"#,
    )
    .unwrap();
    let cfg = BootstrapConfig::from_raw(raw).expect("validate full-shape");
    let plug = &cfg.plugins[0];
    assert_eq!(plug.upstream_auth, "bearer/example.com");
    assert_eq!(plug.port, 8000);
    let res = plug.resources.as_ref().unwrap();
    assert_eq!(res["memory"], "4g");
    assert_eq!(res["pids"], 1024);
    // env is in wire shape: array of {name, value}.
    let env = plug.env.as_array().unwrap();
    assert_eq!(env.len(), 1);
    assert_eq!(env[0]["name"], "LOG_LEVEL");
    assert_eq!(env[0]["value"], "info");
}

// Stop the unused-imports check from flagging `RawPluginEntry` (it's
// re-exported via the `plugin_spec` module path so downstream tests
// can build raw entries directly without going through yaml; we don't
// use it directly here but we want to keep it discoverable).
#[allow(dead_code)]
fn _ref(_e: RawPluginEntry) {}
