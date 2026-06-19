//! In-memory store of `SessionRecord` keyed by session id.
//!
//! Owned by `AppState` and shared (via `Arc<SessionStore>`) across the axum
//! handlers and the xDS task. Concurrency is a single
//! `tokio::sync::RwLock<HashMap<…>>`: v0 traffic is one event per
//! session-spawn/teardown, so write contention is irrelevant and read
//! traffic (the xDS subscriber, future health endpoints) overwhelmingly
//! outweighs writes.
//!
//! Identity and ordering rules:
//!
//! * `session_id` is the unique key. Inserting a record for an id that
//!   already exists is rejected as `AlreadyExists` — the wire path
//!   distinguishes "POST same record twice" (an idempotent retry by
//!   session-broker, which we may want to allow later) from "two
//!   different containers claiming the same id" (a real bug). v0 takes
//!   the strict reading and forces retries to delete-then-insert; the
//!   stricter rule is easier to relax than tighten.
//! * Deleting an unknown id is `NotFound`. Same reasoning as above — we
//!   want a control-plane / session-broker desync to surface loudly, not
//!   pass as a 200 no-op.
//! * Snapshot reads (`list`, `get`) clone the data out. Internal locks
//!   are never held across an `await` to a subscriber — those wake on
//!   a `tokio::sync::watch` channel and re-read.
//!
//! ## Generation channel + ack channel (load-bearing)
//!
//! Two coupled monotonic counters underpin the synchronous-ack hard
//! gate:
//!
//! * `generation_tx: watch::Sender<u64>` — bumped after every successful
//!   `insert` / `remove` / `bulk_seed`. The xDS server subscribes to
//!   it via [`subscribe_generation`] and uses each tick as "re-snapshot
//!   the store, recompile, push fresh LDS to envoy."
//!
//! * `acked_version_tx: watch::Sender<u64>` — published by the xDS
//!   server (and *only* by the xDS server) every time envoy returns a
//!   clean ACK for a Listener push. The HTTP handlers
//!   ([`crate::handler::post_session`], `delete_session`) subscribe to
//!   it via [`subscribe_acked_version`] and block until
//!   `acked >= store_generation_at_mutation_time`, with a per-request
//!   timeout.
//!
//! This is the closure of the cold-start window: session-broker's
//! `POST /sessions` does not return 201 until the egress envoy has
//! actually applied the Listener that carries the new RBAC policy. If
//! envoy is disconnected or slow, the handler 503s after the timeout
//! and session-broker tears the container down. The contract a
//! session-broker caller can rely on is "a 201 means the policy is
//! live in envoy"; nothing weaker would close the race where a freshly
//! spawned plugin's first tool call 403s because xDS hadn't caught up.
//!
//! Why one counter for the data (`generation`) and one for the ack
//! (`acked_version`) rather than reusing one: subscribers to
//! `generation` need a monotonic edge that *something changed*; the
//! ack counter is the value envoy reported and may legitimately lag
//! the data counter. Coupling them in one channel would conflate
//! "store changed" and "envoy confirmed" — which are the two events
//! the gate needs to distinguish.
//!
//! The HTTP handlers also need to know when xDS is *unable* to make
//! progress (no envoy attached). [`xds_subscriber_count`] returns the
//! number of currently-open ADS streams; a value of 0 means a
//! `wait_for_ack` would block to the timeout for no good reason, so
//! handlers shortcut to 503 immediately. The counter is maintained by
//! [`xds_subscriber_guard`], which yields an RAII guard the xDS task
//! holds for the lifetime of each stream.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{watch, RwLock};
use tokio::time::timeout;

