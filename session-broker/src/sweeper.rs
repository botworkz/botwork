//! TTL sweeper for expiry-keyed maps in [`AppState`].
//!
//! `AppState` holds two `HashMap<K, Instant>` maps where the `Instant` value
//! is the entry's *expiry time*: `tombstones` and `liveness_cache`. The lookup
//! paths into these maps are best-effort, lazy purges — `is_tombstoned`
//! removes expired entries on access, and `check_container_liveness`
//! overwrites on refresh — but neither path is guaranteed to visit every
//! expired entry. In the steady state both maps would otherwise grow
//! monotonically with historical session/container count for the lifetime of
//! the broker process:
//!
//! * `tombstones` is keyed on `Mcp-Session-Id`. A real client never retries
//!   against a torn-down id, so the lazy-on-access purge in `is_tombstoned`
//!   almost never fires for entries that should be evicted.
//! * `liveness_cache` is keyed on container name. Names are random per spawn
//!   (`mcp_session_<token>`), so once a container is torn down its entry is
//!   never looked up again — pure dead weight with no lazy eviction path.
//!
//! This module exposes a generic background sweeper that walks the map on a
//! fixed interval and drops every entry whose expiry is in the past. It is
//! deliberately small and shared across both maps — the value type and
//! semantics are identical.
//!
//! [`AppState`]: crate::AppState

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::log_info;

/// Default interval between TTL sweeps, in seconds. Overridden at runtime by
/// `BOTWORK_BROKER_SWEEPER_INTERVAL_SECS` (see [`sweeper_interval_from_env`]).
///
/// Both maps currently use a 5 min TTL (`TOMBSTONE_TTL`, `LIVENESS_TTL`); a
/// 60 s sweep gives a worst-case 1 min residency overshoot on top of the TTL,
/// which is well inside the noise floor for both maps.
pub const DEFAULT_SWEEPER_INTERVAL_SECS: u64 = 60;

/// Pure helper: converts an already-read env value into the sweeper
/// interval. `None` (unset), `Some("0")`, and any unparseable string all
/// yield [`DEFAULT_SWEEPER_INTERVAL_SECS`].
///
/// Extracted so tests can exercise the parse/default logic directly without
/// mutating the process-global environment.
pub fn sweeper_interval_from(raw: Option<&str>) -> Duration {
    let secs = raw
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_SWEEPER_INTERVAL_SECS);
    Duration::from_secs(secs)
}

/// Reads `BOTWORK_BROKER_SWEEPER_INTERVAL_SECS`, falling back to
/// [`DEFAULT_SWEEPER_INTERVAL_SECS`].  A value of `0`, an unparseable string,
/// or an unset variable all yield the default — the sweeper cannot run with a
/// zero interval (`tokio::time::interval` panics on `Duration::ZERO`), and
/// silently degrading to the default is friendlier than refusing to start.
pub fn sweeper_interval_from_env() -> Duration {
    sweeper_interval_from(
        std::env::var("BOTWORK_BROKER_SWEEPER_INTERVAL_SECS")
            .ok()
            .as_deref(),
    )
}

/// Removes every entry from `map` whose expiry is at or before `now`. Returns
/// the number of entries dropped.
///
/// The boundary case (`expires_at == now`) is treated as **expired**, matching
/// the `Instant::now() < expires_at` checks already used by `is_tombstoned`
/// and `check_container_liveness` (both treat equality as expired).
///
/// Public so unit tests can exercise the predicate without spawning a task.
pub async fn prune_expired<K>(map: &Arc<Mutex<HashMap<K, Instant>>>, now: Instant) -> usize
where
    K: Eq + Hash,
{
    let mut guard = map.lock().await;
    let before = guard.len();
    guard.retain(|_, expires_at| *expires_at > now);
    before - guard.len()
}

