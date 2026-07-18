//! End-to-end write-through tests for the RFE #105 PR2 agent_session
//! integration in session-broker.
//!
//! Spins a throwaway postgres + runs migrations + bootstrap-style row
//! seed, then drives the `AgentSessionWriter` API directly and asserts
//! the row shape that comes back from the DB. This is the cargo-side
//! fast-iteration test; the end-to-end container-image proof of the
//! production write-through path lives in `ci.yml` once the
//! companion smoke step lands.
//!
//! Gated on docker the same way `db/migration/tests/migrate_smoke.rs`
//! and `bootstrap/tests/bootstrap_smoke.rs` are; the `IGNORED` short-
//! circuit keeps `cargo test` green on dev machines without docker.

use std::sync::Arc;
use std::time::Duration;

use botwork_bootstrap::{apply, BootstrapConfig, BootstrapConfigRaw};
use botwork_entity::connection::connect;
use botwork_entity::{agent_session, tenant, workspace};
use botwork_migration::Migrator;
use botwork_session_broker::agent_session::AgentSessionWriter;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
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
    Ok((
        container,
        format!("postgres://botwork:test@{host}:{port}/botwork"),
    ))
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

fn seed_config() -> BootstrapConfig {
    // Minimum bootstrap that PR2's writer needs: one tenant + one
    // workspace + at least one validated plugin (because the
    // bootstrap validator refuses a workspace that binds an unknown
    // plugin). agent_session itself is keyed on
    // `(tenant_id, workspace_id, agent_session_id)` and never
    // references the plugin row.
    //
    // We round-trip through the YAML loader rather than constructing
    // BootstrapConfig directly because the public surface of
    // `botwork-bootstrap` exposes only the raw shape + `from_raw`
    // (`PluginEntry`/`ValidatedPlugin` aren't re-exported as a
    // structural-build path).
    let yaml = r#"
tenants:
- name: phlax
  workspaces:
  - name: mcp
    plugins:
    - name: mcp-bash

plugins:
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:1.0
  egress: none
"#;
    let raw: BootstrapConfigRaw = serde_yaml::from_str(yaml).expect("parse seed bootstrap yaml");
    BootstrapConfig::from_raw(raw).expect("validate seed bootstrap yaml")
}

async fn workspace_id_for(
    db: &DatabaseConnection,
    tenant_name: &str,
    workspace_name: &str,
) -> (uuid::Uuid, uuid::Uuid) {
    let t = tenant::Entity::find()
        .filter(tenant::Column::Name.eq(tenant_name))
        .one(db)
        .await
        .expect("tenant lookup")
        .expect("seeded tenant exists");
    let w = workspace::Entity::find()
        .filter(workspace::Column::TenantId.eq(t.id))
        .filter(workspace::Column::Name.eq(workspace_name))
        .one(db)
        .await
        .expect("workspace lookup")
        .expect("seeded workspace exists");
    (t.id, w.id)
}

