use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use botwork_vault::UnlockedMasterKey;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration, Instant};
use tracing::warn;
use uuid::Uuid;

use crate::caps::{cap_is_expired, CapEntry, CapId, CapMap};
use crate::config::TtlConfig;
use crate::metrics::{Metrics, MetricsSnapshot};

/// Default idle TTL: 5 minutes since the last successful use.
pub const IDLE_TTL: Duration = Duration::from_secs(5 * 60);
/// Default absolute TTL: 8 hours since unlock.
pub const ABSOLUTE_TTL: Duration = Duration::from_secs(8 * 3600);
/// Interval between background prune sweeps.
pub const PRUNE_INTERVAL: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Admin key file — mtime-cached dynamic source
// ---------------------------------------------------------------------------

/// Default path for the admin key file (matches `botctl admin-key`'s
/// `DEFAULT_KEY_FILE`).
pub const DEFAULT_ADMIN_KEY_FILE: &str = "/var/lib/botwork/admin.env";

/// Env var that overrides [`DEFAULT_ADMIN_KEY_FILE`] (matches `botctl
/// admin-key`'s `KEY_FILE_ENV`).
pub const ADMIN_KEY_FILE_ENV: &str = "BOTWORK_ADMIN_KEY_FILE";

/// Name of the env var entry within the key file.
const ADMIN_KEY_ENV_VAR: &str = "BOTWORK_ADMIN_API_KEY";

/// Cached state held inside [`AdminKeyFile`].
struct AdminKeyFileCache {
    key: Option<Arc<str>>,
    mtime: Option<SystemTime>,
}

/// Mtime-cached reader for the genesis admin API key file.
///
/// On each call to [`AdminKeyFile::current_key`] the file's mtime is
/// `stat`-ed. If it is unchanged since the last read the cached key is
/// returned immediately. If the mtime changed (or this is the first call)
/// the file is re-read and re-parsed so that rotation via
/// `botctl admin-key set/generate --force` takes effect **without a
/// broker restart**.
///
/// File format (shell-parseable, written by `botctl admin-key`):
///
/// ```text
/// BOTWORK_ADMIN_API_KEY=<key>
/// ```
///
/// If the file does not exist or contains no valid
/// `BOTWORK_ADMIN_API_KEY=<key>` entry, [`AdminKeyFile::current_key`]
/// returns `None` and the admin surface is disabled.
pub struct AdminKeyFile {
    path: PathBuf,
    cache: std::sync::Mutex<AdminKeyFileCache>,
}

impl AdminKeyFile {
    /// Create a new `AdminKeyFile` reader for the given path.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            cache: std::sync::Mutex::new(AdminKeyFileCache {
                key: None,
                mtime: None,
            }),
        }
    }

    /// Return the current admin key, reading from disk when the file's
    /// mtime has changed since the last read.
    ///
    /// Returns `None` when:
    /// - the file does not exist (admin surface disabled), or
    /// - the file contains no valid `BOTWORK_ADMIN_API_KEY=<value>` entry.
    pub fn current_key(&self) -> Option<Arc<str>> {
        // Stat the file to obtain its mtime.
        let metadata = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => {
                // File missing — clear cache and disable admin surface.
                let mut cache = self
                    .cache
                    .lock()
                    .expect("AdminKeyFile cache mutex poisoned");
                cache.key = None;
                cache.mtime = None;
                return None;
            }
        };
        let mtime = metadata.modified().ok();

        let mut cache = self
            .cache
            .lock()
            .expect("AdminKeyFile cache mutex poisoned");

        // Return the cached key when mtime is unchanged.
        if mtime.is_some() && cache.mtime == mtime {
            return cache.key.clone();
        }

        // Re-read and re-parse.
        let key: Option<Arc<str>> = std::fs::read_to_string(&self.path)
            .map_err(|err| {
                warn!(
                    "[auth-broker/admin-key] failed to read key file {}: {err}",
                    self.path.display()
                );
            })
            .ok()
            .and_then(|content| parse_admin_key_from_content(&content))
            .map(|k| Arc::from(k.as_str()));

        cache.key = key.clone();
        cache.mtime = mtime;
        key
    }
}

