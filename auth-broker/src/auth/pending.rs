//! `auth::pending` — in-process map of in-flight OPAQUE login
//! handshakes.
//!
//! ## Why it's in-process
//!
//! `/auth/login/start` returns the server's [`LoginResponse`] to the
//! client and stashes the matching [`ServerLoginState`] (plus the
//! tenant id + requested lease window) here, keyed by a freshly-
//! minted UUID v4 handshake id.
//! `/auth/login/finish` consumes the entry by handshake id, deserialises
//! the state, and feeds it into [`server::login_finish`] alongside the
//! client's `LoginFinalization`. The state is `take`n on lookup —
//! handshakes are single-use, so repeats and concurrent finishes for
//! the same id intentionally lose one side.
//!
//! The map is **deliberately ephemeral** — broker restart drops every
//! in-flight handshake. That's the right posture: any client whose
//! `start → finish` straddled a restart already lost the wire round-
//! trip and will get a fresh `start` on retry. Persistence would
//! widen the postgres surface for zero functional gain.
//!
//! ## TTL
//!
//! [`PENDING_TTL`] caps the time between `start` and `finish` at 60
//! seconds; older entries are evicted by [`PendingMap::take`] on
//! the next lookup and by [`PendingMap::sweep`] when the janitor
//! runs. The TTL is intentionally short — the round-trip is one
//! pair of HTTP requests over a healthy connection, not a human-
//! typed credential challenge — so a long TTL would only widen the
//! window an attacker has to replay a stolen handshake_id.
//!
//! ## Concurrency
//!
//! Single `tokio::sync::Mutex` around an inner `HashMap`. The
//! per-request work inside the lock is `O(1)` (a hash lookup + a
//! couple of memcopies) so contention is not a concern at any
//! plausible per-process login rate. If profiling ever says
//! otherwise, sharding by handshake_id prefix is the obvious next
//! step.

use std::collections::HashMap;
use std::sync::Arc;

use botwork_opaque_handshake::ServerLoginState;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use uuid::Uuid;
use zeroize::Zeroizing;

/// Maximum age of an in-flight handshake. Older entries are evicted
/// on the next lookup or sweep.
pub const PENDING_TTL: Duration = Duration::from_secs(60);

/// Errors returned by the pending-handshake map. Three deliberately
/// distinct arms so the HTTP handler can pick the right structured-
/// 401 / 404 / 500 mapping at the seam — but the *client* sees the
/// same 404 for all three (\"unknown / expired handshake_id\") to
/// avoid leaking which arm tripped.
#[derive(Debug, thiserror::Error)]
pub enum PendingError {
    /// No entry exists for the supplied handshake id. Either the
    /// client typo'd the id, the id was already consumed by a
    /// previous `finish`, or the broker restarted between `start`
    /// and `finish`.
    #[error("no pending handshake with the supplied id")]
    NotFound,
    /// An entry existed but was older than [`PENDING_TTL`] — the
    /// client waited too long between `start` and `finish`. Same
    /// HTTP shape as `NotFound`.
    #[error("pending handshake expired")]
    Expired,
    /// A second `insert` raced with the first on the same UUID.
    /// Vanishingly unlikely (UUIDv4 with 122 bits of entropy) but
    /// still pinned by [`tests::insert_rejects_collision`] so a
    /// future move to a shorter id surface trips this test.
    #[error("pending handshake id collided with an existing entry")]
    Collision,
}