async fn fetch_row(
    db: &DatabaseConnection,
    tenant_id: uuid::Uuid,
    workspace_id: uuid::Uuid,
    agent_session_id: &str,
) -> agent_session::Model {
    agent_session::Entity::find()
        .filter(agent_session::Column::TenantId.eq(tenant_id))
        .filter(agent_session::Column::WorkspaceId.eq(workspace_id))
        .filter(agent_session::Column::AgentSessionId.eq(agent_session_id))
        .one(db)
        .await
        .expect("agent_session lookup")
        .expect("row exists for the seeded triple")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_through_record_bind_agent_inserts_then_updates() {
    if !docker_available().await {
        eprintln!(
            "IGNORED write_through_record_bind_agent_inserts_then_updates: \
             docker not reachable; full proof runs in ci.yml smoke"
        );
        return;
    }
    let (_pg, url) = start_postgres()
        .await
        .expect("postgres container must start");
    let db = Arc::new(
        connect_with_retry(&url)
            .await
            .expect("connect to ephemeral postgres"),
    );
    Migrator::up(db.as_ref(), None)
        .await
        .expect("migrations must apply");
    apply(db.as_ref(), &seed_config())
        .await
        .expect("seed tenant + workspace");

    let (tenant_id, workspace_id) = workspace_id_for(db.as_ref(), "phlax", "mcp").await;
    let writer = AgentSessionWriter::new(Arc::clone(&db));

    // First bind-agent: INSERT path. New row in `state=active` with
    // `reactivation_count=0`.
    writer.record_bind_agent("phlax", "mcp", "goose-abc").await;
    let row = fetch_row(db.as_ref(), tenant_id, workspace_id, "goose-abc").await;
    assert_eq!(row.state, agent_session::state::ACTIVE);
    assert_eq!(row.reactivation_count, 0);
    assert_eq!(row.tenant_id, tenant_id);
    assert_eq!(row.workspace_id, workspace_id);
    let created_at_snapshot = row.created_at;
    let last_active_snapshot = row.last_active_at;

    // Second bind-agent under same triple: row already exists in
    // `active`, so this is NOT a reactivation. `reactivation_count`
    // must stay 0; `created_at` must not change (immutable);
    // `last_active_at` must move forward.
    tokio::time::sleep(Duration::from_millis(20)).await;
    writer.record_bind_agent("phlax", "mcp", "goose-abc").await;
    let row = fetch_row(db.as_ref(), tenant_id, workspace_id, "goose-abc").await;
    assert_eq!(
        row.reactivation_count, 0,
        "bind on already-active row must not bump reactivation_count"
    );
    assert_eq!(
        row.created_at, created_at_snapshot,
        "created_at must be immutable"
    );
    assert!(
        row.last_active_at > last_active_snapshot,
        "last_active_at must move forward on every bind"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_through_lifecycle_active_grace_inactive_reactivate() {
    if !docker_available().await {
        eprintln!(
            "IGNORED write_through_lifecycle_active_grace_inactive_reactivate: \
             docker not reachable; full proof runs in ci.yml smoke"
        );
        return;
    }
    let (_pg, url) = start_postgres()
        .await
        .expect("postgres container must start");
    let db = Arc::new(
        connect_with_retry(&url)
            .await
            .expect("connect to ephemeral postgres"),
    );
    Migrator::up(db.as_ref(), None).await.expect("migrations");
    apply(db.as_ref(), &seed_config()).await.expect("seed");

    let (tenant_id, workspace_id) = workspace_id_for(db.as_ref(), "phlax", "mcp").await;
    let writer = AgentSessionWriter::new(Arc::clone(&db));

    writer
        .record_bind_agent("phlax", "mcp", "goose-lifecycle")
        .await;
    assert_eq!(
        fetch_row(db.as_ref(), tenant_id, workspace_id, "goose-lifecycle")
            .await
            .state,
        agent_session::state::ACTIVE
    );

    // active → grace (transport closed).
    writer.record_grace("phlax", "mcp", "goose-lifecycle").await;
    assert_eq!(
        fetch_row(db.as_ref(), tenant_id, workspace_id, "goose-lifecycle")
            .await
            .state,
        agent_session::state::GRACE
    );

    // grace → inactive (grace timer fired, container reaped).
    writer
        .record_inactive("phlax", "mcp", "goose-lifecycle")
        .await;
    assert_eq!(
        fetch_row(db.as_ref(), tenant_id, workspace_id, "goose-lifecycle")
            .await
            .state,
        agent_session::state::INACTIVE
    );

    // Reactivation: client reconnects, broker spawns a new container.
    // The bind-agent path moves the row back to `active` and bumps
    // `reactivation_count`.
    writer
        .record_bind_agent("phlax", "mcp", "goose-lifecycle")
        .await;
    let row = fetch_row(db.as_ref(), tenant_id, workspace_id, "goose-lifecycle").await;
    assert_eq!(row.state, agent_session::state::ACTIVE);
    assert_eq!(
        row.reactivation_count, 1,
        "inactive → active must bump reactivation_count"
    );

    // Second reactivation cycle: grace → reactivate (without going
    // through inactive). reactivation_count must bump again because
    // `grace` is also a non-active state per the RFE.
    writer.record_grace("phlax", "mcp", "goose-lifecycle").await;
    writer
        .record_bind_agent("phlax", "mcp", "goose-lifecycle")
        .await;
    let row = fetch_row(db.as_ref(), tenant_id, workspace_id, "goose-lifecycle").await;
    assert_eq!(row.state, agent_session::state::ACTIVE);
    assert_eq!(
        row.reactivation_count, 2,
        "grace → active must also bump reactivation_count"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_through_unknown_tenant_fails_soft() {
    if !docker_available().await {
        eprintln!(
            "IGNORED write_through_unknown_tenant_fails_soft: \
             docker not reachable; full proof runs in ci.yml smoke"
        );
        return;
    }
    let (_pg, url) = start_postgres()
        .await
        .expect("postgres container must start");
    let db = Arc::new(
        connect_with_retry(&url)
            .await
            .expect("connect to ephemeral postgres"),
    );
    Migrator::up(db.as_ref(), None).await.expect("migrations");
    apply(db.as_ref(), &seed_config()).await.expect("seed");

    let writer = AgentSessionWriter::new(Arc::clone(&db));

    // Unknown tenant — the writer must NOT panic; it logs a warn
    // and carries on. This is the "fail-soft observability mode"
    // posture from the agent_session.rs module docs.
    writer
        .record_bind_agent("does-not-exist", "mcp", "goose-orphan")
        .await;

    // And the agent_session table stays empty for that triple.
    let count = agent_session::Entity::find()
        .filter(agent_session::Column::AgentSessionId.eq("goose-orphan"))
        .all(db.as_ref())
        .await
        .expect("query")
        .len();
    assert_eq!(
        count, 0,
        "unknown tenant must not produce an agent_session row"
    );
}
