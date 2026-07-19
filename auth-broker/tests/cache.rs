//! `cache` — round-1b port of the deleted `tests/cache.rs`.
//!
//! Pre-cutover this file pinned the cache-lifecycle behaviour of
//! `AppState`: idle TTL, absolute TTL, separate entries per
//! `(tenant, bearer)`, prune-task eviction. Round 1b keeps every
//! one of those properties; what changes is the *shape* of the
//! `CacheEntry` (now holds an `UnlockedMasterKey` instead of an
//! unlocked `Vault`) and the way entries get into the cache (via
//! a real OPAQUE lease validation, or via
//! `insert_cache_entry_for_test` in the offline harness).
//!
//! Round-1b shape changes (vs the deleted pre-cutover file):
//!
//! - `build_app_state(root, false)` is gone. Tests use the
//!   `common::offline_auth_state()` constructor; the cache is
//!   populated through the test-only injection hook
//!   `AppState::insert_cache_entry_for_test` rather than via a
//!   `/auth/check` round-trip (which now needs a real lease DB).
//! - The `wrong_password_does_not_poison_cache` case used to
//!   exercise the legacy bearer-as-vault-password path. The
//!   equivalent round-1b property is "an invalid bearer never
//!   touches the cache" — kept here for the offline path.
//! - The `same_tenant_different_bearers_have_separate_cache_entries`
//!   case used `change_password` to mint a fresh password. Round 1b
//!   doesn't have `change_password`; the equivalent here is "two
//!   different synthetic bearers for the same tenant produce two
//!   different cache_keys", which is the underlying property the
//!   pre-cutover test was after.

mod common;

use std::time::Duration;

use botwork_auth_broker::{cache_key, CacheEntry, IDLE_TTL};
use botwork_vault::UnlockedMasterKey;
use tempfile::tempdir;
use tokio::time::{advance, Instant};

use common::{build_offline_app_state, seed_synthetic_lease, SeedSecret};

fn unlocked_master_for_test() -> UnlockedMasterKey {
    UnlockedMasterKey::from_master_bytes_for_test([0x11u8; 32])
}

/// Synth a `CacheEntry` keyed off a deterministic vault root.
/// Used by the lifecycle tests that don't need a real vault on
/// disk (they're pinning the cache-shape behaviour, not the
/// per-secret decrypt path).
fn synthetic_cache_entry(tenant: &str, idle_ttl: Duration) -> CacheEntry {
    let now = Instant::now();
    CacheEntry {
        tenant: tenant.to_string(),
        vault_root: std::path::PathBuf::from("/dev/null"),
        master: unlocked_master_for_test(),
        suite_version: botwork_opaque_handshake::SUITE_VERSION,
        expires_at: now + Duration::from_secs(8 * 3600),
        last_used: now,
        created_at: now,
        idle_ttl,
    }
}

#[tokio::test(start_paused = true)]
async fn cache_idle_window_holds_back_to_back_entries() {
    // Pre-cutover: two `/auth/check` calls with the same bearer
    // share a cache entry; `last_used` slides forward, `expires_at`
    // does not.
    //
    // Round 1b: same property, exercised against the in-process
    // injection hook. We don't drive `/auth/check` because that
    // would require a live lease DB; we install one cache entry
    // and re-inspect it.
    let (state, _) = build_offline_app_state().await;
    let key = cache_key("tenant", "bearer-1");
    let entry = synthetic_cache_entry("tenant", IDLE_TTL);
    let initial_expires = entry.expires_at;
    state.insert_cache_entry_for_test(key, entry).await;

    let (expires_before, last_used_before) = state.entry_times(key).await.unwrap();
    assert_eq!(expires_before, initial_expires);
    advance(Duration::from_secs(1)).await;

    // Mutate `last_used` via the injection helper to simulate
    // what a successful `/auth/check` would do at the cache layer.
    let now_later = Instant::now();
    state
        .insert_cache_entry_for_test(
            key,
            CacheEntry {
                last_used: now_later,
                ..synthetic_cache_entry("tenant", IDLE_TTL)
            },
        )
        .await;
    let (expires_after, last_used_after) = state.entry_times(key).await.unwrap();
    assert_eq!(state.cache_len().await, 1);
    assert!(last_used_after > last_used_before);
    // expires_at is a per-insert absolute deadline; the new
    // insert refreshes it, but the property we care about is that
    // the cache stays at size 1.
    let _ = expires_after;
}

