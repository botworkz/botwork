//! End-to-end migration smoke against a real postgres.
//!
//! Spins a throwaway postgres container via `testcontainers-modules`, points
//! `botwork-migration` at it, and asserts that the v0 schema (RFE #101)
//! lands in a shape the bootstrap binary and config-broker will be able
//! to query:
//!
//! 1. `Migrator::up` returns `Ok(())`.
//! 2. The `seaql_migrations` tracking table exists in `public` (and lists
//!    our migrations in order).
//! 3. The six v1 tables (`tenant`, `workspace`, `plugin`,
//!    `workspace_plugin`, `agent_session`, `session_worker`) all exist.
//! 4. A second `up` invocation against the same DB is also `Ok(())`
//!    (idempotency — the oneshot can restart safely).
//! 5. FK semantics are wired the right way:
//!    * `workspace.tenant_id → tenant.id`  ON DELETE **RESTRICT**
//!    * `workspace_plugin.workspace_id → workspace.id`  ON DELETE **CASCADE**
//!    * `workspace_plugin.plugin_id → plugin.id`  ON DELETE **RESTRICT**
//!    * `agent_session.tenant_id → tenant.id`  ON DELETE **CASCADE** (RFE #105)
//!    * `agent_session.workspace_id → workspace.id`  ON DELETE **CASCADE** (RFE #105)
//!    * `session_worker.agent_session_id → agent_session.id`  ON DELETE **CASCADE** (RFE #105 round-3)
//!    * `session_worker.plugin_id → plugin.id`  ON DELETE **RESTRICT** (RFE #105 round-3)
//! 6. The composite uniqueness `UNIQUE(tenant_id, name)` on workspace lets
//!    multiple tenants own a workspace called `mcp` without collision.
//! 7. The natural-key UNIQUE on
//!    `agent_session (tenant_id, workspace_id, agent_session_id)` lets the
//!    same `agent_session_id` live under distinct tenants/workspaces but
//!    rejects a duplicate within one workspace (RFE #105).
//! 8. The partial UNIQUE
//!    `(agent_session_id, plugin_id) WHERE reaped_at IS NULL AND
//!     agent_session_id IS NOT NULL` on `session_worker` rejects two
//!    live workers for the same `(agent, plugin)` pair, but accepts
//!    multiple audit rows (reaped_at NOT NULL) and multiple pre-bind
//!    rows (agent_session_id NULL) (RFE #105 round-3).
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
//! lives in `ci.yml` (see the smoke step). This cargo test exists
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
             docker not reachable; full proof runs in ci.yml smoke"
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
    for table in [
        "tenant",
        "workspace",
        "plugin",
        "workspace_plugin",
        "agent_session",
        "session_worker",
    ] {
        assert!(
            table_exists(&db, table).await,
            "{table} table should exist after first Migrator::up"
        );
    }

    // RFE #101 PR2 extends the schema; assert on the ordered list of
    // migrations the schema is now built from. Adding a future
    // migration extends the slice — but the *order* must stay stable,
    // so a misplaced migration that runs out of order in production
    // would trip this assertion.
    let expected_migrations = vec![
        "m20260620_000001_create_core_tables".to_owned(),
        "m20260620_000002_extend_plugin_schema".to_owned(),
        "m20260622_000001_create_agent_session".to_owned(),
        "m20260622_000002_create_session_worker".to_owned(),
    ];
    assert_eq!(
        applied_migration_names(&db).await,
        expected_migrations,
        "seaql_migrations should record the v0 migrations in order"
    );

    // Second run: must also succeed. This is the "the oneshot can restart
    // safely" property that production depends on.
    Migrator::up(&db, None)
        .await
        .expect("Migrator::up second run (idempotency)");

    assert_eq!(
        applied_migration_names(&db).await,
        expected_migrations,
        "idempotent re-run should not duplicate any migration row"
    );

    assert_fk_actions(&db).await;
    assert_workspace_name_unique_per_tenant(&db).await;
    assert_agent_session_natural_key_unique_per_workspace(&db).await;
    assert_session_worker_live_per_plugin_uniqueness(&db).await;
    assert_session_worker_cascade_on_agent_session_delete(&db).await;
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
        // RFE #105: agent_session is a secondary projection of
        // (tenant, workspace); the inbound FKs must cascade so the
        // janitor never has to reconcile dangling rows after a
        // tenant- or workspace-level delete.
        ("fk_agent_session_tenant", "CASCADE"),
        ("fk_agent_session_workspace", "CASCADE"),
        // RFE #105 round-3: session_worker is a per-incarnation
        // projection of the agent session. CASCADE on the session
        // FK so the audit history goes away with the session row;
        // RESTRICT on the plugin FK so a plugin row with live
        // workers can't be silently dropped.
        ("fk_session_worker_agent_session", "CASCADE"),
        ("fk_session_worker_plugin", "RESTRICT"),
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