/// Entry payload stored alongside the handshake id. Holds everything
/// `/auth/login/finish` needs to mint a lease without re-reading the
/// HTTP request body.
pub struct Pending {
    /// Captured at `start` time so `finish` is read-only on the
    /// caller's request body.
    ///
    /// `Some(tenant.id)` for known tenants that successfully matched
    /// a `tenant` row at `/auth/login/start` time. `None` for the
    /// **OPAQUE dummy flow** — unknown tenants and known tenants
    /// without a `password_file` row both go through the dummy path
    /// so the wire shape of `/auth/login/start` is constant-time
    /// against tenant enumeration. `/auth/login/finish` then refuses
    /// to mint a lease for `None` and returns the same structured
    /// `InvalidBearer` 401 a wrong-password against a real tenant
    /// produces.
    ///
    /// Using `Option<Uuid>` rather than a `Uuid::nil()` sentinel
    /// keeps the dummy-flow detection in the type system instead of
    /// in domain-value space — a refactor that drops the sentinel
    /// check breaks compilation rather than silently leaking
    /// tenant existence.
    pub tenant_id: Option<Uuid>,
    /// The OPAQUE server-side state to feed into
    /// [`server::login_finish`].
    pub state: ServerLoginState,
    /// Client-requested lease window, capped server-side at
    /// `finish` time. Carried here so the client doesn't get a
    /// chance to flip the value between the two round-trips.
    pub lease_seconds_requested: u64,
    /// Wall-clock used by [`PendingMap::sweep`] to evict idle
    /// entries.
    pub created_at: Instant,
}

/// Type returned to `/auth/login/finish` callers. The state is
/// already deserialised back into a typed [`ServerLoginState`] so
/// the handler doesn't see the raw bytes at all.
///
/// `tenant_id` is `None` when the entry came in via the OPAQUE
/// dummy flow at `/auth/login/start` time — see [`Pending::tenant_id`]
/// for the enumeration-resistance rationale.
#[derive(Debug)]
pub struct PendingTaken {
    pub tenant_id: Option<Uuid>,
    pub state: ServerLoginState,
    pub lease_seconds_requested: u64,
}

/// In-process map of pending handshakes. Cheap to `clone()` — the
/// inner `Arc<Mutex<HashMap>>` is the actual storage.
#[derive(Clone, Default)]
pub struct PendingMap {
    inner: Arc<Mutex<HashMap<Uuid, PendingEntry>>>,
}

/// Internal storage shape. The state is held in serialised form
/// (`Zeroizing<Vec<u8>>`) rather than as a typed
/// [`ServerLoginState`] so the bytes are wiped from memory the
/// moment the entry is dropped, exactly mirroring the on-the-wire
/// posture from [`ServerLoginState::to_bytes`].
struct PendingEntry {
    tenant_id: Option<Uuid>,
    state_bytes: Zeroizing<Vec<u8>>,
    lease_seconds_requested: u64,
    created_at: Instant,
}

