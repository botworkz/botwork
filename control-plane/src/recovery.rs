//! Cold-start recovery sync — reads from postgres.
//!
//! control-plane's `SessionStore` is in-memory; on every restart it
//! starts empty. Without a sync, every restart drops every live
//! session from the (future) xDS feeder's view, and SOTW xDS treats
//! "absent from snapshot" as "removed" — so the egress envoy would
//! tear down all live routes the moment control-plane restarted.
//!
//! Pre-RFE-#105-round-3 we closed that gap by polling session-broker's
//! `GET /control-plane/sessions` admin endpoint at startup and bulk-
//! seeding the store. That coupled control-plane's boot to session-
//! broker's reachability, which is the cycle that took down
//! botspace-01 on 2026-06-21 and motivated the round-3 cutover.
//!
//! With round-3, every fact session-broker used to expose over that
//! endpoint now lives in postgres (`agent_session` keyed on
//! `(tenant_id, workspace_id, agent_session_id)`; `session_worker`
//! keyed on `container_name`; per-plugin `egress` blob on `plugin`).
//! control-plane reads it directly from the same DB every other
//! consumer uses. No more session-broker round trip; no more
//! after-edge in the systemd graph; recovery converges as soon as
//! postgres is up.
//!
//! ## Wire mapping
//!
//! The JOIN we run mirrors the projection session-broker's old
//! `/control-plane/sessions` endpoint did over `transport_sessions`,
//! one row per fully-formed live session:
//!
//! ```sql
//! SELECT
//!     sw.mcp_session_id  AS session_id,
//!     sw.container_ip    AS container_ip,
//!     t.name             AS tenant,
//!     w.name             AS workspace,
//!     p.name             AS plugin,
//!     p.egress           AS egress_policy
//! FROM   session_worker sw
//! JOIN   agent_session  a  ON sw.agent_session_id = a.id
//! JOIN   workspace      w  ON a.workspace_id      = w.id
//! JOIN   tenant         t  ON a.tenant_id         = t.id
//! JOIN   plugin         p  ON sw.plugin_id        = p.id
//! WHERE  sw.reaped_at IS NULL                    -- live container
//!   AND  sw.agent_session_id IS NOT NULL         -- past first-bind
//!   AND  sw.mcp_session_id  <> ''                -- past initialize
//!   AND  a.state IN ('active', 'grace')          -- live or grace
//! ```
//!
//! The four "alive" gates are deliberately conservative:
//!
//! * `reaped_at IS NULL` — the row is for a container session-broker
//!   still believes is live. Reaped rows are audit-only; xDS pushing
//!   a policy for an absent container would silently 5xx on first
//!   request (no upstream).
//! * `agent_session_id IS NOT NULL` — the goose agent has bound
//!   (which means session-broker has populated the FK). Pre-bind
//!   workers carry the spawn-time INSERT but have no
//!   `(tenant, workspace)` linkage we can resolve via the JOIN.
//! * `mcp_session_id != ''` — the upstream's `initialize` response
//!   has landed. The empty string default exists for the
//!   spawn-to-initialize-response window; routing a session whose
//!   mcp-session-id we don't know yet would 404 at envoy.
//! * `a.state IN ('active', 'grace')` — `inactive`, `teardown_requested`,
//!   `purged` rows are not addressable by an inbound request; their
//!   container has been torn down (or is about to be) and the row
//!   only exists for audit/janitor purposes.
//!
//! The first three predicates are session-broker's writer contract
//! made queryable; the fourth is what makes this the DB-side
//! equivalent of "walk transport_sessions" — transport_sessions only
//! ever held active/grace rows because session-broker reaped its
//! own map on teardown.
//!
//! ## Failure semantics
//!
//! Three cases:
//!
//! * empty result set — legitimate cold start with no live sessions.
//!   Recovery proceeds; `SessionStore` stays empty; control-plane
//!   binds and starts serving. Fresh-deploy boot must work.
//! * N rows — recovered N sessions; store populated; control-plane
//!   binds.
//! * `sea_orm::DbErr` from the SELECT — we *don't know* the live
//!   state, and starting with the wrong view would silently break
//!   the xDS feeder. The retry loop tries [`MAX_ATTEMPTS`] times
//!   with linear backoff before giving up; the binary then exits 1
//!   and systemd's `Restart=always` keeps retrying from scratch.
//!
//! The retry loop is preserved (vs. failing on first error) because
//! postgres can legitimately be in a transient state at boot — the
//! `botwork-postgres.service` unit's container can take a few
//! seconds to come up, and we don't want a 1-second `pg_isready`
//! window to make control-plane exit and re-enter the systemd
//! restart loop.
//!
//! ## Sequencing
//!
//! Recovery requires postgres to be reachable. The supported systemd
//! ordering puts `botwork-control-plane.service` AFTER
//! `botwork-db-migrate.service` (transitively `After=`
//! `botwork-postgres.service` + `botwork-db-init.service`). Same
//! posture config-broker, api, and session-broker use.
//!
//! Crucially the `After=botwork-session-broker.service` edge from
//! the pre-round-3 unit can now be **dropped**: control-plane no
//! longer needs session-broker reachable to make recovery progress.
//! The companion vm PR drops that edge along with the
//! `control_plane_recovery_sync_completed` goss probe (which greps
//! a log line that no longer fires).

