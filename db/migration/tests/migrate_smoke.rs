//! End-to-end migration smoke against a real postgres.
//!
//! Spins a throwaway postgres container via `testcontainers-modules`, points
//! `botwork-migration` at it, and asserts that the v0 schema (RFE #101)
//! lands in a shape the bootstrap binary and config-broker will be able
//! to query:
//!
//! 1. `Migrator::up` returns `Ok(())`.
//! 2. The `seaql_migrations` tracking table exists in `public` (and lists
//!    our migration).
//! 3. The four core tables (`tenant`, `workspace`, `plugin`,
//!    `workspace_plugin`) all exist.
//! 4. A second `up` invocation against the same DB is also `Ok(())`
//!    (idempotency — the oneshot can restart safely).
//! 5. FK semantics are wired the right way:
//!    * `workspace.tenant_id → tenant.id`  ON DELETE **RESTRICT**
//!    * `workspace_plugin.workspace_id → workspace.id`  ON DELETE **CASCADE**
//!    * `workspace_plugin.plugin_id → plugin.id`  ON DELETE **RESTRICT**
//! 6. The composite uniqueness `UNIQUE(tenant_id, name)` on workspace lets
//!    multiple tenants own a workspace called `mcp` without collision.
//!
//! The test gates on whether docker is reachable. In CI, the runner has
//! docker and the test runs in full. On dev machines without docker, the
//! gate prints a structured `IGNORED` line and the test passes — same shape
//! as the workspace's other docker-dependent tests so `cargo test` stays
//! green on a laptop without docker.
//!
//! # Why this lives in `db/migration/tests/`, not a separate crate
//!
//! The end-to-end production-path proof for the migration container image
//! lives in `containers.yml` (see the smoke step). This cargo test exists
//! for fast iteration on schema changes — it exercises the *rust* code path
//! that the container image's CMD invokes, but skips the container build.

use std::time::Duration;

use botwork_entity::connection::connect;
use botwork_migration::Migrator;
use sea_orm::{ConnectionTrait, DatabaseConnection, FromQueryResult, Statement};
use sea_orm_migration::MigratorTrait;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

/// Tag we pin testcontainers to. Mirrors what we'll pin in vm's shasset for
/// production. Bumping these together avoids "tests use a different
/// postgres than prod" drift.
///
/// `16-alpine` matches the major we plan to ship; alpine keeps the test
/// pull cheap. The vm-side image is digest-pinned; this one is tag-pinned
/// because testcontainers-modules doesn't expose a digest API.
const POSTGRES_TAG: &str = "16-alpine";

