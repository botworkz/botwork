//! `lease_export_key_ttl` — pins the acceptance criteria for issue #213:
//! `AuthState.lease_export_keys` must not grow without bound.
//!
//! Every test uses `tokio::time::start_paused = true` so the TTL can be
//! exercised deterministically without real wall-clock sleeps.

mod common;

use botwork_auth_broker::LEASE_EXPORT_KEY_TTL;
use tokio::time::{advance, Instant};
use uuid::Uuid;

use common::offline_auth_state;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Build a minimal export-key payload (64 zero bytes — the OPAQUE
/// SessionKey is 64 bytes; the exact value does not matter for these
/// TTL-only tests).
fn fake_export_key() -> Vec<u8> {
    vec![0u8; 64]
}

// ---------------------------------------------------------------------------
// sweep evicts stale entries
// ---------------------------------------------------------------------------

/// Inserting N entries then sweeping past the TTL must bring the map
/// to zero. This is the core acceptance criterion from issue #213.
#[tokio::test(start_paused = true)]
async fn sweep_evicts_all_stale_entries() {
    let auth = offline_auth_state().await;
    let key = fake_export_key();

    // Insert ten entries with distinct lease IDs.
    for _ in 0..10 {
        auth.remember_lease_export_key(Uuid::new_v4(), &key).await;
    }
    assert_eq!(auth.lease_export_key_count().await, 10);

    // Advance past the TTL and sweep.
    advance(LEASE_EXPORT_KEY_TTL + std::time::Duration::from_secs(1)).await;
    let evicted = auth.sweep_lease_export_keys(Instant::now()).await;
    assert_eq!(evicted, 10, "sweep must evict all stale entries");
    assert_eq!(
        auth.lease_export_key_count().await,
        0,
        "map must be empty after sweep"
    );
}

// ---------------------------------------------------------------------------
// sweep preserves fresh entries
// ---------------------------------------------------------------------------

/// An entry inserted *after* the advance must survive the sweep.
#[tokio::test(start_paused = true)]
async fn sweep_preserves_fresh_entries() {
    let auth = offline_auth_state().await;
    let key = fake_export_key();

    // Insert a stale entry.
    let stale_id = Uuid::new_v4();
    auth.remember_lease_export_key(stale_id, &key).await;

    // Advance by more than one TTL so the stale entry expires.
    advance(LEASE_EXPORT_KEY_TTL + std::time::Duration::from_secs(1)).await;

    // Insert a fresh entry *after* the advance.
    let fresh_id = Uuid::new_v4();
    auth.remember_lease_export_key(fresh_id, &key).await;

    let evicted = auth.sweep_lease_export_keys(Instant::now()).await;
    assert_eq!(evicted, 1, "only the stale entry should be evicted");
    assert_eq!(
        auth.lease_export_key_count().await,
        1,
        "the fresh entry must survive the sweep"
    );

    // Confirm the surviving entry is the fresh one.
    assert!(
        auth.lease_export_key(fresh_id).await.is_some(),
        "fresh entry must still be retrievable"
    );
    assert!(
        auth.lease_export_key(stale_id).await.is_none(),
        "stale entry must be gone"
    );
}

// ---------------------------------------------------------------------------
// lazy eviction on read
// ---------------------------------------------------------------------------

/// `lease_export_key` must return `None` for an expired entry and
/// remove it in-place so the map shrinks without waiting for the sweep.
#[tokio::test(start_paused = true)]
async fn get_removes_expired_entry_lazily() {
    let auth = offline_auth_state().await;
    let id = Uuid::new_v4();
    auth.remember_lease_export_key(id, &fake_export_key()).await;

    // The entry is present while fresh.
    assert!(
        auth.lease_export_key(id).await.is_some(),
        "entry must be retrievable while fresh"
    );

    // Advance past the TTL.
    advance(LEASE_EXPORT_KEY_TTL + std::time::Duration::from_secs(1)).await;

    // Lookup must return None and must have evicted the entry.
    assert!(
        auth.lease_export_key(id).await.is_none(),
        "expired entry must not be returned"
    );
    assert_eq!(
        auth.lease_export_key_count().await,
        0,
        "expired entry must be removed lazily"
    );
}

// ---------------------------------------------------------------------------
// map does not grow unboundedly across repeated logins
// ---------------------------------------------------------------------------

/// Simulate 100 successive logins for distinct lease IDs, all with a
/// clock advanced past the TTL between batches. At no point should
/// the map exceed the number of entries inserted in the current window.
#[tokio::test(start_paused = true)]
async fn repeated_logins_do_not_grow_map_unboundedly() {
    let auth = offline_auth_state().await;
    let key = fake_export_key();
    const BATCH_SIZE: usize = 20;
    const BATCHES: usize = 5;

    for batch in 0..BATCHES {
        // Each batch inserts BATCH_SIZE distinct lease IDs.
        for _ in 0..BATCH_SIZE {
            auth.remember_lease_export_key(Uuid::new_v4(), &key).await;
        }

        // Entries from previous sweeps are gone; only this batch's entries
        // should be in the map.
        assert_eq!(
            auth.lease_export_key_count().await,
            BATCH_SIZE,
            "batch {batch}: map must contain exactly one batch worth of entries \
             (previous entries were swept)"
        );

        // Advance past the TTL and sweep — simulates the background prune.
        advance(LEASE_EXPORT_KEY_TTL + std::time::Duration::from_secs(1)).await;
        let evicted = auth.sweep_lease_export_keys(Instant::now()).await;
        assert_eq!(
            evicted, BATCH_SIZE,
            "batch {batch}: sweep must evict exactly one batch worth of entries"
        );
        assert_eq!(
            auth.lease_export_key_count().await,
            0,
            "batch {batch}: map must be empty after sweep"
        );
    }
}

// ---------------------------------------------------------------------------
// remember refreshes the timestamp (upsert semantics)
// ---------------------------------------------------------------------------

/// Calling `remember_lease_export_key` a second time for the same
/// `lease_id` must refresh the entry's timestamp so it doesn't expire
/// while actively in use.
#[tokio::test(start_paused = true)]
async fn remember_refreshes_timestamp() {
    let auth = offline_auth_state().await;
    let id = Uuid::new_v4();
    auth.remember_lease_export_key(id, &fake_export_key()).await;

    // Advance to just inside the TTL boundary.
    let just_inside = LEASE_EXPORT_KEY_TTL - std::time::Duration::from_secs(1);
    advance(just_inside).await;

    // Refresh the entry — this resets the timestamp to "now".
    auth.remember_lease_export_key(id, &fake_export_key()).await;

    // Advance another TTL - 1s (total: 2*(TTL - 1s)).
    // The first timestamp would be well past TTL; the refreshed one is fresh.
    advance(just_inside).await;

    // Entry must still be live because the refresh reset the clock.
    assert!(
        auth.lease_export_key(id).await.is_some(),
        "refreshed entry must not have expired yet"
    );

    // Advance past the second TTL window.
    advance(std::time::Duration::from_secs(2)).await;
    assert!(
        auth.lease_export_key(id).await.is_none(),
        "entry must expire after second TTL window"
    );
}