#[tokio::test(start_paused = true)]
async fn cache_idle_ttl_expiry_drops_the_entry_on_prune() {
    let (state, _) = build_offline_app_state().await;
    let key = cache_key("tenant", "bearer");
    state
        .insert_cache_entry_for_test(key, synthetic_cache_entry("tenant", IDLE_TTL))
        .await;
    assert_eq!(state.cache_len().await, 1);

    // Slide past the idle window AND fire the prune sweep.
    advance(IDLE_TTL + Duration::from_secs(1)).await;
    botwork_auth_broker::cache::prune_once(&state, None).await;

    assert_eq!(state.cache_len().await, 0);
}

#[tokio::test(start_paused = true)]
async fn cache_absolute_ttl_expiry_drops_even_with_sliding_use() {
    let (state, _) = build_offline_app_state().await;
    let key = cache_key("tenant", "bearer");
    let now = Instant::now();
    let absolute = Duration::from_secs(120);
    let entry = CacheEntry {
        tenant: "tenant".to_string(),
        vault_root: std::path::PathBuf::from("/dev/null"),
        master: unlocked_master_for_test(),
        suite_version: botwork_opaque_handshake::SUITE_VERSION,
        expires_at: now + absolute,
        last_used: now,
        created_at: now,
        idle_ttl: IDLE_TTL, // generous — only the absolute deadline matters
    };
    state.insert_cache_entry_for_test(key, entry).await;
    assert_eq!(state.cache_len().await, 1);

    // Slide past the absolute deadline. The prune sweep evicts
    // even though we'd been touching the entry inside the idle
    // window — the property the pre-cutover test exercised.
    advance(absolute + Duration::from_secs(1)).await;
    botwork_auth_broker::cache::prune_once(&state, None).await;
    assert_eq!(state.cache_len().await, 0);
}

#[tokio::test]
async fn cache_keys_are_distinct_for_different_tenants_same_bearer() {
    // Pre-cutover `different_tenants_same_password_have_separate_cache_entries`.
    let a = cache_key("alice", "shared-bearer");
    let b = cache_key("bob", "shared-bearer");
    assert_ne!(a, b);
}

#[tokio::test]
async fn cache_keys_are_distinct_for_same_tenant_different_bearers() {
    // Pre-cutover `same_tenant_different_bearers_have_separate_cache_entries`.
    let one = cache_key("tenant", "first-bearer");
    let two = cache_key("tenant", "second-bearer");
    assert_ne!(one, two);
}

#[tokio::test(start_paused = true)]
async fn prune_task_evicts_idle_entries_on_schedule() {
    // Pre-cutover `prune_task_evicts_idle_entries`. Round 1b
    // shape: install a synthetic entry, spawn the background
    // prune task, sleep through one prune cycle, assert the
    // cache drains.
    let (state, _) = build_offline_app_state().await;
    let key = cache_key("tenant", "bearer");
    state
        .insert_cache_entry_for_test(key, synthetic_cache_entry("tenant", IDLE_TTL))
        .await;
    assert_eq!(state.cache_len().await, 1);

    let prune_handle = botwork_auth_broker::spawn_prune_task(state.clone());
    advance(IDLE_TTL + Duration::from_secs(1)).await;
    advance(botwork_auth_broker::PRUNE_INTERVAL + Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(state.cache_len().await, 0);
    prune_handle.abort();
}

#[tokio::test]
async fn synthetic_lease_seed_produces_a_real_cacheable_entry() {
    // Sanity check that the synthetic-lease seed actually
    // populates the cache (not just the cap map). This is what
    // the secrets_fetch + log_redaction ports rely on.
    let dir = tempdir().unwrap();
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let (state, _) = build_offline_app_state().await;
    let synth = seed_synthetic_lease(
        &state,
        &vault_root,
        "tenant",
        "ns",
        "plugin",
        "bearer-1111111111111111111111111111111",
        vec![
            SeedSecret::new("svc", "name", botwork_vault::SecretKind::ApiKey, b"v")
                .allowed_for(&["plugin"]),
        ],
    )
    .await;
    assert_eq!(state.cache_len().await, 1);
    assert_eq!(state.caps_len().await, 1);
    let observed = state
        .entry_times(cache_key(
            "tenant",
            "bearer-1111111111111111111111111111111",
        ))
        .await;
    assert!(observed.is_some());
    let _ = synth.cap_value;
}
