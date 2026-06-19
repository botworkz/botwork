//! In-memory store of `SessionRecord` keyed by session id.
//!
//! Owned by `AppState` and shared (via `Arc<SessionStore>`) across the axum
//! handlers. Concurrency is a single `tokio::sync::RwLock<HashMap<…>>`: v0
//! traffic is one event per session-spawn/teardown, so write contention
//! is irrelevant and read traffic (the xDS subscriber, future health
//! endpoints) overwhelmingly outweighs writes.
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
//! ## Generation channel
//!
//! Mutating ops (`insert`, `remove`, `bulk_seed`) bump a monotonic `u64`
//! generation counter published on a `tokio::sync::watch::Sender<u64>`.
//! Subscribers (today: the xDS server) use it as a "something changed,
//! re-snapshot now" signal — the actual data is fetched via `list()`
//! against the same store, so subscribers never see partial state and
//! never deadlock against the write lock.
//!
//! The counter is opaque (it is not a version number we hand to envoy
//! verbatim — the xDS layer derives `version_info` from it). We use a
//! `watch` channel rather than a `broadcast` because xDS subscribers
//! only ever care about "the latest" state; missing intermediate
//! bumps when there's a burst of session churn is fine and in fact
//! desirable (one push per quiescent state, not one per mutation).

use std::collections::HashMap;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{watch, RwLock};

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

pub struct SessionStore {
    inner: RwLock<HashMap<String, SessionRecord>>,
    /// Monotonic mutation counter. Bumped after every successful
    /// `insert` / `remove` / `bulk_seed`. Subscribers (xDS) `subscribe`
    /// to it; the value itself is opaque to them.
    generation_tx: watch::Sender<u64>,
}

impl Default for SessionStore {
    fn default() -> Self {
        let (generation_tx, _) = watch::channel(0u64);
        Self {
            inner: RwLock::new(HashMap::new()),
            generation_tx,
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
    /// asserting that mutations bumped the counter.
    pub fn current_generation(&self) -> u64 {
        *self.generation_tx.borrow()
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
        // The first record stays — the second was rejected, not merged.
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
        // Duplicate insert: bails before mutation.
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
        // Single bump regardless of N.
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

        // Watch channel: changed() returns Ok once the value moves
        // after the receiver's last-observed mark.
        rx.changed().await.expect("watch sender stayed open");
        assert_eq!(*rx.borrow_and_update(), initial.wrapping_add(1));
    }
}
