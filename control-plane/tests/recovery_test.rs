//! End-to-end recovery test against a real postgres via testcontainers.
//!
//! Round-3 follow-up to RFE #105: control-plane's cold-start recovery
//! now reads `session_worker` JOIN `agent_session` from postgres rather
//! than polling session-broker's HTTP admin endpoint. The JOIN logic
//! lives in `recovery::fetch_live_sessions`; this test stands up a
//! real DB, seeds it with the full lifecycle shape session-broker
//! produces, and asserts the JOIN projects to the same wire shape the
//! pre-cutover endpoint did.
//!
//! Gated on docker reachability — same pattern as
//! `config-broker/tests/integration.rs`.

use std::sync::Arc;
use std::time::Duration;

use botwork_bootstrap::{apply, BootstrapConfig, BootstrapConfigRaw};
use botwork_control_plane::{run_recovery_with_retries, SessionStore};
use botwork_entity::connection::connect;
use botwork_entity::{agent_session, session_worker};
use botwork_migration::Migrator;
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
};
use sea_orm_migration::MigratorTrait;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

const POSTGRES_TAG: &str = "16-alpine";

/// Bootstrap fixture: one tenant, one workspace, two plugins with
/// distinct `egress:` blocks (so the recovery JOIN's pass-through of
/// `plugin.egress` is observable per-row).
const SAMPLE_YAML: &str = r#"
tenants:
- name: phlax
  workspaces:
  - name: mcp
    plugins:
    - name: mcp-bash
    - name: mcp-fetch

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

/// Pull the (tenant_id, workspace_id, plugin_id) PKs from the
/// bootstrap-applied rows so the seed helpers can compose a realistic
/// session_worker row. All three names come from `SAMPLE_YAML`.
async fn resolve_ids(db: &DatabaseConnection) -> (Uuid, Uuid, Uuid, Uuid) {
    use botwork_entity::{plugin, tenant, workspace};
    let tenant_row = tenant::Entity::find()
        .filter(tenant::Column::Name.eq("phlax"))
        .one(db)
        .await
        .expect("query tenant")
        .expect("tenant row");
    let workspace_row = workspace::Entity::find()
        .filter(workspace::Column::TenantId.eq(tenant_row.id))
        .filter(workspace::Column::Name.eq("mcp"))
        .one(db)
        .await
        .expect("query workspace")
        .expect("workspace row");
    let bash = plugin::Entity::find()
        .filter(plugin::Column::Name.eq("mcp-bash"))
        .one(db)
        .await
        .expect("query plugin")
        .expect("plugin row");
    let fetch = plugin::Entity::find()
        .filter(plugin::Column::Name.eq("mcp-fetch"))
        .one(db)
        .await
        .expect("query plugin")
        .expect("plugin row");
    (tenant_row.id, workspace_row.id, bash.id, fetch.id)
}

/// Seed one agent_session row and link it. Returns the PK.
async fn seed_agent_session(
    db: &DatabaseConnection,
    tenant_id: Uuid,
    workspace_id: Uuid,
    agent_session_id: &str,
    state: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now();
    agent_session::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant_id),
        workspace_id: Set(workspace_id),
        agent_session_id: Set(agent_session_id.to_string()),
        state: Set(state.to_string()),
        created_at: Set(now),
        last_active_at: Set(now),
        reactivation_count: Set(0),
    }
    .insert(db)
    .await
    .expect("insert agent_session");
    id
}

/// Seed one session_worker row in the lifecycle shape session-broker
/// produces. `mcp_session_id` empty + `agent_session_id` None mirrors
/// the spawn-to-initialize-response window; pass the populated values
/// for the steady-state shape.
#[allow(clippy::too_many_arguments)]
async fn seed_session_worker(
    db: &DatabaseConnection,
    plugin_id: Uuid,
    agent_session_id: Option<Uuid>,
    container_name: &str,
    container_ip: &str,
    mcp_session_id: &str,
    reaped: bool,
) {
    let id = Uuid::new_v4();
    let now = Utc::now();
    session_worker::ActiveModel {
        id: Set(id),
        agent_session_id: Set(agent_session_id),
        plugin_id: Set(plugin_id),
        container_name: Set(container_name.to_string()),
        container_ip: Set(container_ip.to_string()),
        mcp_session_id: Set(mcp_session_id.to_string()),
        spawned_at: Set(now),
        reaped_at: Set(if reaped { Some(now) } else { None }),
    }
    .insert(db)
    .await
    .expect("insert session_worker");
}