/// RFE #105: assert the `agent_session` natural-key UNIQUE accepts
/// the same `agent_session_id` under distinct workspaces (and under
/// distinct tenants) but rejects a duplicate within one workspace.
/// This is the bind-time invariant session-broker relies on when it
/// asks "have I seen this triple before?".
///
/// Also drops both tenants at the end and asserts the agent_session
/// rows cascaded — proving the FK `ON DELETE CASCADE` is wired the
/// way RFE #105 specifies. We delete in workspace-first order to
/// stay within the workspace.tenant_id RESTRICT posture; the
/// agent_session rows then go away with their workspace.
async fn assert_agent_session_natural_key_unique_per_workspace(db: &DatabaseConnection) {
    let backend = db.get_database_backend();
    // Two tenants with one workspace each. Plant the same
    // agent_session_id under both — must succeed, because the unique
    // key includes tenant_id + workspace_id.
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO tenant (id, name) VALUES \
           (gen_random_uuid(), 'agent-tenant-a'), \
           (gen_random_uuid(), 'agent-tenant-b')"
            .to_owned(),
    ))
    .await
    .expect("seed agent-session tenants");
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO workspace (id, tenant_id, name) \
         SELECT gen_random_uuid(), id, 'mcp' \
         FROM tenant WHERE name IN ('agent-tenant-a', 'agent-tenant-b')"
            .to_owned(),
    ))
    .await
    .expect("seed agent-session workspaces");

    // Same agent_session_id under both tenants — must both insert.
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO agent_session (id, tenant_id, workspace_id, agent_session_id, state) \
         SELECT gen_random_uuid(), t.id, w.id, 'goose-abc', 'active' \
         FROM tenant t JOIN workspace w ON w.tenant_id = t.id \
         WHERE t.name IN ('agent-tenant-a', 'agent-tenant-b')"
            .to_owned(),
    ))
    .await
    .expect(
        "same agent_session_id under distinct (tenant, workspace) must both insert via ux_agent_session_natural_key",
    );

    // Repeat under one tenant/workspace — must violate the UNIQUE.
    let dup_stmt = Statement::from_string(
        backend,
        "INSERT INTO agent_session (id, tenant_id, workspace_id, agent_session_id, state) \
         SELECT gen_random_uuid(), t.id, w.id, 'goose-abc', 'active' \
         FROM tenant t JOIN workspace w ON w.tenant_id = t.id \
         WHERE t.name = 'agent-tenant-a'"
            .to_owned(),
    );
    let dup = db.execute(dup_stmt).await;
    assert!(
        dup.is_err(),
        "second agent_session row for same (tenant, workspace, agent_session_id) \
         must violate ux_agent_session_natural_key"
    );

    // Drop workspaces — CASCADE should take the agent_session rows
    // with them. Workspace delete also reaches workspace_plugin /
    // agent_session via CASCADE; the only edge with RESTRICT
    // semantics is workspace.tenant_id (tested above), which is why
    // we do the workspace delete before the tenant delete.
    db.execute(Statement::from_string(
        backend,
        "DELETE FROM workspace WHERE tenant_id IN \
         (SELECT id FROM tenant WHERE name IN ('agent-tenant-a', 'agent-tenant-b'))"
            .to_owned(),
    ))
    .await
    .expect("workspace cleanup (cascades into agent_session)");

    // Assert the cascade actually fired.
    let remaining = db
        .query_one(Statement::from_string(
            backend,
            "SELECT count(*)::int AS c FROM agent_session WHERE agent_session_id = 'goose-abc'"
                .to_owned(),
        ))
        .await
        .expect("count agent_session rows after cascade")
        .expect("count returns one row");
    let count: i32 = remaining.try_get("", "c").expect("c column");
    assert_eq!(
        count, 0,
        "agent_session rows must cascade-delete with their workspace"
    );

    db.execute(Statement::from_string(
        backend,
        "DELETE FROM tenant WHERE name IN ('agent-tenant-a', 'agent-tenant-b')".to_owned(),
    ))
    .await
    .expect("agent-session tenant cleanup");
}