/// One running plugin session as seen by control-plane.
///
/// Wire-compatible with both the `POST /sessions` request body and the
/// `GET /sessions[/<id>]` response body. The single struct keeps the
/// schema honest: anything session-broker can post, control-plane can
/// echo back to a recovery-syncing peer (and vice versa).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// `mcp_session_<token>` — the docker container name session-broker
    /// chose at spawn time. Stable for the lifetime of the session.
    pub session_id: String,
    /// IPv4 address the spawned container holds on the plugin docker
    /// network (`botwork-plugin` per botworkz/vm#84 PR1). v0 enforces
    /// IPv4 only because that's what the broker stack assumes
    /// elsewhere; if dual-stack ever lands the schema bumps then.
    pub container_ip: Ipv4Addr,
    /// `<tenant>` segment from the request URL grammar
    /// `/<tenant>/<namespace>/<plugin>`. Shape-validated, not yet keyed
    /// on for policy resolution.
    pub tenant: String,
    /// `<namespace>` segment from the same URL grammar.
    pub namespace: String,
    /// `<plugin>` segment from the same URL grammar. Joined with the
    /// `egress_policy` below to give envoy "this src IP is this plugin
    /// and this is its policy."
    pub plugin: String,
    /// Verbatim `egress:` block from the plugin's descriptor as
    /// returned by config-broker's `/resolve`. v0 control-plane does
    /// not parse this — it stores it as opaque JSON so the schema can
    /// evolve in config-broker independently. A future xDS materialiser
    /// is what turns it into envoy RBAC / route config.
    pub egress_policy: serde_json::Value,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StoreError {
    #[error("session already exists: {0}")]
    AlreadyExists(String),
    #[error("session not found: {0}")]
    NotFound(String),
}

/// Why a wait-for-ack call returned without success. The HTTP handler
/// maps these to wire-level error codes — see [`crate::handler`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AckWaitError {
    /// No xDS stream is currently connected to control-plane. Waiting
    /// for an ack would block to the timeout for no good reason, so
    /// the handler short-circuits to a 503 immediately. This is the
    /// expected state during the boot window before the egress envoy
    /// connects, or whenever the envoy has restarted and not yet
    /// reconnected.
    #[error("no xDS subscriber attached")]
    NoSubscriber,
    /// The xDS subscriber is attached, but the ack didn't arrive
    /// within the configured per-request timeout. envoy is alive but
    /// not making progress — possibly mid-config-load, possibly
    /// rejecting our pushes via NACK. Either way the handler 503s and
    /// the caller is expected to tear the spawn down.
    #[error("timed out waiting for xDS ack of version {0}")]
    Timeout(u64),
}

pub struct SessionStore {
    inner: RwLock<HashMap<String, SessionRecord>>,
    /// Monotonic mutation counter. Bumped after every successful
    /// `insert` / `remove` / `bulk_seed`. xDS subscribers wake on
    /// this; the *value* of the counter is also forwarded verbatim
    /// to envoy as `version_info`, so a 1:1 mapping between this
    /// counter and the version envoy ACKs is what lets
    /// [`wait_for_ack`] block on the right thing.
    generation_tx: watch::Sender<u64>,
    /// Latest Listener `version_info` envoy has clean-ACKed. Bumped
    /// only by the xDS task in `xds::stream_aggregated_resources`,
    /// monotonic by construction (envoy can NACK an older version
    /// after ACKing a newer one, but our handler only ever asks "have
    /// you acked >= X" so non-monotonic NACKs don't move the gauge
    /// backwards).
    acked_version_tx: watch::Sender<u64>,
    /// Number of currently-open ADS streams. Maintained by
    /// [`xds_subscriber_guard`]: ++ on `new`, -- on `Drop`. The
    /// `Arc<AtomicUsize>` is shared with the guard so the count is
    /// accurate even if the task panics.
    xds_subscribers: Arc<AtomicUsize>,
}