/// Spin postgres, migrate, apply bootstrap. Returns `None` when docker
/// isn't reachable (test prints IGNORED + returns).
async fn setup_db() -> Option<(DatabaseConnection, testcontainers::ContainerAsync<Postgres>)> {
    if !docker_available().await {
        return None;
    }
    let (pg, url) = start_postgres().await.expect("postgres must start");
    let db = connect_with_retry(&url).await.expect("connect to postgres");
    Migrator::up(&db, None)
        .await
        .expect("migrations must apply");

    let raw: BootstrapConfigRaw = serde_yaml::from_str(SAMPLE_YAML).expect("yaml parse");
    let cfg = BootstrapConfig::from_raw(raw).expect("bootstrap validate");
    apply(&db, &cfg).await.expect("bootstrap apply");

    Some((db, pg))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_returns_zero_records_for_empty_session_worker_table() {
    let Some((db, _pg)) = setup_db().await else {
        eprintln!(
            "IGNORED recovery_returns_zero_records_for_empty_session_worker_table: \
             docker not reachable"
        );
        return;
    };
    let store = Arc::new(SessionStore::new());
    let count = run_recovery_with_retries(store.clone(), &db)
        .await
        .expect("recovery succeeds with empty DB");
    assert_eq!(count, 0, "no rows ⇒ no records");
    assert!(store.is_empty().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_projects_active_session_worker_rows_into_store() {
    let Some((db, _pg)) = setup_db().await else {
        eprintln!("IGNORED recovery_projects_active_session_worker_rows_into_store");
        return;
    };
    let (tenant_id, workspace_id, _bash_id, fetch_id) = resolve_ids(&db).await;

    // One bound, post-initialize, live session_worker row for fetch.
    let agent_pk = seed_agent_session(
        &db,
        tenant_id,
        workspace_id,
        "agent-bound-1",
        agent_session::state::ACTIVE,
    )
    .await;
    seed_session_worker(
        &db,
        fetch_id,
        Some(agent_pk),
        "mcp_session_aabbcc",
        "172.20.0.5",
        "mcp_session_aabbcc",
        false,
    )
    .await;

    let store = Arc::new(SessionStore::new());
    let count = run_recovery_with_retries(store.clone(), &db)
        .await
        .expect("recovery ok");
    assert_eq!(count, 1);

    let recovered = store.get("mcp_session_aabbcc").await.expect("present");
    assert_eq!(
        recovered.container_ip,
        "172.20.0.5".parse::<std::net::Ipv4Addr>().unwrap()
    );
    assert_eq!(recovered.tenant, "phlax");
    assert_eq!(recovered.workspace, "mcp");
    assert_eq!(recovered.plugin, "mcp-fetch");
    // Bootstrap normalised egress: the plugin row carries the allow-list
    // shape, recovery passes it through verbatim into egress_policy.
    assert_eq!(
        recovered.egress_policy["allow"][0]["host"], "example.com",
        "egress_policy must round-trip from plugin.egress: {:?}",
        recovered.egress_policy
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_includes_grace_state_excludes_inactive_and_purged() {
    let Some((db, _pg)) = setup_db().await else {
        eprintln!("IGNORED recovery_includes_grace_state_excludes_inactive_and_purged");
        return;
    };
    let (tenant_id, workspace_id, bash_id, fetch_id) = resolve_ids(&db).await;

    // Five sessions across every lifecycle state. The JOIN must
    // surface only ACTIVE + GRACE; the other three are addressable in
    // admin-api but not by envoy (no live container backing them).
    let states = [
        ("agent-active", agent_session::state::ACTIVE, true),
        ("agent-grace", agent_session::state::GRACE, true),
        ("agent-inactive", agent_session::state::INACTIVE, false),
        (
            "agent-teardown",
            agent_session::state::TEARDOWN_REQUESTED,
            false,
        ),
        ("agent-purged", agent_session::state::PURGED, false),
    ];
    for (slug, state, should_recover) in states {
        let agent_pk = seed_agent_session(&db, tenant_id, workspace_id, slug, state).await;
        // Use a stable container_name per slug so the assertion can
        // map back to which session-broker row surfaced or not.
        let container = format!("mcp_session_{slug}");
        seed_session_worker(
            &db,
            // Alternate plugins so the test exercises both bindings.
            if should_recover { fetch_id } else { bash_id },
            Some(agent_pk),
            &container,
            "172.20.0.5",
            &container,
            false,
        )
        .await;
    }

    let store = Arc::new(SessionStore::new());
    let count = run_recovery_with_retries(store.clone(), &db)
        .await
        .expect("recovery ok");
    assert_eq!(count, 2, "only active + grace ⇒ 2 recovered");
    assert!(store.get("mcp_session_agent-active").await.is_some());
    assert!(store.get("mcp_session_agent-grace").await.is_some());
    for absent in ["agent-inactive", "agent-teardown", "agent-purged"] {
        let key = format!("mcp_session_{absent}");
        assert!(
            store.get(&key).await.is_none(),
            "{key} must be excluded from recovery"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_excludes_reaped_pre_bind_and_pre_initialize_rows() {
    let Some((db, _pg)) = setup_db().await else {
        eprintln!("IGNORED recovery_excludes_reaped_pre_bind_and_pre_initialize_rows");
        return;
    };
    let (tenant_id, workspace_id, _bash_id, fetch_id) = resolve_ids(&db).await;

    // Three "won't make it through the JOIN" rows + one positive
    // control so the assertion proves we're not just zero-counting.
    let bound = seed_agent_session(
        &db,
        tenant_id,
        workspace_id,
        "agent-positive",
        agent_session::state::ACTIVE,
    )
    .await;

    // (a) Live + bound + initialized -> recovered (positive control).
    seed_session_worker(
        &db,
        fetch_id,
        Some(bound),
        "mcp_session_positive",
        "172.20.0.5",
        "mcp_session_positive",
        false,
    )
    .await;

    // (b) Reaped row — `reaped_at IS NOT NULL` excludes it.
    seed_session_worker(
        &db,
        fetch_id,
        Some(bound),
        "mcp_session_reaped",
        "172.20.0.6",
        "mcp_session_reaped",
        true,
    )
    .await;

    // (c) Pre-bind row — `agent_session_id IS NULL` excludes it.
    seed_session_worker(
        &db,
        fetch_id,
        None,
        "mcp_session_prebind",
        "172.20.0.7",
        "mcp_session_prebind",
        false,
    )
    .await;

    // (d) Pre-initialize row — `mcp_session_id = ''` excludes it.
    seed_session_worker(
        &db,
        fetch_id,
        Some(bound),
        "mcp_session_preinit",
        "172.20.0.8",
        "",
        false,
    )
    .await;

    let store = Arc::new(SessionStore::new());
    let count = run_recovery_with_retries(store.clone(), &db)
        .await
        .expect("recovery ok");
    assert_eq!(count, 1, "only the positive control survives the JOIN");
    assert!(store.get("mcp_session_positive").await.is_some());
}
