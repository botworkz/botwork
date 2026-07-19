//! `ttl_config` — round-1b port of the deleted
//! `tests/ttl_config.rs`.
//!
//! Pre-cutover: per-tenant idle / absolute TTL knobs (issue #126,
//! Tier 2 hardening) exercised end-to-end through `/auth/check`.
//! Round 1b can't drive `/auth/check` without a real lease DB, so
//! the ported tests exercise the same `TtlConfig` knobs via the
//! in-process injection hook: install a `CacheEntry` with the
//! configured idle/absolute TTL on it, slide the clock past the
//! window, drive `prune_once`, observe the cache draining
//! correctly.
//!
//! The metric counters (`cache_inserts`, `cache_evictions_idle`,
//! `cache_evictions_absolute`) survive the cutover unchanged;
//! they're re-pinned here.

mod common;

use std::time::Duration;

use botwork_auth_broker::{
    build_app_state_with_ttl_config, cache_key, AppState, CacheEntry, TtlConfig, IDLE_TTL,
};
use botwork_vault::UnlockedMasterKey;
use tempfile::tempdir;
use tokio::time::{advance, Instant};

use common::offline_auth_state;

fn unlocked_master_for_test() -> UnlockedMasterKey {
    UnlockedMasterKey::from_master_bytes_for_test([0x55u8; 32])
}

fn synthetic_cache_entry(tenant: &str, idle_ttl: Duration, absolute_ttl: Duration) -> CacheEntry {
    let now = Instant::now();
    CacheEntry {
        tenant: tenant.to_string(),
        vault_root: std::path::PathBuf::from("/dev/null"),
        master: unlocked_master_for_test(),
        suite_version: botwork_opaque_handshake::SUITE_VERSION,
        expires_at: now + absolute_ttl,
        last_used: now,
        created_at: now,
        idle_ttl,
    }
}

async fn build_state(ttl: TtlConfig) -> (AppState, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);
    let state = build_app_state_with_ttl_config(path.clone(), offline_auth_state().await, ttl);
    (state, path)
}

#[tokio::test(start_paused = true)]
async fn idle_ttl_is_taken_from_config_default() {
    // Default config: per-tenant idle == IDLE_TTL. Install one
    // entry under the default, slide past the idle window, prune,
    // assert the entry drops.
    let (state, _) = build_state(TtlConfig::default()).await;
    let key = cache_key("tenant", "bearer");
    let entry = synthetic_cache_entry(
        "tenant",
        state.ttl_config.idle_for("tenant"),
        state.ttl_config.absolute_for("tenant"),
    );
    state.insert_cache_entry_for_test(key, entry).await;
    assert_eq!(state.cache_len().await, 1);

    advance(IDLE_TTL + Duration::from_secs(1)).await;
    botwork_auth_broker::cache::prune_once(&state, None).await;
    assert_eq!(state.cache_len().await, 0);

    let snap = state.metrics_snapshot().await;
    assert_eq!(snap.counters.cache_evictions_idle, 1);
    assert_eq!(snap.counters.cache_evictions_absolute, 0);
}

#[tokio::test(start_paused = true)]
async fn per_tenant_idle_override_shorter_than_default_evicts_sooner() {
    let tight_idle = Duration::from_secs(30);
    let config = TtlConfig::default().with_tenant_idle("tenant", tight_idle);
    let (state, _) = build_state(config).await;
    let key = cache_key("tenant", "bearer");
    let entry = synthetic_cache_entry(
        "tenant",
        state.ttl_config.idle_for("tenant"),
        state.ttl_config.absolute_for("tenant"),
    );
    state.insert_cache_entry_for_test(key, entry).await;

    advance(tight_idle + Duration::from_secs(1)).await;
    botwork_auth_broker::cache::prune_once(&state, None).await;
    assert_eq!(state.cache_len().await, 0);
    let snap = state.metrics_snapshot().await;
    assert!(snap.counters.cache_evictions_idle >= 1);
}

#[tokio::test(start_paused = true)]
async fn per_tenant_absolute_override_evicts_at_configured_deadline() {
    let absolute = Duration::from_secs(120);
    let config = TtlConfig::default().with_tenant_absolute("tenant", absolute);
    let (state, _) = build_state(config).await;
    let key = cache_key("tenant", "bearer");
    let entry = synthetic_cache_entry(
        "tenant",
        state.ttl_config.idle_for("tenant"),
        state.ttl_config.absolute_for("tenant"),
    );
    state.insert_cache_entry_for_test(key, entry).await;

    advance(absolute + Duration::from_secs(1)).await;
    botwork_auth_broker::cache::prune_once(&state, None).await;
    assert_eq!(state.cache_len().await, 0);
    let snap = state.metrics_snapshot().await;
    assert!(snap.counters.cache_evictions_absolute >= 1);
}