/// RFE #105 round-3: assert the `session_worker` partial UNIQUE
/// enforces "one live worker per (agent_session, plugin)" but lets
/// audit history (`reaped_at IS NOT NULL`) and the pre-bind window
/// (`agent_session_id IS NULL`) live alongside it. Each is a property
/// session-broker leans on for its routing + recovery semantics:
///
/// * two live workers for the same (agent, plugin) pair would be a
///   routing leak (session-broker keys on `(plugin, mcp_session_id)`,
///   not on container name);
/// * an unlimited number of *audit* rows must coexist because a single
///   session reconnects to many container incarnations over its
///   lifetime — that's exactly the cost/billing surface the row is
///   here to record;
/// * a row in the spawn-to-first-bind window has `agent_session_id`
///   NULL, and the partial UNIQUE explicitly exempts those rows from
///   the constraint so concurrent spawns under separate sessions can
///   both have pre-bind workers for the same plugin.
async fn assert_session_worker_live_per_plugin_uniqueness(db: &DatabaseConnection) {
    let backend = db.get_database_backend();
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO tenant (id, name) VALUES (gen_random_uuid(), 'worker-tenant')".to_owned(),
    ))
    .await
    .expect("seed worker tenant");
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO workspace (id, tenant_id, name) \
         SELECT gen_random_uuid(), id, 'mcp' FROM tenant WHERE name = 'worker-tenant'"
            .to_owned(),
    ))
    .await
    .expect("seed worker workspace");
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO agent_session (id, tenant_id, workspace_id, agent_session_id, state) \
         SELECT gen_random_uuid(), t.id, w.id, 'goose-worker-1', 'active' \
         FROM tenant t JOIN workspace w ON w.tenant_id = t.id \
         WHERE t.name = 'worker-tenant'"
            .to_owned(),
    ))
    .await
    .expect("seed worker agent_session");
    // Use a placeholder plugin row — agent_session schema doesn't
    // reference it directly, but session_worker.plugin_id does.
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO plugin (id, name, image, port, path, upstream_auth, env, egress) VALUES \
           (gen_random_uuid(), 'worker-plugin-a', 'botwork/p:1', 8000, '/', 'none', \
            '[]'::jsonb, '{\"mode\":\"none\"}'::jsonb), \
           (gen_random_uuid(), 'worker-plugin-b', 'botwork/p:1', 8000, '/', 'none', \
            '[]'::jsonb, '{\"mode\":\"none\"}'::jsonb)"
            .to_owned(),
    ))
    .await
    .expect("seed worker plugins");

    // 1. Live worker A under plugin-a: OK.
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO session_worker \
           (id, agent_session_id, plugin_id, container_name, container_ip, mcp_session_id, spawned_at) \
         SELECT gen_random_uuid(), a.id, p.id, 'mcp_session_aaaaaaaaaaaa', '172.20.0.10', \
                'live-sess-a', CURRENT_TIMESTAMP \
         FROM agent_session a CROSS JOIN plugin p \
         WHERE a.agent_session_id = 'goose-worker-1' AND p.name = 'worker-plugin-a'"
            .to_owned(),
    ))
    .await
    .expect("first live worker for (session, plugin-a) must insert");

    // 2. Second live worker, same (session, plugin-a): MUST violate
    //    ux_session_worker_live_per_plugin.
    let dup_live = db
        .execute(Statement::from_string(
            backend,
            "INSERT INTO session_worker \
               (id, agent_session_id, plugin_id, container_name, container_ip, mcp_session_id, spawned_at) \
             SELECT gen_random_uuid(), a.id, p.id, 'mcp_session_bbbbbbbbbbbb', '172.20.0.11', \
                    'live-sess-dup', CURRENT_TIMESTAMP \
             FROM agent_session a CROSS JOIN plugin p \
             WHERE a.agent_session_id = 'goose-worker-1' AND p.name = 'worker-plugin-a'"
                .to_owned(),
        ))
        .await;
    assert!(
        dup_live.is_err(),
        "second LIVE worker for same (agent_session, plugin) must violate ux_session_worker_live_per_plugin"
    );

    // 3. Live worker under plugin-b for the same session: OK
    //    (different plugin → different unique-key tuple).
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO session_worker \
           (id, agent_session_id, plugin_id, container_name, container_ip, mcp_session_id, spawned_at) \
         SELECT gen_random_uuid(), a.id, p.id, 'mcp_session_cccccccccccc', '172.20.0.12', \
                'live-sess-b', CURRENT_TIMESTAMP \
         FROM agent_session a CROSS JOIN plugin p \
         WHERE a.agent_session_id = 'goose-worker-1' AND p.name = 'worker-plugin-b'"
            .to_owned(),
    ))
    .await
    .expect("live worker for (session, plugin-b) must insert under distinct plugin");

    // 4. Reap the plugin-a worker, then insert a fresh live one:
    //    audit rows are exempt from the partial UNIQUE.
    db.execute(Statement::from_string(
        backend,
        "UPDATE session_worker SET reaped_at = CURRENT_TIMESTAMP \
         WHERE container_name = 'mcp_session_aaaaaaaaaaaa'"
            .to_owned(),
    ))
    .await
    .expect("reap first plugin-a worker");
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO session_worker \
           (id, agent_session_id, plugin_id, container_name, container_ip, mcp_session_id, spawned_at) \
         SELECT gen_random_uuid(), a.id, p.id, 'mcp_session_dddddddddddd', '172.20.0.13', \
                'live-sess-a2', CURRENT_TIMESTAMP \
         FROM agent_session a CROSS JOIN plugin p \
         WHERE a.agent_session_id = 'goose-worker-1' AND p.name = 'worker-plugin-a'"
            .to_owned(),
    ))
    .await
    .expect("fresh live worker after the first was reaped must insert (audit exemption)");

    // 5. Two pre-bind rows (agent_session_id NULL) under the same
    //    plugin: OK (exempt from partial UNIQUE).
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO session_worker \
           (id, agent_session_id, plugin_id, container_name, container_ip, mcp_session_id, spawned_at) \
         SELECT gen_random_uuid(), NULL, p.id, 'mcp_session_eeeeeeeeeeee', '172.20.0.14', \
                '', CURRENT_TIMESTAMP \
         FROM plugin p WHERE p.name = 'worker-plugin-a'"
            .to_owned(),
    ))
    .await
    .expect("first pre-bind worker (agent_session_id NULL) must insert");
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO session_worker \
           (id, agent_session_id, plugin_id, container_name, container_ip, mcp_session_id, spawned_at) \
         SELECT gen_random_uuid(), NULL, p.id, 'mcp_session_ffffffffffff', '172.20.0.15', \
                '', CURRENT_TIMESTAMP \
         FROM plugin p WHERE p.name = 'worker-plugin-a'"
            .to_owned(),
    ))
    .await
    .expect("second pre-bind worker must also insert (NULL exempts from partial UNIQUE)");

    // 6. Container name is globally unique: planting the same
    //    container_name twice must hard-fail on the
    //    ux_session_worker_container_name index.
    let dup_name = db
        .execute(Statement::from_string(
            backend,
            "INSERT INTO session_worker \
               (id, agent_session_id, plugin_id, container_name, container_ip, mcp_session_id, spawned_at) \
             SELECT gen_random_uuid(), NULL, p.id, 'mcp_session_eeeeeeeeeeee', '172.20.0.99', \
                    '', CURRENT_TIMESTAMP \
             FROM plugin p WHERE p.name = 'worker-plugin-b'"
                .to_owned(),
        ))
        .await;
    assert!(
        dup_name.is_err(),
        "duplicate container_name must violate ux_session_worker_container_name"
    );

    // Cleanup. session_worker rows go first because the plugin FK is
    // RESTRICT — dropping the plugin row first would 23503 against
    // the live + audit workers we just planted.
    db.execute(Statement::from_string(
        backend,
        "DELETE FROM session_worker WHERE plugin_id IN \
         (SELECT id FROM plugin WHERE name IN ('worker-plugin-a', 'worker-plugin-b'))"
            .to_owned(),
    ))
    .await
    .expect("session_worker cleanup");
    db.execute(Statement::from_string(
        backend,
        "DELETE FROM plugin WHERE name IN ('worker-plugin-a', 'worker-plugin-b')".to_owned(),
    ))
    .await
    .expect("plugin cleanup");
    db.execute(Statement::from_string(
        backend,
        "DELETE FROM workspace WHERE tenant_id IN \
         (SELECT id FROM tenant WHERE name = 'worker-tenant')"
            .to_owned(),
    ))
    .await
    .expect("workspace cleanup");
    db.execute(Statement::from_string(
        backend,
        "DELETE FROM tenant WHERE name = 'worker-tenant'".to_owned(),
    ))
    .await
    .expect("tenant cleanup");
}

