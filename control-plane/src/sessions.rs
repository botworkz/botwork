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

use std::collections::HashMap;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;

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

#[derive(Default)]
pub struct SessionStore {
    inner: RwLock<HashMap<String, SessionRecord>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Strict insert: rejects a second insert for the same `session_id`.
    /// See module docs for the rationale.
    pub async fn insert(&self, record: SessionRecord) -> Result<(), StoreError> {
        let mut guard = self.inner.write().await;
        if guard.contains_key(&record.session_id) {
            return Err(StoreError::AlreadyExists(record.session_id));
        }
        guard.insert(record.session_id.clone(), record);
        Ok(())
    }

    /// Strict delete: rejects when `session_id` was never inserted (or
    /// was already removed). Forces session-broker and control-plane to
    /// disagree loudly rather than quietly drift.
    pub async fn remove(&self, session_id: &str) -> Result<SessionRecord, StoreError> {
        let mut guard = self.inner.write().await;
        guard
            .remove(session_id)
            .ok_or_else(|| StoreError::NotFound(session_id.to_string()))
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
}