async fn start_postgres() -> Result<(testcontainers::ContainerAsync<Postgres>, String), String> {
    use testcontainers::ImageExt;

    let image = Postgres::default()
        .with_db_name("botwork")
        .with_user("botwork")
        .with_password("test")
        .with_tag(POSTGRES_TAG);

    let container = match image.start().await {
        Ok(c) => c,
        Err(err) => return Err(format!("start container: {err}")),
    };
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
    // The container reports ready before postgres has finished its second
    // initdb cycle on alpine; SeaORM connect can race that. Retry a small
    // number of times with backoff before bubbling the error.
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

/// Cheap "is docker reachable?" probe. If it fails we mark the test as
/// skipped via a clearly-labelled stdout line and return `false`. The
/// `_container` rebind in callers ensures the dropped postgres container is
/// torn down once the test returns even if it panics.
async fn docker_available() -> bool {
    use testcontainers::core::WaitFor;
    use testcontainers::GenericImage;

    // Use the helloworld image because it exits immediately on its own;
    // we just want the "can we talk to dockerd at all" answer. Wrap in
    // a short timeout so missing docker fails fast instead of waiting on
    // bollard's default connect timeout.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrator_up_lands_v0_schema_and_is_idempotent() {
    if !docker_available().await {
        eprintln!(
            "IGNORED migrator_up_lands_v0_schema_and_is_idempotent: \
             docker not reachable; full proof runs in containers.yml smoke"
        );
        return;
    }

    let (_pg, url) = match start_postgres().await {
        Ok(out) => out,
        Err(err) => panic!("postgres container would not start: {err}"),
    };

    let db = connect_with_retry(&url)
        .await
        .expect("connect to ephemeral postgres");

    // First run: must succeed and create the v0 schema as a side effect.
    Migrator::up(&db, None)
        .await
        .expect("Migrator::up first run");

    assert!(
        table_exists(&db, "seaql_migrations").await,
        "seaql_migrations table should exist after first Migrator::up"
    );
    for table in ["tenant", "workspace", "plugin", "workspace_plugin"] {
        assert!(
            table_exists(&db, table).await,
            "{table} table should exist after first Migrator::up"
        );
    }

    assert_eq!(
        applied_migration_names(&db).await,
        vec!["m20260620_000001_create_core_tables".to_owned()],
        "seaql_migrations should record exactly the one v0 migration"
    );

    // Second run: must also succeed. This is the "the oneshot can restart
    // safely" property that production depends on.
    Migrator::up(&db, None)
        .await
        .expect("Migrator::up second run (idempotency)");

    assert_eq!(
        applied_migration_names(&db).await,
        vec!["m20260620_000001_create_core_tables".to_owned()],
        "idempotent re-run should not duplicate the migration row"
    );

    assert_fk_actions(&db).await;
    assert_workspace_name_unique_per_tenant(&db).await;
}

async fn table_exists(db: &DatabaseConnection, name: &str) -> bool {
    let backend = db.get_database_backend();
    let stmt = Statement::from_sql_and_values(
        backend,
        "SELECT 1 FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name = $1 LIMIT 1",
        [name.into()],
    );
    db.query_one(stmt)
        .await
        .expect("information_schema query must succeed")
        .is_some()
}

#[derive(FromQueryResult)]
struct MigrationVersionRow {
    version: String,
}

async fn applied_migration_names(db: &DatabaseConnection) -> Vec<String> {
    let backend = db.get_database_backend();
    let stmt = Statement::from_string(
        backend,
        "SELECT version FROM seaql_migrations ORDER BY version".to_owned(),
    );
    MigrationVersionRow::find_by_statement(stmt)
        .all(db)
        .await
        .expect("seaql_migrations SELECT must succeed")
        .into_iter()
        .map(|r| r.version)
        .collect()
}

#[derive(Debug, FromQueryResult)]
struct FkActionRow {
    delete_rule: String,
}

/// Look up the ON DELETE action for a given FK by its constraint name and
/// assert it. RFE #101 nails these down explicitly so a future migration
/// touching the FK has to be deliberate about changing semantics.
async fn assert_fk_actions(db: &DatabaseConnection) {
    for (name, expected) in [
        ("fk_workspace_tenant", "RESTRICT"),
        ("fk_workspace_plugin_workspace", "CASCADE"),
        ("fk_workspace_plugin_plugin", "RESTRICT"),
    ] {
        let backend = db.get_database_backend();
        let stmt = Statement::from_sql_and_values(
            backend,
            "SELECT delete_rule \
             FROM information_schema.referential_constraints \
             WHERE constraint_name = $1",
            [name.into()],
        );
        let row = FkActionRow::find_by_statement(stmt)
            .one(db)
            .await
            .expect("referential_constraints query must succeed")
            .unwrap_or_else(|| panic!("FK {name} should exist"));
        assert_eq!(
            row.delete_rule, expected,
            "FK {name} ON DELETE must be {expected}"
        );
    }
}

/// Plant a tenant + a `mcp` workspace under each of two tenants and assert
/// no unique-key collision. The point is that `workspace.name` alone is NOT
/// unique — the business key is `(tenant_id, name)` — and the design
/// explicitly counts on every new tenant getting a default `mcp` workspace.
async fn assert_workspace_name_unique_per_tenant(db: &DatabaseConnection) {
    let backend = db.get_database_backend();
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO tenant (id, name) VALUES \
           (gen_random_uuid(), 'tenant-a'), \
           (gen_random_uuid(), 'tenant-b')"
            .to_owned(),
    ))
    .await
    .expect("seed tenants");
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO workspace (id, tenant_id, name) \
         SELECT gen_random_uuid(), id, 'mcp' FROM tenant WHERE name IN ('tenant-a', 'tenant-b')"
            .to_owned(),
    ))
    .await
    .expect("two workspaces named 'mcp' under distinct tenants must both insert");

    // Same name twice under the SAME tenant must fail the unique index.
    let dup_stmt = Statement::from_string(
        backend,
        "INSERT INTO workspace (id, tenant_id, name) \
         SELECT gen_random_uuid(), id, 'mcp' FROM tenant WHERE name = 'tenant-a'"
            .to_owned(),
    );
    let dup = db.execute(dup_stmt).await;
    assert!(
        dup.is_err(),
        "second 'mcp' under same tenant must violate ux_workspace_tenant_name"
    );

    // Clean up so the test is repeatable against the same container.
    //
    // Workspace rows must go first because the inbound FK from
    // workspace.tenant_id has ON DELETE RESTRICT — which is the
    // property the rest of this test exists to assert. Dropping in
    // the wrong order panics this cleanup with a 23503
    // foreign_key_violation, which is exactly what we want production
    // operators to see; the test just has to walk the rows in the
    // correct order to demonstrate it.
    db.execute(Statement::from_string(
        backend,
        "DELETE FROM workspace WHERE tenant_id IN \
         (SELECT id FROM tenant WHERE name IN ('tenant-a', 'tenant-b'))"
            .to_owned(),
    ))
    .await
    .expect("workspace cleanup");
    db.execute(Statement::from_string(
        backend,
        "DELETE FROM tenant WHERE name IN ('tenant-a', 'tenant-b')".to_owned(),
    ))
    .await
    .expect("tenant cleanup");
}
