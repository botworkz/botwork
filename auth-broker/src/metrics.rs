//! Lightweight counters exposed by `auth-broker` for in-memory cache
//! lifecycle observability.
//!
//! ## Scope (issue #126)
//!
//! Issue #126's TTL-tightening section asks for metrics on cache size,
//! evictions per minute, and average entry age. This module provides
//! the *primitives*; a future Prometheus exporter PR can either pull
//! these snapshots on /metrics scrape or push them through an
//! `OpenTelemetry` exporter — that wiring is out of scope here.
//!
//! ## Why not `prometheus` directly?
//!
//! - The auth-broker container is currently distroless-with-no-rustls;
//!   pulling `prometheus` adds a non-trivial dep closure to a service
//!   that ought to remain lean.
//! - Operators in the `botworkz/space` smoke-test loop don't yet have
//!   a scraper to point at the broker. Atomic counters that get
//!   queried from the test harness are enough to prove the eviction
//!   path is exercised.
//! - When the time comes, the [`MetricsSnapshot`] returned by
//!   [`Metrics::snapshot`] is a drop-in datasource for an exporter.
//!
//! All counters are monotonic since broker start. Average entry age
//! is computed on-demand from the cache itself (see
//! [`crate::cache::AppState::metrics_snapshot`]).

use std::sync::atomic::{AtomicU64, Ordering};

/// Lock-free counter set. Each field is incremented from at most one
/// call site; using `AtomicU64` keeps the hot path off the cache mutex.
///
/// Counters are intentionally minimal: the only eviction reasons this
/// crate can currently emit are idle-TTL and absolute-TTL expiry.
/// When bounded-cache / LRU eviction lands as its own follow-up PR,
/// it adds the counter (`cache_evictions_lru` or whatever fits) at
/// the same time as the code path that fires it — so the counter
/// set and the eviction reasons stay one-to-one. Reserving an
/// always-zero counter here would just train operators to ignore it.
#[derive(Debug, Default)]
pub struct Metrics {
    cache_inserts: AtomicU64,
    cache_evictions_idle: AtomicU64,
    cache_evictions_absolute: AtomicU64,
}

impl Metrics {
    /// Increment the "vault unlocked + cached" counter.
    pub fn inc_insert(&self) {
        self.cache_inserts.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the "evicted because idle TTL elapsed" counter.
    pub fn inc_eviction_idle(&self) {
        self.cache_evictions_idle.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the "evicted because absolute TTL elapsed" counter.
    pub fn inc_eviction_absolute(&self) {
        self.cache_evictions_absolute
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Take a non-atomic snapshot of the counters. Inexpensive — pure
    /// loads.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            cache_inserts: self.cache_inserts.load(Ordering::Relaxed),
            cache_evictions_idle: self.cache_evictions_idle.load(Ordering::Relaxed),
            cache_evictions_absolute: self.cache_evictions_absolute.load(Ordering::Relaxed),
        }
    }
}

/// Read-only snapshot returned by [`Metrics::snapshot`]. Combined with
/// a live cache-size + average-entry-age reading in
/// [`crate::cache::AppState::metrics_snapshot`] for the full picture.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MetricsSnapshot {
    /// Total successful "unlock + cache insert" calls since startup.
    pub cache_inserts: u64,
    /// Total entries evicted because the idle TTL elapsed.
    pub cache_evictions_idle: u64,
    /// Total entries evicted because the absolute TTL elapsed.
    pub cache_evictions_absolute: u64,
}

impl MetricsSnapshot {
    /// Sum of every eviction reason. Convenience accessor for the
    /// common "how much churn?" dashboard tile.
    pub fn cache_evictions_total(self) -> u64 {
        self.cache_evictions_idle
            .saturating_add(self.cache_evictions_absolute)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_each_counter_independently() {
        let m = Metrics::default();
        m.inc_insert();
        m.inc_insert();
        m.inc_eviction_idle();
        m.inc_eviction_absolute();
        m.inc_eviction_absolute();
        m.inc_eviction_absolute();

        let snap = m.snapshot();
        assert_eq!(snap.cache_inserts, 2);
        assert_eq!(snap.cache_evictions_idle, 1);
        assert_eq!(snap.cache_evictions_absolute, 3);
        assert_eq!(snap.cache_evictions_total(), 4);
    }
}