use std::sync::Arc;
use std::time::Duration;

use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, JoinType, QueryFilter, QuerySelect, RelationTrait,
};
use serde::Deserialize;
use thiserror::Error;
use tokio::time::sleep;
use tracing::{info, warn};

use botwork_entity::{agent_session, plugin, session_worker, tenant, workspace};

use crate::sessions::{SessionRecord, SessionStore};

const PREFIX: &str = "[control-plane:recovery]";

/// How many fetch attempts before recovery gives up and the binary
/// exits non-zero. 30 × ~5s spacing = ~150s headroom for a slow
/// postgres cold start; longer than that and systemd's restart loop
/// is the right place to retry, not us.
pub const MAX_ATTEMPTS: u32 = 30;

/// Pause between attempts. Linear (not exponential): the dominant
/// failure mode at boot is "postgres hasn't bound yet," which
/// resolves in seconds; we don't want to back off into minutes for a
/// transient issue, and we'd rather give up and let systemd retry
/// than block startup indefinitely.
pub const ATTEMPT_INTERVAL: Duration = Duration::from_secs(5);

/// All ways the DB recovery path can surface a failure. Wrapped so
/// the binary's startup path can pattern-match if it ever wants
/// finer-grained handling (today it just logs the message and
/// exits 1).
#[derive(Debug, Error)]
pub enum RecoveryError {
    /// The JOIN itself failed — DB unreachable, schema drift, etc.
    /// Always retried until [`MAX_ATTEMPTS`] is exhausted; control-
    /// plane refuses to bind if recovery never succeeds.
    #[error("db error during recovery JOIN: {0}")]
    Db(#[from] sea_orm::DbErr),
    /// A recovered row was structurally wrong (the canonical example
    /// is a non-IPv4 string in `session_worker.container_ip`).
    /// session-broker validates this on the way in, so production
    /// rows can't trip it; the variant exists so a future schema
    /// change has somewhere clean to surface.
    #[error("recovered row failed validation: {0}")]
    BadRow(String),
}

/// Run the recovery loop. Returns once the store has been seeded
/// (possibly with zero records); errors out only after
/// [`MAX_ATTEMPTS`] failures.
///
/// `store` is the same `Arc<SessionStore>` `AppState` holds; we mutate
/// through that handle so the HTTP server (which `main.rs` starts
/// immediately after this returns) sees the seeded state.
pub async fn run_with_retries(
    store: Arc<SessionStore>,
    db: &DatabaseConnection,
) -> Result<usize, RecoveryError> {
    run_with_retries_config(store, db, MAX_ATTEMPTS, ATTEMPT_INTERVAL).await
}

/// Test-visible variant with knobs exposed. Keeps `run_with_retries`
/// a one-line caller so the call site is short and the defaults are
/// the single source of production behaviour.
pub async fn run_with_retries_config(
    store: Arc<SessionStore>,
    db: &DatabaseConnection,
    max_attempts: u32,
    interval: Duration,
) -> Result<usize, RecoveryError> {
    info!("{PREFIX} starting DB recovery sync (max_attempts={max_attempts})");

    let mut last_err: Option<RecoveryError> = None;
    for attempt in 1..=max_attempts {
        match fetch_live_sessions(db).await {
            Ok(records) => {
                let count = seed_store(&store, records).await;
                info!(
                    "{PREFIX} recovered {count} session(s) from DB on attempt {attempt}/{max_attempts}"
                );
                return Ok(count);
            }
            Err(err) => {
                warn!(
                    "{PREFIX} attempt {attempt}/{max_attempts} failed: {err}; retrying in {:?}",
                    interval
                );
                last_err = Some(err);
                if attempt < max_attempts {
                    sleep(interval).await;
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        RecoveryError::Db(sea_orm::DbErr::Custom(
            "recovery exhausted with no recorded error".to_string(),
        ))
    }))
}

/// Pull the rows the JOIN above describes and project each one into a
/// [`SessionRecord`]. Public for tests; the production path goes
/// through [`run_with_retries`] which adds the retry/backoff envelope.
pub async fn fetch_live_sessions(
    db: &DatabaseConnection,
) -> Result<Vec<SessionRecord>, RecoveryError> {
    // Project the JOIN through SeaORM's `select_only().column(...)` so
    // the result type is a small, deserialise-friendly struct rather
    // than five entity models we'd then have to thread together.
    // `FromQueryResult` decodes column-by-column off whatever the
    // driver returns.
    //
    // The JOIN order goes session_worker → agent_session → workspace →
    // tenant (left-to-right by FK direction), with a sibling JOIN to
    // plugin off session_worker. The four WHERE predicates are spelled
    // out in the module docs.
    let rows: Vec<SessionRow> = session_worker::Entity::find()
        .select_only()
        .column_as(session_worker::Column::McpSessionId, "session_id")
        .column_as(session_worker::Column::ContainerIp, "container_ip")
        .column_as(tenant::Column::Name, "tenant")
        .column_as(workspace::Column::Name, "workspace")
        .column_as(plugin::Column::Name, "plugin")
        .column_as(plugin::Column::Egress, "egress_policy")
        .join(
            JoinType::InnerJoin,
            session_worker::Relation::AgentSession.def(),
        )
        .join(
            JoinType::InnerJoin,
            agent_session::Relation::Workspace.def(),
        )
        .join(JoinType::InnerJoin, agent_session::Relation::Tenant.def())
        .join(JoinType::InnerJoin, session_worker::Relation::Plugin.def())
        .filter(session_worker::Column::ReapedAt.is_null())
        .filter(session_worker::Column::AgentSessionId.is_not_null())
        .filter(session_worker::Column::McpSessionId.ne(""))
        .filter(agent_session::Column::State.is_in(vec![
            agent_session::state::ACTIVE,
            agent_session::state::GRACE,
        ]))
        .into_model::<SessionRow>()
        .all(db)
        .await?;

    let mut records = Vec::with_capacity(rows.len());
    for row in rows {
        records.push(row.into_session_record()?);
    }
    Ok(records)
}

/// Bulk-insert recovered records into the store.
///
/// Uses `SessionStore::insert` — the strict, ergonomic-per-spawn
/// path. In recovery we *expect* the store to be empty, so duplicate
/// errors here would be a bug (probably someone calling
/// `run_with_retries` twice on the same store, or seeding while the
/// HTTP server is already accepting POSTs). We log + skip dupes
/// rather than abort so a transient "control-plane started early"
/// double-call doesn't bring down the binary.
async fn seed_store(store: &SessionStore, records: Vec<SessionRecord>) -> usize {
    let mut inserted = 0usize;
    for record in records {
        let id = record.session_id.clone();
        match store.insert(record).await {
            Ok(()) => inserted += 1,
            Err(err) => {
                warn!("{PREFIX} skipping recovered record {id}: {err}");
            }
        }
    }
    inserted
}

/// Untyped projection of the JOIN above. `egress_policy` is `jsonb`
/// in postgres which sqlx decodes as [`serde_json::Value`]; everything
/// else is `text`. `container_ip` is parsed into `Ipv4Addr` in
/// [`SessionRow::into_session_record`] so a bad row surfaces as a
/// distinct error variant rather than blowing up at insert time.
#[derive(Debug, sea_orm::FromQueryResult, Deserialize)]
struct SessionRow {
    session_id: String,
    container_ip: String,
    tenant: String,
    workspace: String,
    plugin: String,
    egress_policy: serde_json::Value,
}

impl SessionRow {
    fn into_session_record(self) -> Result<SessionRecord, RecoveryError> {
        let container_ip = self.container_ip.parse().map_err(|_| {
            RecoveryError::BadRow(format!(
                "session_worker.container_ip {:?} is not IPv4 for mcp_session_id {:?}",
                self.container_ip, self.session_id
            ))
        })?;
        Ok(SessionRecord {
            session_id: self.session_id,
            container_ip,
            tenant: self.tenant,
            workspace: self.workspace,
            plugin: self.plugin,
            egress_policy: self.egress_policy,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::SessionRecord;
    use sea_orm::{DatabaseBackend, MockDatabase};

    /// Construct a SessionRow with deliberately-fixed defaults so the
    /// projection tests are tightly readable. Mirrors the row shape
    /// the production JOIN returns; tests assert the
    /// `into_session_record` projection.
    fn row(id: &str, ip: &str, plugin: &str) -> SessionRow {
        SessionRow {
            session_id: id.to_string(),
            container_ip: ip.to_string(),
            tenant: "phlax".to_string(),
            workspace: "mcp".to_string(),
            plugin: plugin.to_string(),
            egress_policy: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn session_row_projects_into_session_record() {
        let row = row("mcp_session_a", "172.20.0.5", "fetch");
        let record = row.into_session_record().expect("good row");
        assert_eq!(record.session_id, "mcp_session_a");
        assert_eq!(
            record.container_ip,
            "172.20.0.5".parse::<std::net::Ipv4Addr>().unwrap()
        );
        assert_eq!(record.plugin, "fetch");
        assert!(record.egress_policy.is_object());
    }

    #[tokio::test]
    async fn session_row_with_bad_ip_returns_bad_row() {
        let row = row("mcp_session_a", "not-an-ip", "fetch");
        let err = row.into_session_record().expect_err("must error");
        match err {
            RecoveryError::BadRow(msg) => {
                assert!(msg.contains("not-an-ip"), "{msg}");
                assert!(msg.contains("mcp_session_a"), "{msg}");
            }
            other => panic!("expected BadRow, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn seed_store_skips_duplicates_without_aborting() {
        // Recovery is supposed to run against an empty store, but if a
        // record collides (e.g. someone seeded externally) we log and
        // continue rather than blow up.
        let store = SessionStore::new();
        let pre = SessionRecord {
            session_id: "mcp_session_a".to_string(),
            container_ip: "172.20.0.5".parse().unwrap(),
            tenant: "phlax".to_string(),
            workspace: "mcp".to_string(),
            plugin: "fetch".to_string(),
            egress_policy: serde_json::Value::Null,
        };
        store.insert(pre).await.expect("pre-insert");

        let records = vec![
            SessionRecord {
                session_id: "mcp_session_a".to_string(),
                container_ip: "172.20.0.5".parse().unwrap(),
                tenant: "phlax".to_string(),
                workspace: "mcp".to_string(),
                plugin: "fetch".to_string(),
                egress_policy: serde_json::Value::Null,
            },
            SessionRecord {
                session_id: "mcp_session_b".to_string(),
                container_ip: "172.20.0.6".parse().unwrap(),
                tenant: "phlax".to_string(),
                workspace: "mcp".to_string(),
                plugin: "git".to_string(),
                egress_policy: serde_json::Value::Null,
            },
        ];
        let inserted = seed_store(&store, records).await;
        assert_eq!(inserted, 1);
        assert_eq!(store.len().await, 2);
    }

    // ── fetch_live_sessions / run_with_retries_config (MockDatabase) ─────────

    /// Helper: build a MockDatabase connection backed by Postgres dialect.
    fn mock_db(mock: MockDatabase) -> sea_orm::DatabaseConnection {
        mock.into_connection()
    }

    #[tokio::test]
    async fn fetch_live_sessions_empty_result() {
        // No live rows → Ok([]).
        use botwork_entity::session_worker;
        let db = mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([Vec::<session_worker::Model>::new()]),
        );
        let records = fetch_live_sessions(&db).await.expect("should succeed");
        assert!(records.is_empty());
    }

    #[tokio::test]
    async fn fetch_live_sessions_db_error() {
        // DB returns an error → RecoveryError::Db.
        let db = mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_errors([sea_orm::DbErr::Custom("db gone".to_string())]),
        );
        let err = fetch_live_sessions(&db).await.expect_err("should fail");
        match err {
            RecoveryError::Db(inner) => {
                assert!(inner.to_string().contains("db gone"), "{inner}");
            }
            other => panic!("expected Db variant, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_live_sessions_nonempty_row_projects_ok() {
        // Single live row with the right column aliases → Ok([SessionRecord]).
        use std::collections::BTreeMap;
        let mut mock_row: BTreeMap<String, sea_orm::Value> = BTreeMap::new();
        mock_row.insert(
            "session_id".to_string(),
            sea_orm::Value::String(Some(Box::new("mcp_session_abc".to_string()))),
        );
        mock_row.insert(
            "container_ip".to_string(),
            sea_orm::Value::String(Some(Box::new("10.0.0.7".to_string()))),
        );
        mock_row.insert(
            "tenant".to_string(),
            sea_orm::Value::String(Some(Box::new("acme".to_string()))),
        );
        mock_row.insert(
            "workspace".to_string(),
            sea_orm::Value::String(Some(Box::new("mcp".to_string()))),
        );
        mock_row.insert(
            "plugin".to_string(),
            sea_orm::Value::String(Some(Box::new("fetch".to_string()))),
        );
        mock_row.insert(
            "egress_policy".to_string(),
            sea_orm::Value::Json(Some(Box::new(serde_json::json!({})))),
        );

        let db = mock_db(
            MockDatabase::new(DatabaseBackend::Postgres).append_query_results([vec![mock_row]]),
        );
        let records = fetch_live_sessions(&db).await.expect("should succeed");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].session_id, "mcp_session_abc");
        assert_eq!(
            records[0].container_ip,
            "10.0.0.7".parse::<std::net::Ipv4Addr>().unwrap()
        );
        assert_eq!(records[0].tenant, "acme");
        assert_eq!(records[0].plugin, "fetch");
    }

    #[tokio::test]
    async fn fetch_live_sessions_row_with_bad_ip_returns_bad_row() {
        // A row with an un-parseable IP → RecoveryError::BadRow.
        use std::collections::BTreeMap;
        let mut mock_row: BTreeMap<String, sea_orm::Value> = BTreeMap::new();
        mock_row.insert(
            "session_id".to_string(),
            sea_orm::Value::String(Some(Box::new("mcp_session_bad".to_string()))),
        );
        mock_row.insert(
            "container_ip".to_string(),
            sea_orm::Value::String(Some(Box::new("not-an-ip".to_string()))),
        );
        mock_row.insert(
            "tenant".to_string(),
            sea_orm::Value::String(Some(Box::new("acme".to_string()))),
        );
        mock_row.insert(
            "workspace".to_string(),
            sea_orm::Value::String(Some(Box::new("mcp".to_string()))),
        );
        mock_row.insert(
            "plugin".to_string(),
            sea_orm::Value::String(Some(Box::new("fetch".to_string()))),
        );
        mock_row.insert(
            "egress_policy".to_string(),
            sea_orm::Value::Json(Some(Box::new(serde_json::json!({})))),
        );

        let db = mock_db(
            MockDatabase::new(DatabaseBackend::Postgres).append_query_results([vec![mock_row]]),
        );
        let err = fetch_live_sessions(&db)
            .await
            .expect_err("bad ip should fail");
        match err {
            RecoveryError::BadRow(msg) => {
                assert!(msg.contains("not-an-ip"), "{msg}");
            }
            other => panic!("expected BadRow, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_with_retries_config_succeeds_on_first_attempt() {
        // First DB call returns empty result → success, count=0, no retries.
        use botwork_entity::session_worker;
        let db = mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([Vec::<session_worker::Model>::new()]),
        );
        let store = Arc::new(SessionStore::new());
        let count = run_with_retries_config(store, &db, 3, Duration::ZERO)
            .await
            .expect("success on first attempt");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn run_with_retries_config_error_then_success() {
        // First attempt errors; second attempt returns empty set → Ok(0).
        use botwork_entity::session_worker;
        let db = mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_errors([sea_orm::DbErr::Custom("transient".to_string())])
                .append_query_results([Vec::<session_worker::Model>::new()]),
        );
        let store = Arc::new(SessionStore::new());
        let count = run_with_retries_config(store, &db, 2, Duration::ZERO)
            .await
            .expect("second attempt succeeds");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn run_with_retries_config_exhausted_returns_err() {
        // All attempts fail → Err after max_attempts=1 (no sleep needed).
        let db = mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_errors([sea_orm::DbErr::Custom("always-fail".to_string())]),
        );
        let store = Arc::new(SessionStore::new());
        let err = run_with_retries_config(store, &db, 1, Duration::ZERO)
            .await
            .expect_err("should exhaust attempts");
        match err {
            RecoveryError::Db(inner) => {
                assert!(inner.to_string().contains("always-fail"), "{inner}");
            }
            other => panic!("expected Db, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_with_retries_delegates_to_config() {
        // The public wrapper uses default constants; just confirm it returns
        // Ok(0) on a clean empty-result mock without blocking.
        use botwork_entity::session_worker;
        let db = mock_db(
            MockDatabase::new(DatabaseBackend::Postgres)
                .append_query_results([Vec::<session_worker::Model>::new()]),
        );
        let store = Arc::new(SessionStore::new());
        // run_with_retries uses MAX_ATTEMPTS=30 and ATTEMPT_INTERVAL=5s, but
        // since the first attempt succeeds the interval is never used.
        let count = run_with_retries(store, &db).await.expect("delegates ok");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn run_with_retries_config_seeded_record_counts() {
        // Non-empty successful result → count reflects inserted sessions.
        use std::collections::BTreeMap;
        let make_row = |session_id: &str, ip: &str| -> BTreeMap<String, sea_orm::Value> {
            let mut r = BTreeMap::new();
            r.insert(
                "session_id".to_string(),
                sea_orm::Value::String(Some(Box::new(session_id.to_string()))),
            );
            r.insert(
                "container_ip".to_string(),
                sea_orm::Value::String(Some(Box::new(ip.to_string()))),
            );
            r.insert(
                "tenant".to_string(),
                sea_orm::Value::String(Some(Box::new("acme".to_string()))),
            );
            r.insert(
                "workspace".to_string(),
                sea_orm::Value::String(Some(Box::new("mcp".to_string()))),
            );
            r.insert(
                "plugin".to_string(),
                sea_orm::Value::String(Some(Box::new("fetch".to_string()))),
            );
            r.insert(
                "egress_policy".to_string(),
                sea_orm::Value::Json(Some(Box::new(serde_json::json!({})))),
            );
            r
        };
        let db = mock_db(
            MockDatabase::new(DatabaseBackend::Postgres).append_query_results([vec![
                make_row("mcp_session_1", "10.0.0.1"),
                make_row("mcp_session_2", "10.0.0.2"),
            ]]),
        );
        let store = Arc::new(SessionStore::new());
        let count = run_with_retries_config(store.clone(), &db, 1, Duration::ZERO)
            .await
            .expect("success");
        assert_eq!(count, 2);
        assert_eq!(store.len().await, 2);
    }
}