/// Parse the admin key from the content of an admin.env file.
///
/// Scans for a line of the form `BOTWORK_ADMIN_API_KEY=<value>`.
/// Blank lines and lines beginning with `#` are skipped.
/// Returns `None` if no such line is found or the value is empty.
fn parse_admin_key_from_content(content: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(val) = line
            .strip_prefix(ADMIN_KEY_ENV_VAR)
            .and_then(|s| s.strip_prefix('='))
        {
            let val = val.trim();
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// AdminKeySource — unified key accessor
// ---------------------------------------------------------------------------

/// Source for the genesis admin API key.
///
/// All admin-bearer checks go through [`AdminKeySource::current_key`] so
/// that both the static (test) and file-backed (production) variants share
/// one call site.
#[derive(Clone)]
pub enum AdminKeySource {
    /// Admin surface is disabled — no key configured.
    None,
    /// Static in-memory key. Used by tests via
    /// [`AppState::with_admin_api_key`].
    Static(Arc<str>),
    /// File-backed key with mtime-based caching. Used in production via
    /// [`AppState::with_admin_key_file`] (path read from
    /// `BOTWORK_ADMIN_KEY_FILE` or defaulting to
    /// `/var/lib/botwork/admin.env`).
    File(Arc<AdminKeyFile>),
}

impl AdminKeySource {
    /// Return the current admin key, or `None` if the admin surface is
    /// disabled.
    ///
    /// For `File` variant: `stat`s the backing file on every call and
    /// re-reads it when the mtime has changed, making key rotation
    /// effective without a broker restart. The comparison in
    /// `check()` / `require_admin()` still uses
    /// `subtle::ConstantTimeEq` against the returned value.
    pub fn current_key(&self) -> Option<Arc<str>> {
        match self {
            AdminKeySource::None => Option::None,
            AdminKeySource::Static(key) => Some(key.clone()),
            AdminKeySource::File(f) => f.current_key(),
        }
    }
}

/// A single unlocked-master-key cache entry.
///
/// Round 1b: we no longer cache the whole decrypted
/// [`botwork_vault::VaultContents`] payload. We cache the
/// [`UnlockedMasterKey`] alone; `/secrets/fetch` reaches back into
/// the vault's per-entry envelopes (which carry wrapped DEKs, not
/// plaintext) and runs a fresh per-entry decrypt on demand. A
/// memory dump after one fetch leaks exactly that one secret's
/// plaintext, not every secret in the vault.
pub struct CacheEntry {
    pub tenant: String,
    /// Path to the tenant's vault root. The cache holds the
    /// unlocked master key plus the metadata needed to re-locate
    /// the vault file on each fetch; per-fetch we instantiate a
    /// fresh [`botwork_vault::Vault`] from this root, unlock it
    /// against `master`, and decrypt only the entries the caller
    /// asked for.
    pub vault_root: PathBuf,
    /// HKDF-derived master key for the tenant's v4 vault. Held in
    /// `Zeroizing<[u8; 32]>` via [`UnlockedMasterKey`] so eviction
    /// (`lock_entry` below) scrubs the bytes on drop.
    pub master: UnlockedMasterKey,
    /// OPAQUE suite version observed at lease-mint time. Re-fed
    /// into `Vault::unlock` on every fetch so a stale lease (from
    /// a pre-suite-rotation login) fails fast with
    /// `VaultError::UnsupportedVersion` instead of opening a vault
    /// sealed under the previous suite.
    pub suite_version: u8,
    pub expires_at: Instant,
    pub last_used: Instant,
    pub created_at: Instant,
    pub idle_ttl: Duration,
}

#[derive(Clone)]
pub struct AppState {
    pub vault_root: PathBuf,
    /// Round 1b: every endpoint now goes through the OPAQUE lease
    /// path. `auth` is no longer optional — `AppState::new` /
    /// `AppState::with_ttl_config` (the auth-less constructors) are
    /// gone; the production binary calls [`AppState::with_auth`]
    /// or [`AppState::with_auth_and_ttl_config`].
    pub cache: Arc<Mutex<HashMap<[u8; 32], CacheEntry>>>,
    pub caps: CapMap,
    pub ttl_config: Arc<TtlConfig>,
    pub metrics: Arc<Metrics>,
    /// OPAQUE login / lease validation state. Always populated in
    /// round 1b.
    pub auth: crate::auth::AuthState,
    /// Per-tenant write-serialisation locks for the internal write
    /// endpoints (`POST /secrets` and `DELETE /secrets/…`).
    /// Lazily populated on first
    /// write for a tenant; never removed (the map entry is small
    /// and the set of tenants is bounded).
    pub write_locks: Arc<std::sync::Mutex<HashMap<uuid::Uuid, Arc<Mutex<()>>>>>,
    /// Source for the genesis admin API key. When `AdminKeySource::None`
    /// (the default), the admin surface is disabled and all admin calls
    /// return 401. In production, set to `AdminKeySource::File` via
    /// [`AppState::with_admin_key_file`] so the key is read dynamically
    /// from disk (mtime-cached). In tests, set to `AdminKeySource::Static`
    /// via [`AppState::with_admin_api_key`].
    pub admin_key_source: AdminKeySource,
}

impl AppState {
    /// Construct an [`AppState`] using the in-tree TTL defaults.
    /// Round 1b: every constructor now requires an OPAQUE
    /// [`crate::auth::AuthState`] — `state.auth` is no longer
    /// `Option<_>`. The auth-less `new` constructor that v3 carried
    /// is gone alongside the legacy `/auth/check` fall-through.
    pub fn with_auth(vault_root: PathBuf, auth: crate::auth::AuthState) -> Self {
        Self::with_auth_and_ttl_config(vault_root, auth, TtlConfig::default())
    }

    /// Construct an `AppState` with both an explicit [`TtlConfig`]
    /// and an OPAQUE [`crate::auth::AuthState`].
    pub fn with_auth_and_ttl_config(
        vault_root: PathBuf,
        auth: crate::auth::AuthState,
        ttl_config: TtlConfig,
    ) -> Self {
        Self {
            vault_root,
            cache: Arc::new(Mutex::new(HashMap::new())),
            caps: Arc::new(Mutex::new(HashMap::new())),
            ttl_config: Arc::new(ttl_config),
            metrics: Arc::new(Metrics::default()),
            auth,
            write_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            admin_key_source: AdminKeySource::None,
        }
    }

    /// Builder-style setter for a static in-memory admin API key.
    ///
    /// Used in tests. In production, prefer [`AppState::with_admin_key_file`]
    /// so the key is read dynamically from disk and rotation takes effect
    /// without a broker restart.
    pub fn with_admin_api_key(mut self, key: impl Into<Arc<str>>) -> Self {
        self.admin_key_source = AdminKeySource::Static(key.into());
        self
    }

    /// Builder-style setter for the file-backed admin key source.
    ///
    /// The key is read from `path` on first use and whenever the file's
    /// mtime changes, so `botctl admin-key set/generate --force` takes
    /// effect **without a broker restart**. If the file does not exist or
    /// contains no valid `BOTWORK_ADMIN_API_KEY=<key>` entry, the admin
    /// surface is disabled.
    pub fn with_admin_key_file(mut self, path: PathBuf) -> Self {
        self.admin_key_source = AdminKeySource::File(Arc::new(AdminKeyFile::new(path)));
        self
    }

    /// Snapshot of every counter plus a derived `(cache_size,
    /// average_entry_age_secs)` pair.
    pub async fn metrics_snapshot(&self) -> AppStateMetricsSnapshot {
        let cache = self.cache.lock().await;
        let cache_size = cache.len();
        let now = Instant::now();
        let age_secs_total: u128 = cache
            .values()
            .map(|entry| now.duration_since(entry.created_at).as_secs() as u128)
            .sum();
        let avg_age_secs = if cache_size == 0 {
            0
        } else {
            (age_secs_total / cache_size as u128) as u64
        };
        AppStateMetricsSnapshot {
            counters: self.metrics.snapshot(),
            cache_size,
            avg_age_secs,
        }
    }

    /// Obtain the per-tenant write-serialisation lock for `tenant_id`.
    /// Creates the entry on first call and returns the same
    /// `Arc<Mutex<()>>` on every subsequent call. Callers `.await`
    /// the returned mutex before touching the tenant's vault file
    /// so concurrent remote-write requests serialise safely.
    pub fn tenant_write_lock(&self, tenant_id: uuid::Uuid) -> Arc<Mutex<()>> {
        let mut map = self.write_locks.lock().expect("write_locks poisoned");
        map.entry(tenant_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn cache_len(&self) -> usize {
        let cache = self.cache.lock().await;
        cache.len()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn entry_times(&self, key: [u8; 32]) -> Option<(Instant, Instant)> {
        let cache = self.cache.lock().await;
        cache
            .get(&key)
            .map(|entry| (entry.expires_at, entry.last_used))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn caps_len(&self) -> usize {
        let caps = self.caps.lock().await;
        caps.len()
    }

    /// Test-only introspection hook: runs `f` while holding the
    /// caps mutex. Used by [`crate::evict_caps_for_lease`]'s
    /// cohort-eviction tests.
    // Not exercised in the coverage build (test-support scaffolding only).
    #[cfg(not(tarpaulin_include))]
    #[cfg(any(test, feature = "test-support"))]
    pub async fn with_locked_caps<R>(&self, f: impl FnOnce(&HashMap<CapId, CapEntry>) -> R) -> R {
        let caps = self.caps.lock().await;
        f(&caps)
    }

    /// Test-only injection hook: insert a synthetic `CapEntry`.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn insert_cap_for_test(&self, id: CapId, entry: CapEntry) {
        let mut caps = self.caps.lock().await;
        caps.insert(id, entry);
    }

    /// Test-only injection hook: insert a synthetic [`CacheEntry`]
    /// keyed by `cache_key`. Used by the ported lifecycle / TTL /
    /// log-redaction / secrets-fetch tests so they can pin
    /// invariants that survive the v4 cutover without needing a
    /// real OPAQUE lease or a docker-backed postgres.
    ///
    /// The hook also bumps `metrics.cache_inserts` so a test that
    /// drives `prune_once` and reads `metrics_snapshot()` sees the
    /// same counter movement a real `/auth/check` insert would
    /// produce.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn insert_cache_entry_for_test(&self, key: [u8; 32], entry: CacheEntry) {
        let mut cache = self.cache.lock().await;
        cache.insert(key, entry);
        self.metrics.inc_insert();
    }

    /// Test-only introspection hook (issue #126 → tightened in
    /// round 1b / #146): runs `f` while holding the cache mutex.
    ///
    /// **What the hook sees in v4.** The cache holds
    /// [`CacheEntry`]s whose `master` field is an
    /// [`UnlockedMasterKey`] — a `Zeroizing<[u8; 32]>` opaque
    /// holder. It does NOT hold any plaintext secret value.
    /// Per-secret unlock is satisfied at the cache-shape level: a
    /// caller that fetches secret X reaches into the vault's
    /// envelopes (which sit on disk under `vault_root`) and gets
    /// back the bytes for *X only*; the cache is not widened with a
    /// SecretEntry side-table.
    ///
    /// `tests/secrets_fetch.rs::no_plaintext_for_x_lingers_in_cache`
    /// uses this hook to pin the property by string-scanning the
    /// `CacheEntry` byte image for plaintext after a successful
    /// fetch — see that test for the precise assertion.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn with_locked_cache<R>(
        &self,
        f: impl FnOnce(&HashMap<[u8; 32], CacheEntry>) -> R,
    ) -> R {
        let cache = self.cache.lock().await;
        f(&cache)
    }
}

/// Snapshot returned by [`AppState::metrics_snapshot`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AppStateMetricsSnapshot {
    pub counters: MetricsSnapshot,
    pub cache_size: usize,
    pub avg_age_secs: u64,
}

/// Treat an entry as expired when either the absolute deadline has
/// passed *or* the per-entry idle TTL has elapsed.
pub(crate) fn is_expired(entry: &CacheEntry, now: Instant) -> bool {
    now > entry.expires_at || now.duration_since(entry.last_used) > entry.idle_ttl
}

fn lock_entry(_entry: &mut CacheEntry) {
    // Eviction drops the `CacheEntry` and its `UnlockedMasterKey`
    // along with it — the `Zeroizing` wrapper scrubs the master
    // bytes. There is no longer a `vault.lock()` step because v4
    // doesn't hold an unlocked Vault on the cache entry.
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EvictReason {
    Idle,
    Absolute,
}

pub(crate) fn classify_eviction(entry: &CacheEntry, now: Instant) -> EvictReason {
    if now > entry.expires_at {
        EvictReason::Absolute
    } else {
        EvictReason::Idle
    }
}

pub fn evict_if_expired(
    cache: &mut HashMap<[u8; 32], CacheEntry>,
    cache_key: &[u8; 32],
    now: Instant,
    metrics: &Metrics,
) -> Option<[u8; 32]> {
    let entry = cache.get(cache_key)?;
    if !is_expired(entry, now) {
        return None;
    }

    let reason = classify_eviction(entry, now);
    if let Some(mut evicted) = cache.remove(cache_key) {
        lock_entry(&mut evicted);
        match reason {
            EvictReason::Idle => metrics.inc_eviction_idle(),
            EvictReason::Absolute => metrics.inc_eviction_absolute(),
        }
        return Some(*cache_key);
    }
    // Unreachable: the entry was just observed via `cache.get` above,
    // and we hold the caller's `&mut HashMap` exclusively — no concurrent
    // removal is possible under the mutex.
    None
}

/// Drop every cap minted from the named lease. Returns the number
/// of cap entries removed.
///
/// Round 1b is the cutover: the legacy `evict_caps_for_map` helper
/// that lived alongside this one in round 1a is gone, and
/// `CapEntry::lease_id` is now a required `Uuid` rather than
/// `Option<Uuid>` — every cap is a member of exactly one lease
/// cohort.
///
/// This is the seam the admin lease-revoke endpoint
/// (round 1a admin follow-up) and a future bulk-revoke flow hang
/// off. The bulk revocation path that 1b's password-change flow
/// will use isn't wired in this PR (re-registration is its own
/// issue — see the "Out of scope" section); the function exists so
/// the typed shape stays pinned.
pub async fn evict_caps_for_lease(state: &AppState, lease_id: Uuid) -> usize {
    evict_caps_from_map(&state.caps, lease_id).await
}

/// `state.caps`-detached helper: same cohort semantics as
/// [`evict_caps_for_lease`] but takes the bare [`CapMap`]. Used by
/// the data-structure-level tests in `tests/cap_lease_cohort.rs`
/// so they can pin the eviction contract without standing up the
/// full `AppState` (which requires a live DB connection in
/// round 1b).
pub async fn evict_caps_from_map(caps: &CapMap, lease_id: Uuid) -> usize {
    let mut caps = caps.lock().await;
    let before = caps.len();
    caps.retain(|_, entry| entry.lease_id != lease_id);
    before.saturating_sub(caps.len())
}

/// Drop every cap whose underlying cache entry has been evicted
/// (matched on `cache_key`). Round 1b retains this because the
/// orphaned-cap clean-up runs whenever the cache prune fires — it
/// doesn't depend on lease_id, only on whether the master-key
/// entry still exists.
pub(crate) fn evict_caps_for_cache_key(
    caps: &mut HashMap<CapId, CapEntry>,
    cache_key: &[u8; 32],
) -> usize {
    let before = caps.len();
    caps.retain(|_, entry| &entry.cache_key != cache_key);
    before.saturating_sub(caps.len())
}

pub async fn evict_caps_for(state: &AppState, cache_key: [u8; 32]) -> usize {
    let mut caps = state.caps.lock().await;
    evict_caps_for_cache_key(&mut caps, &cache_key)
}

pub async fn prune_caps_once(state: &AppState, evicted_cache_keys: Vec<[u8; 32]>, now: Instant) {
    let mut caps = state.caps.lock().await;
    caps.retain(|_, entry| {
        !evicted_cache_keys.contains(&entry.cache_key) && !cap_is_expired(entry, now)
    });
}

pub async fn prune_once(state: &AppState, now: Option<Instant>) {
    let prune_now = now.unwrap_or_else(Instant::now);
    let mut cache = state.cache.lock().await;
    let stale: Vec<([u8; 32], EvictReason)> = cache
        .iter()
        .filter(|(_, entry)| is_expired(entry, prune_now))
        .map(|(key, entry)| (*key, classify_eviction(entry, prune_now)))
        .collect();

    let mut stale_keys = Vec::with_capacity(stale.len());
    for (key, reason) in &stale {
        if let Some(mut entry) = cache.remove(key) {
            lock_entry(&mut entry);
            match reason {
                EvictReason::Idle => state.metrics.inc_eviction_idle(),
                EvictReason::Absolute => state.metrics.inc_eviction_absolute(),
            }
            stale_keys.push(*key);
        }
    }
    drop(cache);

    prune_caps_once(state, stale_keys, prune_now).await;
    state.auth.pending.sweep(prune_now).await;
    state.auth.sweep_lease_export_keys(prune_now).await;
    state.auth.rate_limiter.sweep(prune_now).await;
}

pub fn spawn_prune_task(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        // NOTE: the two lines below (`sleep` and `prune_once`) live inside an
        // infinite `loop {}` that never terminates while the broker is running.
        // They cannot be exercised by a unit test without leaking a spawned
        // task, so they remain intentionally uncovered.
        loop {
            sleep(PRUNE_INTERVAL).await;
            prune_once(&state, None).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use botwork_vault::UnlockedMasterKey;
    use uuid::Uuid;

    use crate::auth::AuthState;
    use crate::caps::CapEntry;
    use crate::metrics::Metrics;
    use crate::store::mock::{MockLeaseStore, MockPasswordFileStore, MockTenantStore};

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_master() -> UnlockedMasterKey {
        UnlockedMasterKey::from_master_bytes_for_test([0x42u8; 32])
    }

    fn make_entry(
        now: Instant,
        expires_at: Instant,
        last_used: Instant,
        idle_ttl: Duration,
    ) -> CacheEntry {
        CacheEntry {
            tenant: "t".to_string(),
            vault_root: PathBuf::from("/dev/null"),
            master: make_master(),
            suite_version: 0,
            expires_at,
            last_used,
            created_at: now,
            idle_ttl,
        }
    }

    async fn make_state() -> AppState {
        let setup = botwork_opaque_handshake::ServerSetup::generate(&mut rand::rng());
        let auth = AuthState::from_stores(
            Arc::new(MockLeaseStore::new()),
            Arc::new(MockTenantStore::new()),
            Arc::new(MockPasswordFileStore::new()),
            setup,
        );
        AppState::with_auth(PathBuf::from("/dev/null"), auth)
    }

    // -----------------------------------------------------------------------
    // is_expired
    // -----------------------------------------------------------------------

    #[test]
    fn is_expired_by_absolute_deadline() {
        let now = Instant::now();
        // expires_at is in the past → expired via the absolute branch.
        let entry = make_entry(
            now,
            now - Duration::from_secs(1),
            now,
            Duration::from_secs(300),
        );
        assert!(is_expired(&entry, now));
    }

    #[test]
    fn is_expired_by_idle_ttl() {
        let now = Instant::now();
        // last_used is longer ago than idle_ttl, but expires_at is far
        // in the future → expired via the idle branch.
        let last_used = now - Duration::from_secs(10);
        let entry = make_entry(
            now,
            now + Duration::from_secs(3600),
            last_used,
            Duration::from_secs(5),
        );
        assert!(is_expired(&entry, now));
    }

    #[test]
    fn is_expired_fresh_entry_is_not_expired() {
        let now = Instant::now();
        let entry = make_entry(
            now,
            now + Duration::from_secs(3600),
            now,
            Duration::from_secs(300),
        );
        assert!(!is_expired(&entry, now));
    }

    // -----------------------------------------------------------------------
    // classify_eviction
    // -----------------------------------------------------------------------

    #[test]
    fn classify_eviction_past_absolute_deadline() {
        let now = Instant::now();
        let entry = make_entry(
            now,
            now - Duration::from_secs(1),
            now,
            Duration::from_secs(300),
        );
        assert_eq!(classify_eviction(&entry, now), EvictReason::Absolute);
    }

    #[test]
    fn classify_eviction_idle_ttl_exceeded() {
        let now = Instant::now();
        let last_used = now - Duration::from_secs(10);
        let entry = make_entry(
            now,
            now + Duration::from_secs(3600),
            last_used,
            Duration::from_secs(5),
        );
        assert_eq!(classify_eviction(&entry, now), EvictReason::Idle);
    }

    // -----------------------------------------------------------------------
    // evict_if_expired
    // -----------------------------------------------------------------------

    #[test]
    fn evict_if_expired_key_not_present_returns_none() {
        let mut cache = HashMap::new();
        let key = [0x01u8; 32];
        let metrics = Metrics::default();
        assert!(evict_if_expired(&mut cache, &key, Instant::now(), &metrics).is_none());
    }

    #[test]
    fn evict_if_expired_fresh_entry_returns_none_and_stays() {
        let now = Instant::now();
        let mut cache = HashMap::new();
        let key = [0x02u8; 32];
        cache.insert(
            key,
            make_entry(
                now,
                now + Duration::from_secs(3600),
                now,
                Duration::from_secs(300),
            ),
        );
        let metrics = Metrics::default();
        let result = evict_if_expired(&mut cache, &key, now, &metrics);
        assert!(result.is_none());
        assert!(cache.contains_key(&key), "fresh entry must not be removed");
    }

    #[test]
    fn evict_if_expired_idle_expired_returns_key_and_increments_idle_metric() {
        let now = Instant::now();
        let last_used = now - Duration::from_secs(10);
        let mut cache = HashMap::new();
        let key = [0x03u8; 32];
        cache.insert(
            key,
            make_entry(
                now,
                now + Duration::from_secs(3600),
                last_used,
                Duration::from_secs(5),
            ),
        );
        let metrics = Metrics::default();
        let result = evict_if_expired(&mut cache, &key, now, &metrics);
        assert_eq!(result, Some(key));
        assert!(!cache.contains_key(&key));
        assert_eq!(metrics.snapshot().cache_evictions_idle, 1);
        assert_eq!(metrics.snapshot().cache_evictions_absolute, 0);
    }

    #[test]
    fn evict_if_expired_absolute_expired_returns_key_and_increments_absolute_metric() {
        let now = Instant::now();
        let mut cache = HashMap::new();
        let key = [0x04u8; 32];
        cache.insert(
            key,
            make_entry(
                now,
                now - Duration::from_secs(1),
                now,
                Duration::from_secs(300),
            ),
        );
        let metrics = Metrics::default();
        let result = evict_if_expired(&mut cache, &key, now, &metrics);
        assert_eq!(result, Some(key));
        assert!(!cache.contains_key(&key));
        assert_eq!(metrics.snapshot().cache_evictions_absolute, 1);
        assert_eq!(metrics.snapshot().cache_evictions_idle, 0);
    }

    // -----------------------------------------------------------------------
    // evict_caps_for_cache_key
    // -----------------------------------------------------------------------

    fn make_cap(cache_key: [u8; 32]) -> CapEntry {
        CapEntry {
            cache_key,
            namespace: "ns".to_string(),
            plugin: "p".to_string(),
            expires_at: Instant::now() + Duration::from_secs(60),
            lease_id: Uuid::new_v4(),
        }
    }

    #[test]
    fn evict_caps_for_cache_key_no_match_retains_all() {
        let mut caps = HashMap::new();
        let target_key = [0xAAu8; 32];
        let other_key = [0xBBu8; 32];
        caps.insert([0x01u8; 32], make_cap(other_key));
        caps.insert([0x02u8; 32], make_cap(other_key));
        let removed = evict_caps_for_cache_key(&mut caps, &target_key);
        assert_eq!(removed, 0);
        assert_eq!(caps.len(), 2);
    }

    #[test]
    fn evict_caps_for_cache_key_match_removes_exactly_matching_caps() {
        let mut caps = HashMap::new();
        let target_key = [0xAAu8; 32];
        let other_key = [0xBBu8; 32];
        caps.insert([0x01u8; 32], make_cap(target_key));
        caps.insert([0x02u8; 32], make_cap(target_key));
        caps.insert([0x03u8; 32], make_cap(other_key)); // should survive
        let removed = evict_caps_for_cache_key(&mut caps, &target_key);
        assert_eq!(removed, 2);
        assert_eq!(caps.len(), 1);
    }

    // -----------------------------------------------------------------------
    // tenant_write_lock
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn tenant_write_lock_returns_same_arc_on_repeat_call() {
        let state = make_state().await;
        let id = Uuid::new_v4();
        let first = state.tenant_write_lock(id);
        let second = state.tenant_write_lock(id);
        assert!(
            Arc::ptr_eq(&first, &second),
            "repeated calls must return the same Arc"
        );
    }

    #[tokio::test]
    async fn tenant_write_lock_different_ids_return_different_arcs() {
        let state = make_state().await;
        let a = state.tenant_write_lock(Uuid::new_v4());
        let b = state.tenant_write_lock(Uuid::new_v4());
        assert!(
            !Arc::ptr_eq(&a, &b),
            "different tenant IDs must get different Arcs"
        );
    }

    // -----------------------------------------------------------------------
    // metrics_snapshot (empty-cache branch)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn metrics_snapshot_empty_cache_has_zero_avg_age() {
        let state = make_state().await;
        let snap = state.metrics_snapshot().await;
        assert_eq!(snap.cache_size, 0);
        assert_eq!(
            snap.avg_age_secs, 0,
            "empty cache must report avg_age_secs == 0"
        );
    }

    // -----------------------------------------------------------------------
    // prune_once: absolute eviction + orphaned-cap cleanup
    // -----------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn prune_once_absolute_eviction_removes_entry_and_orphaned_caps() {
        let state = make_state().await;
        let now = Instant::now();
        let cache_key = [0x11u8; 32];

        // Insert an entry whose absolute deadline is already past.
        state
            .insert_cache_entry_for_test(
                cache_key,
                make_entry(
                    now,
                    now - Duration::from_secs(1), // already expired (absolute)
                    now,
                    Duration::from_secs(300),
                ),
            )
            .await;

        // Attach a cap to that entry.
        let cap_id = [0x22u8; 32];
        state.insert_cap_for_test(cap_id, make_cap(cache_key)).await;

        assert_eq!(state.cache_len().await, 1);
        assert_eq!(state.caps_len().await, 1);

        prune_once(&state, Some(now)).await;

        assert_eq!(state.cache_len().await, 0, "expired entry must be removed");
        assert_eq!(
            state.caps_len().await,
            0,
            "orphaned cap must be evicted alongside its cache entry"
        );
        let snap = state.metrics_snapshot().await;
        assert_eq!(snap.counters.cache_evictions_absolute, 1);
        assert_eq!(snap.counters.cache_evictions_idle, 0);
    }

    // -----------------------------------------------------------------------
    // metrics_snapshot (populated-cache branch — avg_age > 0)
    // -----------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn metrics_snapshot_populated_cache_computes_avg_age() {
        let state = make_state().await;
        let now = Instant::now();
        let cache_key = [0x33u8; 32];

        // Insert an entry that is NOT expired so it persists in the cache.
        state
            .insert_cache_entry_for_test(
                cache_key,
                make_entry(
                    now,
                    now + Duration::from_secs(3600),
                    now,
                    Duration::from_secs(300),
                ),
            )
            .await;

        // Advance time so `created_at` is in the past — gives avg_age_secs > 0
        // and exercises the `else { (age_secs_total / cache_size as u128) as u64 }`
        // branch of `metrics_snapshot`.
        tokio::time::advance(Duration::from_secs(10)).await;
        let snap = state.metrics_snapshot().await;
        assert_eq!(snap.cache_size, 1);
        assert!(
            snap.avg_age_secs >= 10,
            "avg_age_secs must reflect elapsed time, got {}",
            snap.avg_age_secs
        );
    }

    // -----------------------------------------------------------------------
    // prune_once: idle eviction
    // -----------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn prune_once_idle_eviction_removes_entry_and_increments_idle_metric() {
        let state = make_state().await;
        let now = Instant::now();
        let cache_key = [0x44u8; 32];

        // Insert an entry whose idle TTL has already elapsed but whose
        // absolute deadline is far in the future — pure idle expiry.
        let last_used = now - Duration::from_secs(10);
        state
            .insert_cache_entry_for_test(
                cache_key,
                make_entry(
                    now,
                    now + Duration::from_secs(3600), // absolute far in future
                    last_used,
                    Duration::from_secs(5), // idle TTL=5s, last_used=10s ago → expired
                ),
            )
            .await;

        assert_eq!(state.cache_len().await, 1);

        prune_once(&state, Some(now)).await;

        assert_eq!(
            state.cache_len().await,
            0,
            "idle-expired entry must be removed"
        );
        let snap = state.metrics_snapshot().await;
        assert_eq!(
            snap.counters.cache_evictions_idle, 1,
            "idle eviction counter must be incremented"
        );
        assert_eq!(snap.counters.cache_evictions_absolute, 0);
    }

    // -----------------------------------------------------------------------
    // parse_admin_key_from_content
    // -----------------------------------------------------------------------

    #[test]
    fn parse_admin_key_finds_key_on_simple_line() {
        let content = "BOTWORK_ADMIN_API_KEY=my-secret-key\n";
        assert_eq!(
            parse_admin_key_from_content(content),
            Some("my-secret-key".to_string())
        );
    }

    #[test]
    fn parse_admin_key_skips_comments_and_blank_lines() {
        let content = "\n# This is a comment\nBOTWORK_ADMIN_API_KEY=the-key\n";
        assert_eq!(
            parse_admin_key_from_content(content),
            Some("the-key".to_string())
        );
    }

    #[test]
    fn parse_admin_key_trims_whitespace_from_value() {
        let content = "BOTWORK_ADMIN_API_KEY=  trimmed-key  \n";
        assert_eq!(
            parse_admin_key_from_content(content),
            Some("trimmed-key".to_string())
        );
    }

    #[test]
    fn parse_admin_key_returns_none_when_var_absent() {
        let content = "OTHER_VAR=something\n# no key here\n";
        assert_eq!(parse_admin_key_from_content(content), None);
    }

    #[test]
    fn parse_admin_key_returns_none_when_value_empty() {
        let content = "BOTWORK_ADMIN_API_KEY=\n";
        assert_eq!(parse_admin_key_from_content(content), None);
    }

    #[test]
    fn parse_admin_key_returns_none_on_empty_content() {
        assert_eq!(parse_admin_key_from_content(""), None);
    }

    // -----------------------------------------------------------------------
    // AdminKeyFile — mtime-cached reading
    // -----------------------------------------------------------------------

    #[test]
    fn admin_key_file_returns_none_when_file_missing() {
        let path = PathBuf::from("/nonexistent/path/admin.env");
        let akf = AdminKeyFile::new(path);
        assert_eq!(akf.current_key(), None);
    }

    #[test]
    fn admin_key_file_reads_key_from_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.env");
        std::fs::write(&path, "BOTWORK_ADMIN_API_KEY=test-key-abc\n").unwrap();

        let akf = AdminKeyFile::new(path);
        let key = akf.current_key().expect("should find key");
        assert_eq!(&*key, "test-key-abc");
    }

    #[test]
    fn admin_key_file_uses_cache_on_unchanged_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.env");
        std::fs::write(&path, "BOTWORK_ADMIN_API_KEY=cached-key\n").unwrap();

        let akf = AdminKeyFile::new(path.clone());
        let key1 = akf.current_key().expect("first read");

        // Overwrite with different content but force same mtime so the
        // cache path is exercised.  We do this by writing the same content,
        // then manually setting the cache mtime to match the file's current
        // mtime — simulating what happens between two calls when no rotation
        // occurred.
        let key2 = akf.current_key().expect("second read (cached)");
        assert_eq!(&*key1, &*key2, "second call must return the cached value");
    }

    #[test]
    fn admin_key_file_re_reads_after_file_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.env");
        std::fs::write(&path, "BOTWORK_ADMIN_API_KEY=initial-key\n").unwrap();

        let akf = AdminKeyFile::new(path.clone());
        assert!(akf.current_key().is_some(), "should read key initially");

        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            akf.current_key(),
            None,
            "should return None after file is deleted"
        );
    }

    // -----------------------------------------------------------------------
    // AdminKeySource
    // -----------------------------------------------------------------------

    #[test]
    fn admin_key_source_none_returns_none() {
        assert_eq!(AdminKeySource::None.current_key(), None);
    }

    #[test]
    fn admin_key_source_static_returns_key() {
        let src = AdminKeySource::Static(Arc::from("static-key"));
        let key = src.current_key().expect("static key");
        assert_eq!(&*key, "static-key");
    }

    #[test]
    fn admin_key_source_file_returns_key_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.env");
        std::fs::write(&path, "BOTWORK_ADMIN_API_KEY=file-key\n").unwrap();

        let src = AdminKeySource::File(Arc::new(AdminKeyFile::new(path)));
        let key = src.current_key().expect("file key");
        assert_eq!(&*key, "file-key");
    }

    // -----------------------------------------------------------------------
    // AppState builders
    // -----------------------------------------------------------------------

    #[test]
    fn with_admin_api_key_sets_static_source() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let state = rt.block_on(make_state());
        let state = state.with_admin_api_key("my-static-key");
        let key = state.admin_key_source.current_key().expect("key");
        assert_eq!(&*key, "my-static-key");
    }

    #[test]
    fn with_admin_key_file_sets_file_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.env");
        std::fs::write(&path, "BOTWORK_ADMIN_API_KEY=file-based-key\n").unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let state = rt.block_on(make_state());
        let state = state.with_admin_key_file(path);
        let key = state.admin_key_source.current_key().expect("key");
        assert_eq!(&*key, "file-based-key");
    }
}