impl PendingMap {
    /// Construct an empty map. Equivalent to `PendingMap::default()`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a fresh pending entry. The supplied
    /// [`ServerLoginState`] is serialised via
    /// [`ServerLoginState::to_bytes`] before storage so the typed
    /// state object can be dropped after this call.
    ///
    /// Returns [`PendingError::Collision`] if `handshake_id` is
    /// already present. The HTTP handler treats that as a 500
    /// (internal: UUIDv4 collision means our RNG is broken).
    pub async fn insert(&self, handshake_id: Uuid, entry: Pending) -> Result<(), PendingError> {
        let stored = PendingEntry {
            tenant_id: entry.tenant_id,
            state_bytes: entry.state.to_bytes(),
            lease_seconds_requested: entry.lease_seconds_requested,
            created_at: entry.created_at,
        };
        let mut guard = self.inner.lock().await;
        // Use the entry API so the existence check + insert are a
        // single atomic step under the lock; using contains_key +
        // insert separately would still be correct (we hold the
        // lock the whole time) but the entry shape reads as the
        // direct intent.
        match guard.entry(handshake_id) {
            std::collections::hash_map::Entry::Occupied(_) => Err(PendingError::Collision),
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(stored);
                Ok(())
            }
        }
    }

    /// Look up and remove the entry for `handshake_id`. Returns
    /// [`PendingError::NotFound`] if the id is unknown, or
    /// [`PendingError::Expired`] if the entry has aged past
    /// [`PENDING_TTL`] (the entry is also removed in that case).
    ///
    /// Single-shot: a successful `take` consumes the entry, so a
    /// second `finish` with the same id sees `NotFound`.
    pub async fn take(
        &self,
        handshake_id: Uuid,
        now: Instant,
    ) -> Result<PendingTaken, PendingError> {
        let mut guard = self.inner.lock().await;
        let entry = guard.remove(&handshake_id).ok_or(PendingError::NotFound)?;
        if now.saturating_duration_since(entry.created_at) > PENDING_TTL {
            // Already removed above; nothing else to clean up.
            return Err(PendingError::Expired);
        }
        let state = ServerLoginState::from_bytes(&entry.state_bytes).map_err(|_| {
            // A round-tripped state failing to deserialise would
            // mean the bytes were corrupted in-memory between
            // insert and take — there is no path that should make
            // that happen. Surface as NotFound so the wire-visible
            // shape stays uniform with the other failures.
            PendingError::NotFound
        })?;
        Ok(PendingTaken {
            tenant_id: entry.tenant_id,
            state,
            lease_seconds_requested: entry.lease_seconds_requested,
        })
    }

    /// Evict every entry older than [`PENDING_TTL`]. The handler
    /// drives this from a tokio task on a 30s tick alongside the
    /// existing cache pruner; can also be called from tests via
    /// `tokio::time::advance` to deterministically expire entries.
    pub async fn sweep(&self, now: Instant) -> usize {
        let mut guard = self.inner.lock().await;
        let before = guard.len();
        guard.retain(|_, entry| now.saturating_duration_since(entry.created_at) <= PENDING_TTL);
        before.saturating_sub(guard.len())
    }

    /// Length of the map. Test-only — the production handlers
    /// never need to ask.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn len(&self) -> usize {
        let guard = self.inner.lock().await;
        guard.len()
    }

    /// Companion to [`Self::len`]. Same test-only posture;
    /// exists so `#[deny(clippy::len_without_is_empty)]` is happy
    /// in the strict CI gate.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use botwork_opaque_handshake::{client, server, ServerSetup};

    fn synth_state() -> ServerLoginState {
        // Build a real, deserialisable ServerLoginState through the
        // PAKE rather than synthesising garbage bytes — the take()
        // path round-trips through ServerLoginState::from_bytes, so
        // garbage would surface as PendingError::NotFound and mask
        // the actual contract we want to pin.
        let mut rng = rand::thread_rng();
        let setup = ServerSetup::generate(&mut rng);
        let cred = b"pending-test@example.com";
        let cr = client::registration_start(&mut rng, b"pw").unwrap();
        let sr = server::registration_start(&setup, cr.request, cred).unwrap();
        let cf = client::registration_finish(&mut rng, cr.state, b"pw", sr.response).unwrap();
        let pf = server::registration_finish(cf.upload);
        let cl = client::login_start(&mut rng, b"pw").unwrap();
        let sl = server::login_start(&mut rng, &setup, Some(&pf), cl.request, cred).unwrap();
        sl.state
    }

    fn tenant() -> Option<Uuid> {
        Some(Uuid::new_v4())
    }

    #[tokio::test(start_paused = true)]
    async fn insert_and_take_round_trips() {
        let map = PendingMap::new();
        let id = Uuid::new_v4();
        map.insert(
            id,
            Pending {
                tenant_id: tenant(),
                state: synth_state(),
                lease_seconds_requested: 604800,
                created_at: Instant::now(),
            },
        )
        .await
        .unwrap();

        assert_eq!(map.len().await, 1);
        let taken = map.take(id, Instant::now()).await.unwrap();
        assert_eq!(taken.lease_seconds_requested, 604800);
        assert_eq!(map.len().await, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn take_is_single_shot() {
        let map = PendingMap::new();
        let id = Uuid::new_v4();
        map.insert(
            id,
            Pending {
                tenant_id: tenant(),
                state: synth_state(),
                lease_seconds_requested: 30,
                created_at: Instant::now(),
            },
        )
        .await
        .unwrap();

        map.take(id, Instant::now()).await.unwrap();
        let err = map.take(id, Instant::now()).await.unwrap_err();
        assert!(matches!(err, PendingError::NotFound), "got {err:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn take_after_ttl_returns_expired() {
        let map = PendingMap::new();
        let id = Uuid::new_v4();
        let now = Instant::now();
        map.insert(
            id,
            Pending {
                tenant_id: tenant(),
                state: synth_state(),
                lease_seconds_requested: 30,
                created_at: now,
            },
        )
        .await
        .unwrap();

        let later = now + PENDING_TTL + Duration::from_secs(1);
        let err = map.take(id, later).await.unwrap_err();
        assert!(matches!(err, PendingError::Expired), "got {err:?}");
        // The entry is consumed by the failed take so a follow-up
        // take sees NotFound rather than Expired again.
        let err = map.take(id, later).await.unwrap_err();
        assert!(matches!(err, PendingError::NotFound), "got {err:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn sweep_evicts_idle_entries() {
        let map = PendingMap::new();
        let now = Instant::now();
        for _ in 0..3 {
            map.insert(
                Uuid::new_v4(),
                Pending {
                    tenant_id: tenant(),
                    state: synth_state(),
                    lease_seconds_requested: 30,
                    created_at: now,
                },
            )
            .await
            .unwrap();
        }
        // Also insert a still-fresh entry that must survive.
        let fresh = Uuid::new_v4();
        map.insert(
            fresh,
            Pending {
                tenant_id: tenant(),
                state: synth_state(),
                lease_seconds_requested: 30,
                created_at: now + PENDING_TTL,
            },
        )
        .await
        .unwrap();

        let evicted = map.sweep(now + PENDING_TTL + Duration::from_secs(1)).await;
        assert_eq!(evicted, 3);
        assert_eq!(map.len().await, 1);
        // The fresh entry's `created_at` is also now+PENDING_TTL, and
        // sweep removed everything *older than* PENDING_TTL — so the
        // fresh one survives the sweep, then a follow-up take with
        // a wall clock past its own TTL would expire it.
        let _ = map
            .take(fresh, now + PENDING_TTL + Duration::from_secs(2))
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn insert_rejects_collision() {
        // UUIDv4 collision is vanishingly unlikely (122 bits), but
        // the contract is pinned here so a future move to a
        // shorter id surface (truncated hash, monotonic counter,
        // etc.) trips a test rather than silently overwriting.
        let map = PendingMap::new();
        let id = Uuid::new_v4();
        map.insert(
            id,
            Pending {
                tenant_id: tenant(),
                state: synth_state(),
                lease_seconds_requested: 30,
                created_at: Instant::now(),
            },
        )
        .await
        .unwrap();
        let err = map
            .insert(
                id,
                Pending {
                    tenant_id: tenant(),
                    state: synth_state(),
                    lease_seconds_requested: 30,
                    created_at: Instant::now(),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, PendingError::Collision), "got {err:?}");
    }

    #[tokio::test]
    async fn is_empty_on_empty_map() {
        let map = PendingMap::new();
        assert!(map.is_empty().await);
    }

    #[tokio::test(start_paused = true)]
    async fn is_empty_on_nonempty_map() {
        let map = PendingMap::new();
        map.insert(
            Uuid::new_v4(),
            Pending {
                tenant_id: tenant(),
                state: synth_state(),
                lease_seconds_requested: 30,
                created_at: Instant::now(),
            },
        )
        .await
        .unwrap();
        assert!(!map.is_empty().await);
    }

    /// Exercise the `map_err(|_| PendingError::NotFound)` closure in
    /// `take()`. That closure runs when `ServerLoginState::from_bytes`
    /// fails, which cannot happen through the public `insert()` API (which
    /// always stores bytes produced by `state.to_bytes()`). We reach it by
    /// directly injecting a `PendingEntry` with corrupted bytes into the
    /// inner map, which is accessible from this in-file test module.
    #[tokio::test]
    async fn take_with_corrupted_state_bytes_returns_not_found() {
        let map = PendingMap::new();
        let id = Uuid::new_v4();

        // Bypass the public insert() API: write a PendingEntry whose
        // state_bytes are not a valid serialised ServerLoginState.
        let bad_entry = PendingEntry {
            tenant_id: Some(Uuid::new_v4()),
            state_bytes: Zeroizing::new(vec![0xFFu8; 8]),
            lease_seconds_requested: 30,
            created_at: Instant::now(),
        };
        map.inner.lock().await.insert(id, bad_entry);

        let err = map
            .take(id, Instant::now())
            .await
            .expect_err("corrupted bytes must surface as PendingError");
        assert!(
            matches!(err, PendingError::NotFound),
            "expected PendingError::NotFound for corrupted state bytes, got {err:?}"
        );
    }
}