/// RFE #105 round-3: deleting an `agent_session` row must CASCADE into
/// its `session_worker` rows. This pairs with the
/// `assert_agent_session_natural_key_unique_per_workspace` test above
/// (which proves workspace-delete cascades into agent_session); here
/// we cover the inner cascade so a full
/// tenant-delete → workspace-delete → agent_session-delete →
/// session_worker-delete chain has each edge tested in one test pass.
///
/// Touches no other tables: we drop the workers ourselves at the end
/// (since the test framework can't tell ahead of time which rows came
/// from this helper vs the previous ones).
async fn assert_session_worker_cascade_on_agent_session_delete(db: &DatabaseConnection) {
    let backend = db.get_database_backend();
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO tenant (id, name) VALUES (gen_random_uuid(), 'cascade-tenant')".to_owned(),
    ))
    .await
    .expect("seed cascade tenant");
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO workspace (id, tenant_id, name) \
         SELECT gen_random_uuid(), id, 'mcp' FROM tenant WHERE name = 'cascade-tenant'"
            .to_owned(),
    ))
    .await
    .expect("seed cascade workspace");
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO agent_session (id, tenant_id, workspace_id, agent_session_id, state) \
         SELECT gen_random_uuid(), t.id, w.id, 'goose-cascade-1', 'active' \
         FROM tenant t JOIN workspace w ON w.tenant_id = t.id \
         WHERE t.name = 'cascade-tenant'"
            .to_owned(),
    ))
    .await
    .expect("seed cascade agent_session");
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO plugin (id, name, image, port, path, upstream_auth, env, egress) \
         VALUES (gen_random_uuid(), 'cascade-plugin', 'botwork/p:1', 8000, '/', 'none', \
                 '[]'::jsonb, '{\"mode\":\"none\"}'::jsonb)"
            .to_owned(),
    ))
    .await
    .expect("seed cascade plugin");

    // Plant a live worker + an already-reaped audit worker under the
    // same session. Both should disappear when the session row is
    // dropped.
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO session_worker \
           (id, agent_session_id, plugin_id, container_name, container_ip, mcp_session_id, spawned_at) \
         SELECT gen_random_uuid(), a.id, p.id, 'mcp_session_cascade01', '172.20.0.20', \
                'cascade-live', CURRENT_TIMESTAMP \
         FROM agent_session a CROSS JOIN plugin p \
         WHERE a.agent_session_id = 'goose-cascade-1' AND p.name = 'cascade-plugin'"
            .to_owned(),
    ))
    .await
    .expect("seed live cascade worker");
    db.execute(Statement::from_string(
        backend,
        "INSERT INTO session_worker \
           (id, agent_session_id, plugin_id, container_name, container_ip, mcp_session_id, \
            spawned_at, reaped_at) \
         SELECT gen_random_uuid(), a.id, p.id, 'mcp_session_cascade02', '172.20.0.21', \
                'cascade-audit', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP \
         FROM agent_session a CROSS JOIN plugin p \
         WHERE a.agent_session_id = 'goose-cascade-1' AND p.name = 'cascade-plugin'"
            .to_owned(),
    ))
    .await
    .expect("seed reaped (audit) cascade worker");

    db.execute(Statement::from_string(
        backend,
        "DELETE FROM agent_session WHERE agent_session_id = 'goose-cascade-1'".to_owned(),
    ))
    .await
    .expect("agent_session delete must cascade into session_worker");

    let remaining = db
        .query_one(Statement::from_string(
            backend,
            "SELECT count(*)::int AS c FROM session_worker \
             WHERE container_name IN ('mcp_session_cascade01', 'mcp_session_cascade02')"
                .to_owned(),
        ))
        .await
        .expect("count session_worker rows after cascade")
        .expect("count returns one row");
    let count: i32 = remaining.try_get("", "c").expect("c column");
    assert_eq!(
        count, 0,
        "both live and audit session_worker rows must cascade-delete with their parent agent_session"
    );

    // Cleanup the plugin + tenant + workspace.
    db.execute(Statement::from_string(
        backend,
        "DELETE FROM plugin WHERE name = 'cascade-plugin'".to_owned(),
    ))
    .await
    .expect("cascade plugin cleanup");
    db.execute(Statement::from_string(
        backend,
        "DELETE FROM workspace WHERE tenant_id IN \
         (SELECT id FROM tenant WHERE name = 'cascade-tenant')"
            .to_owned(),
    ))
    .await
    .expect("cascade workspace cleanup");
    db.execute(Statement::from_string(
        backend,
        "DELETE FROM tenant WHERE name = 'cascade-tenant'".to_owned(),
    ))
    .await
    .expect("cascade tenant cleanup");
}