#[tokio::test(start_paused = true)]
async fn min_idle_floor_clamps_a_too_aggressive_per_tenant_override() {
    let floor = Duration::from_secs(60);
    let attempted_override = Duration::from_secs(10);
    let config = TtlConfig::default()
        .with_min_idle(floor)
        .with_tenant_idle("tenant", attempted_override);
    let (state, _) = build_state(config).await;

    // The override of 10s gets clamped to the floor (60s); the
    // effective idle TTL we see on the entry is the floor.
    let effective = state.ttl_config.idle_for("tenant");
    assert!(
        effective >= floor,
        "expected idle TTL to clamp at floor; got {effective:?}"
    );

    // And per the floor, a request inside the attempted override
    // (10s) but outside the original would-have-been-too-aggressive
    // window does NOT trigger eviction.
    let key = cache_key("tenant", "bearer");
    state
        .insert_cache_entry_for_test(key, synthetic_cache_entry("tenant", effective, IDLE_TTL))
        .await;
    advance(attempted_override + Duration::from_secs(1)).await;
    botwork_auth_broker::cache::prune_once(&state, None).await;
    assert_eq!(state.cache_len().await, 1);
}

#[tokio::test(start_paused = true)]
async fn metrics_count_idle_evictions() {
    let tight_idle = Duration::from_secs(30);
    let config = TtlConfig::default().with_tenant_idle("tenant", tight_idle);
    let (state, _) = build_state(config).await;
    let key = cache_key("tenant", "bearer");
    state
        .insert_cache_entry_for_test(key, synthetic_cache_entry("tenant", tight_idle, IDLE_TTL))
        .await;

    let initial = state.metrics_snapshot().await;
    assert_eq!(initial.counters.cache_inserts, 1);
    assert_eq!(initial.counters.cache_evictions_idle, 0);
    assert_eq!(initial.cache_size, 1);

    advance(tight_idle + Duration::from_secs(1)).await;
    botwork_auth_broker::cache::prune_once(&state, None).await;

    let after = state.metrics_snapshot().await;
    assert_eq!(after.counters.cache_evictions_idle, 1);
    assert_eq!(after.counters.cache_evictions_absolute, 0);
    assert_eq!(after.cache_size, 0);
}

#[tokio::test(start_paused = true)]
async fn metrics_average_entry_age_reflects_live_cache() {
    let (state, _) = build_state(TtlConfig::default()).await;
    state
        .insert_cache_entry_for_test(
            cache_key("a", "bearer"),
            synthetic_cache_entry("a", IDLE_TTL, Duration::from_secs(8 * 3600)),
        )
        .await;
    advance(Duration::from_secs(10)).await;
    state
        .insert_cache_entry_for_test(
            cache_key("b", "bearer"),
            synthetic_cache_entry("b", IDLE_TTL, Duration::from_secs(8 * 3600)),
        )
        .await;

    let snap = state.metrics_snapshot().await;
    assert_eq!(snap.cache_size, 2);
    assert!(
        (3..=7).contains(&snap.avg_age_secs),
        "avg_age_secs={} not in [3,7]",
        snap.avg_age_secs
    );
}

#[tokio::test(start_paused = true)]
async fn prune_task_credits_evictions_to_idle_counter() {
    let config = TtlConfig::default().with_tenant_idle("tenant", Duration::from_secs(30));
    let (state, _) = build_state(config).await;
    let _prune = botwork_auth_broker::spawn_prune_task(state.clone());

    state
        .insert_cache_entry_for_test(
            cache_key("tenant", "bearer"),
            synthetic_cache_entry("tenant", Duration::from_secs(30), IDLE_TTL),
        )
        .await;

    advance(Duration::from_secs(31)).await;
    advance(botwork_auth_broker::PRUNE_INTERVAL + Duration::from_secs(1)).await;
    tokio::task::yield_now().await;

    let snap = state.metrics_snapshot().await;
    assert_eq!(snap.cache_size, 0);
    assert!(snap.counters.cache_evictions_idle >= 1);
}
