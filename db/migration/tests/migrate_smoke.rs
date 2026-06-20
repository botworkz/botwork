//! End-to-end migration smoke against a real postgres.
//!
//! Spins a throwaway postgres container via `testcontainers-modules`, points
//! `botwork-migration` at it, and asserts the post-conditions that prove the
//! migrate oneshot would do the right thing in production:
//!
//! 1. `Migrator::up` returns `Ok(())`.
//! 2. The `seaql_migrations` tracking table exists in `public`.
//! 3. A second `up` invocation against the same DB is also `Ok(())`
//!    (idempotency — the oneshot can restart safely).
//!
//! The test is gated on whether docker is reachable. In CI, the runner has
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
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
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
async fn migrator_up_creates_tracking_table_and_is_idempotent() {
    if !docker_available().await {
        eprintln!(
            "IGNORED migrator_up_creates_tracking_table_and_is_idempotent: \
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

    // First run: must succeed (no migrations to apply) and create the
    // sea-orm tracking table as a side effect.
    Migrator::up(&db, None)
        .await
        .expect("Migrator::up first run");

    assert!(
        seaql_migrations_table_exists(&db).await,
        "seaql_migrations table should exist after first Migrator::up"
    );

    // Second run: must also succeed. This is the "the oneshot can restart
    // safely" property that production depends on.
    Migrator::up(&db, None)
        .await
        .expect("Migrator::up second run (idempotency)");

    assert!(
        seaql_migrations_table_exists(&db).await,
        "seaql_migrations table should still exist after second Migrator::up"
    );
}

async fn seaql_migrations_table_exists(db: &DatabaseConnection) -> bool {
    let backend = db.get_database_backend();
    // information_schema is the portable place to ask "does this table
    // exist?". We scope to `public` because that's where SeaORM places its
    // own tracking table and where every entity in this workspace will live
    // in v0.
    let stmt = Statement::from_string(
        backend,
        "SELECT 1 FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name = 'seaql_migrations' \
         LIMIT 1",
    );
    let row = db
        .query_one(stmt)
        .await
        .expect("information_schema query must succeed");
    row.is_some()
}