/// Spawns a long-running task that prunes expired entries from `map` every
/// `interval`.
///
/// The returned [`JoinHandle`] is owned by the caller — typically the broker's
/// `run()`, which keeps it alive for the lifetime of the process and lets the
/// tokio runtime abort it on shutdown. There is no graceful-shutdown signal
/// for the sweeper itself; it holds no resources beyond the map `Arc`.
///
/// `label` is included in the `log_info` line emitted when a sweep evicts ≥ 1
/// entry. Keep it short and operator-recognisable (the existing two call
/// sites use the matching `AppState` field name).
///
/// Panics if `interval` is zero, matching `tokio::time::interval`'s contract.
/// Callers should obtain the interval via [`sweeper_interval_from_env`], which
/// rejects zero.
pub fn spawn_ttl_sweeper<K>(
    label: &'static str,
    map: Arc<Mutex<HashMap<K, Instant>>>,
    interval: Duration,
) -> JoinHandle<()>
where
    K: Eq + Hash + Send + 'static,
{
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // tokio::time::interval fires immediately on the first tick(). Skip
        // it — there is nothing to prune before any traffic has arrived, and
        // we don't want a sweep log line at boot.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let removed = prune_expired(&map, Instant::now()).await;
            if removed > 0 {
                log_info(&format!(
                    "{label} sweeper: removed {removed} expired entries"
                ));
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prune_expired_drops_only_expired_entries() {
        let now = Instant::now();
        let map: Arc<Mutex<HashMap<String, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut g = map.lock().await;
            g.insert("past-1".to_string(), now - Duration::from_secs(10));
            g.insert("past-2".to_string(), now - Duration::from_secs(1));
            // Boundary: expiry equal to "now" is considered expired (matches
            // the `<` check used by the routing path).
            g.insert("boundary".to_string(), now);
            g.insert("future-1".to_string(), now + Duration::from_secs(10));
            g.insert("future-2".to_string(), now + Duration::from_secs(60));
        }

        let removed = prune_expired(&map, now).await;
        assert_eq!(removed, 3);

        let g = map.lock().await;
        assert_eq!(g.len(), 2);
        assert!(g.contains_key("future-1"));
        assert!(g.contains_key("future-2"));
    }

    #[tokio::test]
    async fn prune_expired_on_empty_map_is_noop() {
        let map: Arc<Mutex<HashMap<String, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
        let removed = prune_expired(&map, Instant::now()).await;
        assert_eq!(removed, 0);
    }

    #[tokio::test]
    async fn spawn_ttl_sweeper_evicts_expired_entries() {
        let interval = Duration::from_millis(50);
        let map: Arc<Mutex<HashMap<String, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut g = map.lock().await;
            // Already expired at spawn time — should disappear on the first
            // sweep tick (which runs at t = interval, since tick #0 is
            // skipped).
            g.insert("stale".to_string(), Instant::now() - Duration::from_secs(1));
            // Still valid well past any plausible test runtime.
            g.insert(
                "fresh".to_string(),
                Instant::now() + Duration::from_secs(3600),
            );
        }

        let handle = spawn_ttl_sweeper("test", Arc::clone(&map), interval);
        // Sweeper skips its first tick, so wait > 2 × interval to guarantee
        // exactly one prune cycle has run before we assert.
        tokio::time::sleep(interval * 3).await;
        handle.abort();

        let g = map.lock().await;
        assert_eq!(g.len(), 1, "expected 1 entry remaining, got {}", g.len());
        assert!(g.contains_key("fresh"));
        assert!(!g.contains_key("stale"));
    }

    #[tokio::test]
    async fn spawn_ttl_sweeper_survives_no_op_sweeps() {
        // Regression: a sweep that evicts nothing must not panic, log, or
        // exit the task. Boot the sweeper against an empty map, wait long
        // enough for several ticks, then verify the handle is still alive
        // (not finished) by checking that `abort()` actually cancels it.
        let interval = Duration::from_millis(20);
        let map: Arc<Mutex<HashMap<String, Instant>>> = Arc::new(Mutex::new(HashMap::new()));

        let handle = spawn_ttl_sweeper("noop-test", Arc::clone(&map), interval);
        tokio::time::sleep(interval * 5).await;
        assert!(
            !handle.is_finished(),
            "sweeper task must not exit when sweeps find nothing to prune"
        );
        handle.abort();
    }

    #[test]
    fn sweeper_interval_unset_yields_default() {
        assert_eq!(
            sweeper_interval_from(None),
            Duration::from_secs(DEFAULT_SWEEPER_INTERVAL_SECS),
            "unset should fall back to default"
        );
    }

    #[test]
    fn sweeper_interval_zero_yields_default() {
        assert_eq!(
            sweeper_interval_from(Some("0")),
            Duration::from_secs(DEFAULT_SWEEPER_INTERVAL_SECS),
            "zero should fall back to default"
        );
    }

    #[test]
    fn sweeper_interval_garbage_yields_default() {
        assert_eq!(
            sweeper_interval_from(Some("not-a-number")),
            Duration::from_secs(DEFAULT_SWEEPER_INTERVAL_SECS),
            "garbage should fall back to default"
        );
    }

    #[test]
    fn sweeper_interval_positive_integer_is_honoured() {
        assert_eq!(
            sweeper_interval_from(Some("7")),
            Duration::from_secs(7),
            "positive integer should be honoured"
        );
    }
}
