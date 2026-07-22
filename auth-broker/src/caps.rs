use std::collections::HashMap;
use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::{rngs::SysRng, TryRng};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use uuid::Uuid;

pub const CAP_TTL: Duration = Duration::from_secs(60);
pub const CAP_BYTES: usize = 32;

/// A minted capability bound at `auth/check` time to the
/// `(cache_key, namespace, plugin, lease_id)` quad captured from
/// `x-envoy-original-path` plus the originating lease row.
///
/// Round 1b removes the legacy bearer-as-vault-password path, so
/// `lease_id` is now a required field: every cap is part of exactly
/// one lease cohort and [`super::cache::evict_caps_for_lease`] is
/// the only eviction helper the broker carries (the legacy
/// `evict_caps_for_map` helper from round 1a is gone).
///
/// `plugin` continues to gate `allowed_consumers` in
/// `secrets/fetch`; `namespace` is carried for audit + routing
/// parity with the rest of the system and is *not* part of the ACL
/// match today. Both are immutable for the cap's lifetime so a
/// stolen cap cannot pivot to a different namespace or plugin.
#[derive(Clone)]
pub struct CapEntry {
    pub cache_key: [u8; 32],
    pub namespace: String,
    pub plugin: String,
    pub expires_at: Instant,
    /// Lease row this cap was minted from. Round 1b collapses the
    /// previous `Option<Uuid>` into a plain `Uuid` because the
    /// legacy path that minted `None` caps is gone.
    pub lease_id: Uuid,
}

pub type CapId = [u8; CAP_BYTES];
pub type CapMap = Arc<Mutex<HashMap<CapId, CapEntry>>>;

pub fn mint_cap_id() -> CapId {
    let mut buf = [0u8; CAP_BYTES];
    let mut rng = SysRng;
    rng.try_fill_bytes(&mut buf)
        .expect("SysRng should be available");
    buf
}

pub fn encode_cap(id: &CapId) -> String {
    URL_SAFE_NO_PAD.encode(id)
}

pub fn decode_cap(s: &str) -> Option<CapId> {
    let decoded = URL_SAFE_NO_PAD.decode(s).ok()?;
    let decoded: [u8; CAP_BYTES] = decoded.try_into().ok()?;
    Some(decoded)
}

pub fn cap_is_expired(entry: &CapEntry, now: Instant) -> bool {
    now >= entry.expires_at
}
