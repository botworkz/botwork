//! `cap_lease_cohort` — pin the lease-cohort eviction semantics in
//! tests that don't need a live broker / postgres.
//!
//! Round 1b: [`CapEntry::lease_id`] is now a required `Uuid` (the
//! `Option<Uuid>` from round 1a collapsed when the legacy
//! bearer-as-vault-password path went away). The data-structure
//! tests below assert that:
//!
//! 1. **Lease cohort eviction is precise.** Caps for lease A are
//!    removed; caps for lease B survive.
//! 2. **Repeat-revoke is idempotent.** A second call on the same
//!    `lease_id` evicts zero additional caps and doesn't panic.
//! 3. **Already-expired-by-TTL caps in the cohort are still
//!    evicted.** Cohort eviction is keyed on lease_id, not on cap
//!    expiry.
//!
//! End-to-end "real postgres lease id flowing into a cap" coverage
//! lives in `tests/opaque_e2e.rs::lease_path_cap_carries_lease_id`,
//! which is docker-gated. This file stays green on dev machines
//! without docker by exercising the [`CapMap`] helper directly.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use botwork_auth_broker::cache::evict_caps_from_map;
use botwork_auth_broker::{CapEntry, CapId, CAP_TTL};
use rand::{rngs::SysRng, TryRng};
use tokio::sync::Mutex;
use tokio::time::Instant;
use uuid::Uuid;

fn fresh_cap_id() -> CapId {
    let mut id = [0u8; 32];
    let mut rng = SysRng;
    rng.try_fill_bytes(&mut id)
        .expect("SysRng should be available");
    id
}

fn cap_entry(cache_key: [u8; 32], lease_id: Uuid) -> CapEntry {
    CapEntry {
        cache_key,
        namespace: "ns".to_string(),
        plugin: "plugin".to_string(),
        expires_at: Instant::now() + CAP_TTL,
        lease_id,
    }
}

fn make_caps() -> Arc<Mutex<HashMap<CapId, CapEntry>>> {
    Arc::new(Mutex::new(HashMap::new()))
}

async fn caps_len(caps: &Arc<Mutex<HashMap<CapId, CapEntry>>>) -> usize {
    caps.lock().await.len()
}

#[tokio::test]
async fn evict_caps_for_lease_drops_only_the_named_cohort() {
    let caps = make_caps();

    let lease_a = Uuid::new_v4();
    let lease_b = Uuid::new_v4();
    let key_a = [0xAAu8; 32];
    let key_b = [0xBBu8; 32];

    let a1 = fresh_cap_id();
    let a2 = fresh_cap_id();
    let a3 = fresh_cap_id();
    let b1 = fresh_cap_id();

    {
        let mut c = caps.lock().await;
        c.insert(a1, cap_entry(key_a, lease_a));
        c.insert(a2, cap_entry(key_a, lease_a));
        c.insert(a3, cap_entry(key_a, lease_a));
        c.insert(b1, cap_entry(key_b, lease_b));
    }
    assert_eq!(caps_len(&caps).await, 4);

    let evicted = evict_caps_from_map(&caps, lease_a).await;
    assert_eq!(evicted, 3, "must evict exactly the lease_a cohort");
    assert_eq!(caps_len(&caps).await, 1);

    let surviving: Vec<Uuid> = caps
        .lock()
        .await
        .values()
        .map(|entry| entry.lease_id)
        .collect();
    assert_eq!(surviving, vec![lease_b]);
}

#[tokio::test]
async fn evict_caps_for_lease_is_idempotent_on_repeat_call() {
    let caps = make_caps();
    let lease = Uuid::new_v4();
    let key = [0xDDu8; 32];

    {
        let mut c = caps.lock().await;
        c.insert(fresh_cap_id(), cap_entry(key, lease));
        c.insert(fresh_cap_id(), cap_entry(key, lease));
    }
    assert_eq!(caps_len(&caps).await, 2);

    let first = evict_caps_from_map(&caps, lease).await;
    let second = evict_caps_from_map(&caps, lease).await;
    assert_eq!(first, 2);
    assert_eq!(
        second, 0,
        "repeat eviction must report zero additional removals"
    );
    assert_eq!(caps_len(&caps).await, 0);
}

#[tokio::test]
async fn caps_with_expired_entries_are_still_evicted_by_lease_cohort() {
    // The cohort eviction is keyed on lease_id, not on cap expiry —
    // an already-expired cap whose lease just got revoked still
    // needs to fall out of the map so it can't be re-issued by a
    // pathological caller that stuffs it back into the map.
    let caps = make_caps();
    let lease = Uuid::new_v4();
    let key = [0xFFu8; 32];
    let already_expired = CapEntry {
        cache_key: key,
        namespace: "ns".to_string(),
        plugin: "plugin".to_string(),
        expires_at: Instant::now() - Duration::from_secs(1),
        lease_id: lease,
    };
    caps.lock().await.insert(fresh_cap_id(), already_expired);
    assert_eq!(caps_len(&caps).await, 1);

    let evicted = evict_caps_from_map(&caps, lease).await;
    assert_eq!(evicted, 1);
    assert_eq!(caps_len(&caps).await, 0);
}

/// Issue #146 acceptance: zero `lease_id: None` in `auth-broker/src/`.
///
/// The round 1b cutover collapses `CapEntry::lease_id` from
/// `Option<Uuid>` to `Uuid`. A future refactor that re-introduces
/// the optional shape (e.g. resurrecting the legacy
/// bearer-as-vault-password path on the side) MUST trip this test.
///
/// Grep-gated: walks `auth-broker/src` looking for the literal text
/// `lease_id: None` and fails if any match is found.
#[test]
fn no_lease_id_none_in_broker_src() {
    let mut hits = Vec::new();
    let src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    visit(&src, &mut hits);

    if !hits.is_empty() {
        panic!(
            "found {} `lease_id: None` occurrence(s) in auth-broker/src — round 1b cutover \
             eliminated the optional shape (see issue #146 acceptance criteria); offending \
             locations:\n  {}",
            hits.len(),
            hits.join("\n  ")
        );
    }

    fn visit(dir: &std::path::Path, hits: &mut Vec<String>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, hits);
                continue;
            }
            if path.extension().map(|x| x != "rs").unwrap_or(true) {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for (idx, line) in content.lines().enumerate() {
                if line.contains("lease_id: None") {
                    hits.push(format!("{}:{}: {}", path.display(), idx + 1, line.trim()));
                }
            }
        }
    }
}