impl Default for SessionStore {
    fn default() -> Self {
        let (generation_tx, _) = watch::channel(0u64);
        let (acked_version_tx, _) = watch::channel(0u64);
        Self {
            inner: RwLock::new(HashMap::new()),
            generation_tx,
            acked_version_tx,
            xds_subscribers: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a fresh `watch::Receiver` for the generation channel. Each
    /// subscriber gets its own; receivers never miss the *latest* value,
    /// only intermediate ones (which is exactly what xDS wants — see
    /// module docs).
    pub fn subscribe_generation(&self) -> watch::Receiver<u64> {
        self.generation_tx.subscribe()
    }

    /// Read the current generation without subscribing. Used by tests
    /// asserting that mutations bumped the counter, and by the HTTP
    /// handler immediately after a mutation to capture the generation
    /// to wait for.
    pub fn current_generation(&self) -> u64 {
        *self.generation_tx.borrow()
    }

    /// Subscribe to the "latest version envoy has ACKed" channel. Used
    /// by [`wait_for_ack`]; not for direct consumption.
    pub fn subscribe_acked_version(&self) -> watch::Receiver<u64> {
        self.acked_version_tx.subscribe()
    }

    /// Read the current acked version without subscribing. Used by
    /// tests and by [`wait_for_ack`]'s first-check fast path.
    pub fn current_acked_version(&self) -> u64 {
        *self.acked_version_tx.borrow()
    }

    /// Called by the xDS task on every clean ACK. The bump is
    /// monotonic: a later ACK with a lower version (which envoy can
    /// emit if it re-ACKs an older config after a NACK) does not move
    /// the gauge backwards. The HTTP gate only ever asks "have you
    /// acked >= X", so a stale lower value would falsely unblock
    /// waiters expecting the higher one.
    pub fn record_acked_version(&self, version: u64) {
        self.acked_version_tx.send_if_modified(|current| {
            if version > *current {
                *current = version;
                true
            } else {
                false
            }
        });
    }

    /// Snapshot of how many ADS streams are currently connected.
    /// `0` means [`wait_for_ack`] should short-circuit to
    /// `Err(NoSubscriber)` — there is no envoy to ACK us.
    pub fn xds_subscriber_count(&self) -> usize {
        self.xds_subscribers.load(Ordering::Acquire)
    }

    /// Build an RAII guard the xDS task holds for the duration of one
    /// connected stream. Construction increments
    /// [`xds_subscriber_count`]; `Drop` decrements it. Even if the
    /// task panics, the count stays accurate.
    pub fn xds_subscriber_guard(&self) -> XdsSubscriberGuard {
        XdsSubscriberGuard::new(self.xds_subscribers.clone())
    }

    /// Block (with timeout) until envoy has ACKed a Listener with
    /// `version_info >= target_version`.
    ///
    /// Returns:
    ///
    /// * `Ok(())` — ack received in time.
    /// * `Err(NoSubscriber)` — no ADS stream is attached. Short-circuits
    ///   without waiting; the caller is expected to 503.
    /// * `Err(Timeout(target))` — ack didn't arrive within the configured
    ///   timeout. The caller is expected to 503 and tear the session
    ///   down (session-broker's hard-gate posture).
    ///
    /// Implementation note: we re-check the subscriber count after
    /// `changed()` to handle the case where envoy disconnected mid-wait.
    /// Without that, the wait would block to the timeout even after
    /// it becomes hopeless.
    pub async fn wait_for_ack(
        &self,
        target_version: u64,
        wait: Duration,
    ) -> Result<(), AckWaitError> {
        if self.xds_subscriber_count() == 0 {
            return Err(AckWaitError::NoSubscriber);
        }
        let mut rx = self.acked_version_tx.subscribe();
        // Fast path: already acked. Cheap to check before arming the
        // timeout.
        if *rx.borrow_and_update() >= target_version {
            return Ok(());
        }
        let outcome = timeout(wait, async {
            loop {
                if rx.changed().await.is_err() {
                    // The store was dropped: the binary is shutting
                    // down. Treat as a no-subscriber outcome.
                    return Err(AckWaitError::NoSubscriber);
                }
                if *rx.borrow_and_update() >= target_version {
                    return Ok(());
                }
                if self.xds_subscriber_count() == 0 {
                    // envoy went away mid-wait — fail fast instead of
                    // blocking to the timeout.
                    return Err(AckWaitError::NoSubscriber);
                }
            }
        })
        .await;
        match outcome {
            Ok(result) => result,
            Err(_) => Err(AckWaitError::Timeout(target_version)),
        }
    }

    fn bump(&self) {
        // `send_modify` always wakes subscribers regardless of whether
        // the value differs — perfect for a counter we always increment.
        self.generation_tx.send_modify(|g| *g = g.wrapping_add(1));
    }

    /// Strict insert: rejects a second insert for the same `session_id`.
    /// See module docs for the rationale.
    pub async fn insert(&self, record: SessionRecord) -> Result<(), StoreError> {
        let mut guard = self.inner.write().await;
        if guard.contains_key(&record.session_id) {
            return Err(StoreError::AlreadyExists(record.session_id));
        }
        guard.insert(record.session_id.clone(), record);
        drop(guard);
        self.bump();
        Ok(())
    }

    /// Strict delete: rejects when `session_id` was never inserted (or
    /// was already removed). Forces session-broker and control-plane to
    /// disagree loudly rather than quietly drift.
    pub async fn remove(&self, session_id: &str) -> Result<SessionRecord, StoreError> {
        let mut guard = self.inner.write().await;
        let removed = guard
            .remove(session_id)
            .ok_or_else(|| StoreError::NotFound(session_id.to_string()))?;
        drop(guard);
        self.bump();
        Ok(removed)
    }

    /// Bulk-load N records under a single write lock. Used by cold-start
    /// recovery to seed the empty store from session-broker's snapshot
    /// without serialising N individual `insert()` awaits. Bumps the
    /// generation exactly once at the end so xDS subscribers see one
    /// "store seeded" push, not N back-to-back churns.
    ///
    /// Treats duplicates inside `records` as `AlreadyExists`; bails on
    /// the first dup and the store is left in a partial state. Recovery
    /// is the only caller and is itself a fail-then-restart loop, so
    /// surfacing rather than silently dropping is right.
    pub async fn bulk_seed(&self, records: Vec<SessionRecord>) -> Result<usize, StoreError> {
        let mut guard = self.inner.write().await;
        let mut count = 0;
        for record in records {
            if guard.contains_key(&record.session_id) {
                return Err(StoreError::AlreadyExists(record.session_id));
            }
            guard.insert(record.session_id.clone(), record);
            count += 1;
        }
        drop(guard);
        self.bump();
        Ok(count)
    }

    /// Snapshot read of a single record. `None` for unknown ids; the
    /// handler maps that to 404. We deliberately do not surface
    /// `StoreError` here because callers can express "absent" with the
    /// `Option` directly.
    pub async fn get(&self, session_id: &str) -> Option<SessionRecord> {
        let guard = self.inner.read().await;
        guard.get(session_id).cloned()
    }

    /// Snapshot read of all records. Sorted by `session_id` for stable
    /// output — important for the recovery-sync consumer (control-plane
    /// restart → polls session-broker, then compares snapshots) and for
    /// human ops eyeballing `curl /sessions`.
    pub async fn list(&self) -> Vec<SessionRecord> {
        let guard = self.inner.read().await;
        let mut out: Vec<SessionRecord> = guard.values().cloned().collect();
        out.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        out
    }

    pub async fn len(&self) -> usize {
        let guard = self.inner.read().await;
        guard.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

/// RAII guard the xDS task holds for the lifetime of one connected
/// stream. Increments the shared subscriber counter on construction;
/// decrements on `Drop`. The guard is `Send` so it lives inside the
/// per-stream tokio task; if the task panics, `Drop` still runs and
/// the count stays right.
///
/// Constructed via [`SessionStore::xds_subscriber_guard`].
pub struct XdsSubscriberGuard {
    counter: Arc<AtomicUsize>,
}

impl XdsSubscriberGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self { counter }
    }
}

impl Drop for XdsSubscriberGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, ip: &str, plugin: &str) -> SessionRecord {
        SessionRecord {
            session_id: id.to_string(),
            container_ip: ip.parse().expect("test ip"),
            tenant: "phlax".to_string(),
            namespace: "mcp".to_string(),
            plugin: plugin.to_string(),
            egress_policy: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn insert_then_get_returns_the_record() {
        let store = SessionStore::new();
        store
            .insert(record("mcp_session_abc", "172.20.0.5", "fetch"))
            .await
            .expect("insert");
        let got = store.get("mcp_session_abc").await.expect("present");
        assert_eq!(got.container_ip, "172.20.0.5".parse::<Ipv4Addr>().unwrap());
        assert_eq!(got.plugin, "fetch");
    }

    #[tokio::test]
    async fn duplicate_insert_is_rejected() {
        let store = SessionStore::new();
        store
            .insert(record("mcp_session_abc", "172.20.0.5", "fetch"))
            .await
            .expect("first");
        let err = store
            .insert(record("mcp_session_abc", "172.20.0.6", "fetch"))
            .await
            .expect_err("second should fail");
        assert_eq!(err, StoreError::AlreadyExists("mcp_session_abc".into()));
        let still_first = store.get("mcp_session_abc").await.expect("present");
        assert_eq!(
            still_first.container_ip,
            "172.20.0.5".parse::<Ipv4Addr>().unwrap()
        );
    }

    #[tokio::test]
    async fn remove_returns_removed_record_and_then_empty() {
        let store = SessionStore::new();
        store
            .insert(record("mcp_session_abc", "172.20.0.5", "fetch"))
            .await
            .expect("insert");
        let removed = store.remove("mcp_session_abc").await.expect("remove");
        assert_eq!(removed.session_id, "mcp_session_abc");
        assert!(store.get("mcp_session_abc").await.is_none());
        assert!(store.is_empty().await);
    }

    #[tokio::test]
    async fn remove_unknown_session_errors() {
        let store = SessionStore::new();
        let err = store
            .remove("mcp_session_nope")
            .await
            .expect_err("should fail");
        assert_eq!(err, StoreError::NotFound("mcp_session_nope".into()));
    }

    #[tokio::test]
    async fn list_returns_sorted_records() {
        let store = SessionStore::new();
        store
            .insert(record("mcp_session_b", "172.20.0.6", "fetch"))
            .await
            .unwrap();
        store
            .insert(record("mcp_session_a", "172.20.0.5", "git"))
            .await
            .unwrap();
        store
            .insert(record("mcp_session_c", "172.20.0.7", "exec-jq"))
            .await
            .unwrap();
        let listed = store.list().await;
        let ids: Vec<&str> = listed.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(ids, vec!["mcp_session_a", "mcp_session_b", "mcp_session_c"]);
    }

    #[tokio::test]
    async fn insert_bumps_generation() {
        let store = SessionStore::new();
        let initial = store.current_generation();
        store
            .insert(record("mcp_session_abc", "172.20.0.5", "fetch"))
            .await
            .unwrap();
        assert_eq!(store.current_generation(), initial.wrapping_add(1));
    }

    #[tokio::test]
    async fn remove_bumps_generation() {
        let store = SessionStore::new();
        store
            .insert(record("mcp_session_abc", "172.20.0.5", "fetch"))
            .await
            .unwrap();
        let before = store.current_generation();
        store.remove("mcp_session_abc").await.unwrap();
        assert_eq!(store.current_generation(), before.wrapping_add(1));
    }

    #[tokio::test]
    async fn failed_insert_does_not_bump_generation() {
        let store = SessionStore::new();
        store
            .insert(record("mcp_session_abc", "172.20.0.5", "fetch"))
            .await
            .unwrap();
        let before = store.current_generation();
        store
            .insert(record("mcp_session_abc", "172.20.0.6", "fetch"))
            .await
            .unwrap_err();
        assert_eq!(store.current_generation(), before);
    }

    #[tokio::test]
    async fn failed_remove_does_not_bump_generation() {
        let store = SessionStore::new();
        let before = store.current_generation();
        store.remove("mcp_session_nope").await.unwrap_err();
        assert_eq!(store.current_generation(), before);
    }

    #[tokio::test]
    async fn bulk_seed_bumps_generation_once() {
        let store = SessionStore::new();
        let before = store.current_generation();
        let count = store
            .bulk_seed(vec![
                record("mcp_session_a", "172.20.0.5", "git"),
                record("mcp_session_b", "172.20.0.6", "fetch"),
                record("mcp_session_c", "172.20.0.7", "exec-jq"),
            ])
            .await
            .unwrap();
        assert_eq!(count, 3);
        assert_eq!(store.current_generation(), before.wrapping_add(1));
        assert_eq!(store.len().await, 3);
    }

    #[tokio::test]
    async fn bulk_seed_rejects_internal_duplicates() {
        let store = SessionStore::new();
        let err = store
            .bulk_seed(vec![
                record("mcp_session_a", "172.20.0.5", "git"),
                record("mcp_session_a", "172.20.0.6", "fetch"),
            ])
            .await
            .unwrap_err();
        assert_eq!(err, StoreError::AlreadyExists("mcp_session_a".into()));
    }

    #[tokio::test]
    async fn subscribers_observe_generation_bumps() {
        let store = SessionStore::new();
        let mut rx = store.subscribe_generation();
        let initial = *rx.borrow();
        store
            .insert(record("mcp_session_abc", "172.20.0.5", "fetch"))
            .await
            .unwrap();
        rx.changed().await.expect("watch sender stayed open");
        assert_eq!(*rx.borrow_and_update(), initial.wrapping_add(1));
    }

    #[tokio::test]
    async fn record_acked_version_is_monotonic() {
        let store = SessionStore::new();
        assert_eq!(store.current_acked_version(), 0);
        store.record_acked_version(5);
        assert_eq!(store.current_acked_version(), 5);
        // Lower values are ignored — important because the xDS task
        // could theoretically receive an old ACK after a NACK round-
        // trip, and unblocking gate-waiters expecting the higher
        // value would be wrong.
        store.record_acked_version(3);
        assert_eq!(store.current_acked_version(), 5);
        store.record_acked_version(7);
        assert_eq!(store.current_acked_version(), 7);
    }

    #[tokio::test]
    async fn xds_subscriber_guard_tracks_open_streams() {
        let store = SessionStore::new();
        assert_eq!(store.xds_subscriber_count(), 0);
        let g1 = store.xds_subscriber_guard();
        assert_eq!(store.xds_subscriber_count(), 1);
        {
            let _g2 = store.xds_subscriber_guard();
            assert_eq!(store.xds_subscriber_count(), 2);
        }
        // Drop of g2 decremented.
        assert_eq!(store.xds_subscriber_count(), 1);
        drop(g1);
        assert_eq!(store.xds_subscriber_count(), 0);
    }

    #[tokio::test]
    async fn wait_for_ack_short_circuits_when_no_subscriber() {
        let store = SessionStore::new();
        let outcome = store
            .wait_for_ack(1, Duration::from_secs(5))
            .await
            .expect_err("must fail fast");
        assert_eq!(outcome, AckWaitError::NoSubscriber);
    }

    #[tokio::test]
    async fn wait_for_ack_returns_immediately_when_already_acked() {
        let store = SessionStore::new();
        let _guard = store.xds_subscriber_guard();
        store.record_acked_version(7);
        store
            .wait_for_ack(5, Duration::from_secs(5))
            .await
            .expect("already acked");
    }

    #[tokio::test]
    async fn wait_for_ack_unblocks_when_ack_arrives() {
        let store = Arc::new(SessionStore::new());
        let _guard = store.xds_subscriber_guard();
        let waiter_store = store.clone();
        let waiter =
            tokio::spawn(async move { waiter_store.wait_for_ack(3, Duration::from_secs(5)).await });
        // Give the waiter a chance to subscribe before the ack lands.
        tokio::time::sleep(Duration::from_millis(50)).await;
        store.record_acked_version(3);
        waiter.await.expect("join").expect("ack arrived");
    }

    #[tokio::test]
    async fn wait_for_ack_times_out_when_ack_does_not_arrive() {
        let store = SessionStore::new();
        let _guard = store.xds_subscriber_guard();
        let outcome = store
            .wait_for_ack(99, Duration::from_millis(75))
            .await
            .expect_err("must time out");
        assert_eq!(outcome, AckWaitError::Timeout(99));
    }

    #[tokio::test]
    async fn wait_for_ack_fails_fast_when_subscriber_drops_during_wait() {
        let store = Arc::new(SessionStore::new());
        let guard = store.xds_subscriber_guard();
        let waiter_store = store.clone();
        let waiter =
            tokio::spawn(
                async move { waiter_store.wait_for_ack(99, Duration::from_secs(5)).await },
            );
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Subscriber goes away.
        drop(guard);
        // Nudge the watch channel so wait_for_ack's loop notices the
        // subscriber-count change. `record_acked_version` with a lower
        // value still calls send_if_modified -> no notification; use a
        // value below the target so the waiter sees it but doesn't
        // succeed.
        store.record_acked_version(1);
        let outcome = waiter.await.expect("join").expect_err("must fail fast");
        assert_eq!(outcome, AckWaitError::NoSubscriber);
    }
}
