//! `auth::rate_limit` — per-`(tenant, source-IP)` token-bucket rate limiter
//! for the four OPAQUE auth endpoints.
//!
//! ## Design
//!
//! Token-bucket per key: each `(tenant, client_ip)` pair gets its own
//! bucket. Tokens replenish at a configurable sustained rate; the bucket
//! capacity caps burst. Each request consumes one token; when the bucket
//! is empty the request is rejected with a `Duration` indicating how long
//! the caller should wait before retrying.
//!
//! ## Store
//!
//! **In-memory, per broker instance.** The map resets on restart and is
//! not shared across replicas. A postgres-backed shared store is a
//! possible future step if the broker is ever scaled to multiple replicas;
//! see `SECURITY.md` for details. The map is bounded: stale buckets are
//! evicted by [`RateLimiter::sweep`], which is called from the background
//! prune task (see `crate::cache::prune_once`).
//!
//! ## Enumeration safety
//!
//! The rate-limit key is the *requested* tenant string, applied uniformly
//! regardless of whether the tenant exists. Keying on the requested string
//! ensures the limiter's behaviour — including rejection timing — is
//! identical for known and unknown tenants, preserving the OPAQUE
//! enumeration-resistance property.
//!
//! ## Disabling for tests
//!
//! Construct with [`RateLimitConfig::disabled`] (or set
//! `BOTWORK_AUTH_BROKER_RATE_LIMIT_RPS=0`) to bypass all rate checks.
//! The in-process test harness uses [`AuthState::from_stores`], which
//! defaults to a disabled limiter, so existing tests are unaffected.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tracing::warn;

const PREFIX: &str = "[auth-broker/rate-limit]";

/// How long a bucket must be idle before it is eligible for eviction by
/// [`RateLimiter::sweep`]. Mirrors [`crate::cache::IDLE_TTL`] (5 min).
pub const BUCKET_STALE_TTL: Duration = Duration::from_secs(5 * 60);

const ENV_RATE_LIMIT_RPS: &str = "BOTWORK_AUTH_BROKER_RATE_LIMIT_RPS";
const ENV_RATE_LIMIT_BURST: &str = "BOTWORK_AUTH_BROKER_RATE_LIMIT_BURST";

/// Default sustained rate: 10 requests per second per `(tenant, IP)`.
pub const DEFAULT_RATE_PER_SECOND: u32 = 10;
/// Default burst capacity: 20 tokens. Allows short bursts (e.g.
/// register + login in quick succession) while still bounding the
/// sustained attempt rate.
pub const DEFAULT_BURST: u32 = 20;

/// Configuration for [`RateLimiter`]. Cheap to clone (all fields are
/// scalars). Resolved once at startup from environment variables.
#[derive(Clone, Debug)]
pub struct RateLimitConfig {
    /// Sustained token-replenishment rate in tokens per second.
    /// A value of `0` is treated identically to setting `disabled = true`.
    pub rate_per_second: u32,
    /// Bucket capacity (burst size). Must be ≥ 1 when not disabled.
    pub burst: u32,
    /// When `true`, all rate-limit checks pass immediately. Use for
    /// the in-process test harness so existing tests are unaffected.
    pub disabled: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            rate_per_second: DEFAULT_RATE_PER_SECOND,
            burst: DEFAULT_BURST,
            disabled: false,
        }
    }
}

impl RateLimitConfig {
    /// Resolve from `std::env`. Unknown/malformed variables fall back to
    /// defaults. A `rate_per_second` of `0` disables limiting entirely.
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Resolve from an arbitrary key-lookup function. Allows unit tests
    /// to exercise every parsing branch without touching process env vars.
    ///
    /// `lookup(key)` returns `Some(value_string)` when the variable is
    /// set, or `None` when it is absent. Unknown/malformed values fall
    /// back to defaults. A `rate_per_second` of `0` disables limiting
    /// entirely.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let mut config = Self::default();

        if let Some(rps) =
            lookup(ENV_RATE_LIMIT_RPS).and_then(|v| parse_u32_str(ENV_RATE_LIMIT_RPS, &v))
        {
            if rps == 0 {
                config.disabled = true;
            } else {
                config.rate_per_second = rps;
            }
        }

        if let Some(burst) =
            lookup(ENV_RATE_LIMIT_BURST).and_then(|v| parse_u32_str(ENV_RATE_LIMIT_BURST, &v))
        {
            if burst == 0 {
                #[rustfmt::skip]
                warn!("{PREFIX} ignoring {}=0 (must be >= 1); using default {}", ENV_RATE_LIMIT_BURST, DEFAULT_BURST);
            } else {
                config.burst = burst;
            }
        }

        config
    }

    /// A config with rate limiting completely disabled. Used by the
    /// in-process test harness (via [`crate::auth::AuthState::from_stores`])
    /// so existing tests are unaffected.
    pub fn disabled() -> Self {
        Self {
            disabled: true,
            ..Self::default()
        }
    }

    /// Whether limiting is effectively off (either `disabled` flag or
    /// zero rate).
    pub fn is_disabled(&self) -> bool {
        self.disabled || self.rate_per_second == 0
    }
}

fn parse_u32_str(key: &str, val: &str) -> Option<u32> {
    match val.parse::<u32>() {
        Ok(n) => Some(n),
        Err(err) => {
            warn!("{PREFIX} ignoring {key}={val:?} (parse error: {err})");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Token bucket
// ---------------------------------------------------------------------------

/// A single token bucket for one `(tenant, ip)` key.
struct Bucket {
    /// Current token count. May be fractional due to partial refills.
    tokens: f64,
    /// Monotonic timestamp of the last access; drives refill + stale eviction.
    last_access: Instant,
}

impl Bucket {
    fn new(capacity: f64, now: Instant) -> Self {
        Self {
            tokens: capacity,
            last_access: now,
        }
    }

    /// Refill based on elapsed time, then attempt to consume one token.
    ///
    /// Returns `Ok(())` if the request is allowed, or `Err(retry_after)`
    /// indicating how long the caller must wait before the bucket will
    /// have a token again.
    fn try_consume(&mut self, rate: f64, capacity: f64, now: Instant) -> Result<(), Duration> {
        let elapsed = now
            .saturating_duration_since(self.last_access)
            .as_secs_f64();
        self.tokens = (self.tokens + elapsed * rate).min(capacity);
        self.last_access = now;

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            // Tokens needed to reach 1: (1 - current).
            let deficit = 1.0 - self.tokens;
            // Time to refill: deficit / rate.
            let secs = (deficit / rate).max(0.0);
            // Round up to the next whole second; always at least 1 second.
            let secs_ceil = secs.ceil() as u64;
            Err(Duration::from_secs(secs_ceil.max(1)))
        }
    }

    fn is_stale(&self, now: Instant, stale_after: Duration) -> bool {
        now.saturating_duration_since(self.last_access) > stale_after
    }
}

// ---------------------------------------------------------------------------
// RateLimiter
// ---------------------------------------------------------------------------

/// Rate-limit key: `(tenant, source_ip)`.
///
/// The *requested* tenant string is used uniformly regardless of whether
/// the tenant exists (enumeration resistance).
type RateLimitKey = (String, String);

/// In-memory per-instance token-bucket rate limiter. Cheap to clone
/// (`Arc`-backed inner storage, `Clone` for axum `State` extraction).
#[derive(Clone)]
pub struct RateLimiter {
    buckets: Arc<Mutex<HashMap<RateLimitKey, Bucket>>>,
    config: Arc<RateLimitConfig>,
}

impl RateLimiter {
    /// Construct with the given config.
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            buckets: Arc::new(Mutex::new(HashMap::new())),
            config: Arc::new(config),
        }
    }

    /// Check whether a request from `(tenant, ip)` is permitted.
    ///
    /// Returns `Ok(())` when allowed; `Err(retry_after)` when the bucket
    /// is exhausted, indicating how long the caller must wait.
    ///
    /// Timing is identical for known and unknown tenants: the limiter
    /// keys on the *requested* tenant string and never reads from the
    /// tenant store.
    pub async fn check(&self, tenant: &str, ip: &str, now: Instant) -> Result<(), Duration> {
        if self.config.is_disabled() {
            return Ok(());
        }

        let rate = self.config.rate_per_second as f64;
        let capacity = self.config.burst as f64;

        let key = (tenant.to_string(), ip.to_string());
        let mut guard = self.buckets.lock().await;
        let bucket = guard
            .entry(key)
            .or_insert_with(|| Bucket::new(capacity, now));

        bucket.try_consume(rate, capacity, now)
    }

    /// Evict buckets that have been idle for longer than
    /// [`BUCKET_STALE_TTL`]. Called from `crate::cache::prune_once`
    /// alongside the other sweep operations so the map does not grow
    /// without bound.
    ///
    /// Returns the number of entries evicted.
    pub async fn sweep(&self, now: Instant) -> usize {
        if self.config.is_disabled() {
            return 0;
        }
        let mut guard = self.buckets.lock().await;
        let before = guard.len();
        guard.retain(|_, bucket| !bucket.is_stale(now, BUCKET_STALE_TTL));
        before.saturating_sub(guard.len())
    }

    /// Number of tracked buckets. Available under `test` /
    /// `test-support` so tests can assert eviction without going
    /// through the HTTP layer.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn bucket_count(&self) -> usize {
        self.buckets.lock().await.len()
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::advance;

    fn limiter_with(rps: u32, burst: u32) -> RateLimiter {
        RateLimiter::new(RateLimitConfig {
            rate_per_second: rps,
            burst,
            disabled: false,
        })
    }

    fn disabled_limiter() -> RateLimiter {
        RateLimiter::new(RateLimitConfig::disabled())
    }

    // ------------------------------------------------------------------
    // Bucket basics: consume and refill
    // ------------------------------------------------------------------

    /// A fresh bucket allows exactly `burst` consecutive requests.
    #[tokio::test(start_paused = true)]
    async fn burst_exhausted_then_rejected() {
        let limiter = limiter_with(1, 5);
        let now = Instant::now();

        // First 5 requests (burst capacity) must succeed.
        for i in 0..5 {
            assert!(
                limiter.check("t", "1.2.3.4", now).await.is_ok(),
                "request {i} should be allowed"
            );
        }

        // Request 6 should be rejected.
        let result = limiter.check("t", "1.2.3.4", now).await;
        assert!(result.is_err(), "request 6 should be rate-limited");
        let retry_after = result.unwrap_err();
        assert!(retry_after.as_secs() >= 1, "retry_after must be >= 1s");
    }

    /// After waiting one full second, one token is refilled (rate=1 rps).
    #[tokio::test(start_paused = true)]
    async fn bucket_refills_after_one_second() {
        let limiter = limiter_with(1, 1);
        let now = Instant::now();

        // Consume the single token.
        assert!(limiter.check("t", "1.2.3.4", now).await.is_ok());
        // Immediate retry is rejected.
        assert!(limiter.check("t", "1.2.3.4", now).await.is_err());

        // Advance 1 second — one token is refilled.
        advance(Duration::from_secs(1)).await;
        let now2 = Instant::now();
        assert!(
            limiter.check("t", "1.2.3.4", now2).await.is_ok(),
            "should be allowed after 1s refill"
        );
    }

    /// retry_after is a positive duration even when called repeatedly
    /// while the bucket is empty.
    #[tokio::test(start_paused = true)]
    async fn retry_after_is_always_positive() {
        let limiter = limiter_with(1, 1);
        let now = Instant::now();

        assert!(limiter.check("t", "1.2.3.4", now).await.is_ok());

        for _ in 0..3 {
            let result = limiter.check("t", "1.2.3.4", now).await;
            assert!(result.is_err());
            let retry_after = result.unwrap_err();
            assert!(
                retry_after >= Duration::from_secs(1),
                "retry_after must be >= 1s, got {retry_after:?}"
            );
        }
    }

    // ------------------------------------------------------------------
    // Per-key isolation
    // ------------------------------------------------------------------

    /// Different `(tenant, ip)` pairs each get their own bucket.
    #[tokio::test(start_paused = true)]
    async fn per_key_isolation() {
        let limiter = limiter_with(1, 1);
        let now = Instant::now();

        // Exhaust bucket for tenant "a", ip "1.1.1.1".
        assert!(limiter.check("a", "1.1.1.1", now).await.is_ok());
        assert!(limiter.check("a", "1.1.1.1", now).await.is_err());

        // Bucket for same tenant, different IP is untouched.
        assert!(
            limiter.check("a", "2.2.2.2", now).await.is_ok(),
            "different IP must not share bucket"
        );

        // Bucket for different tenant, same IP is untouched.
        assert!(
            limiter.check("b", "1.1.1.1", now).await.is_ok(),
            "different tenant must not share bucket"
        );
    }

    // ------------------------------------------------------------------
    // Stale-bucket eviction
    // ------------------------------------------------------------------

    /// `sweep` removes buckets that have been idle for longer than
    /// `BUCKET_STALE_TTL`.
    #[tokio::test(start_paused = true)]
    async fn sweep_evicts_stale_buckets() {
        let limiter = limiter_with(1, 5);
        let now = Instant::now();

        // Create three buckets.
        for i in 0..3u8 {
            let ip = format!("10.0.0.{i}");
            let _ = limiter.check("tenant", &ip, now).await;
        }
        assert_eq!(limiter.bucket_count().await, 3);

        // Advance past the stale TTL.
        advance(BUCKET_STALE_TTL + Duration::from_secs(1)).await;
        let evicted = limiter.sweep(Instant::now()).await;
        assert_eq!(evicted, 3, "all stale buckets must be evicted");
        assert_eq!(limiter.bucket_count().await, 0);
    }

    /// A bucket used *after* the advance must survive the sweep.
    #[tokio::test(start_paused = true)]
    async fn sweep_preserves_fresh_buckets() {
        let limiter = limiter_with(1, 5);
        let t0 = Instant::now();

        // Stale bucket inserted at t0.
        let _ = limiter.check("t", "1.0.0.1", t0).await;

        // Advance past the stale TTL.
        advance(BUCKET_STALE_TTL + Duration::from_secs(1)).await;

        // Fresh bucket inserted after the advance.
        let t1 = Instant::now();
        let _ = limiter.check("t", "1.0.0.2", t1).await;

        let evicted = limiter.sweep(Instant::now()).await;
        assert_eq!(evicted, 1, "only the stale bucket must be evicted");
        assert_eq!(limiter.bucket_count().await, 1, "fresh bucket must survive");
    }

    // ------------------------------------------------------------------
    // Disabled limiter
    // ------------------------------------------------------------------

    /// A disabled limiter always allows requests regardless of rate.
    #[tokio::test(start_paused = true)]
    async fn disabled_limiter_always_allows() {
        let limiter = disabled_limiter();
        let now = Instant::now();

        // Far more than any burst would allow.
        for _ in 0..1000 {
            assert!(
                limiter.check("t", "1.2.3.4", now).await.is_ok(),
                "disabled limiter must always allow"
            );
        }
    }

    /// A disabled limiter's sweep is a no-op.
    #[tokio::test(start_paused = true)]
    async fn disabled_limiter_sweep_is_noop() {
        let limiter = disabled_limiter();
        // check() on a disabled limiter does not insert any buckets.
        let _ = limiter.check("t", "1.2.3.4", Instant::now()).await;
        // bucket_count should be 0 — no buckets ever inserted.
        assert_eq!(limiter.bucket_count().await, 0);
        let evicted = limiter.sweep(Instant::now()).await;
        assert_eq!(evicted, 0);
    }

    // ------------------------------------------------------------------
    // High-rate config: many requests succeed within burst
    // ------------------------------------------------------------------

    /// With a high burst, a sequence of requests all succeed.
    #[tokio::test(start_paused = true)]
    async fn high_burst_allows_many_sequential_requests() {
        let limiter = limiter_with(100, 100);
        let now = Instant::now();

        for i in 0..100 {
            assert!(
                limiter.check("t", "1.2.3.4", now).await.is_ok(),
                "request {i} should be allowed with burst=100"
            );
        }

        // Request 101 should be rate-limited.
        assert!(limiter.check("t", "1.2.3.4", now).await.is_err());
    }

    // ------------------------------------------------------------------
    // RateLimitConfig::from_lookup — env parsing branches
    // ------------------------------------------------------------------

    fn lookup_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    /// No env vars set → defaults apply.
    #[test]
    fn from_lookup_empty_uses_defaults() {
        let cfg = RateLimitConfig::from_lookup(|_| None);
        assert_eq!(cfg.rate_per_second, DEFAULT_RATE_PER_SECOND);
        assert_eq!(cfg.burst, DEFAULT_BURST);
        assert!(!cfg.disabled);
    }

    /// RPS=0 sets `disabled = true`.
    #[test]
    fn from_lookup_rps_zero_disables() {
        let cfg = RateLimitConfig::from_lookup(lookup_from(&[(ENV_RATE_LIMIT_RPS, "0")]));
        assert!(cfg.disabled, "rps=0 must disable limiting");
    }

    /// RPS=non-zero sets `rate_per_second`.
    #[test]
    fn from_lookup_rps_nonzero_sets_rate() {
        let cfg = RateLimitConfig::from_lookup(lookup_from(&[(ENV_RATE_LIMIT_RPS, "42")]));
        assert_eq!(cfg.rate_per_second, 42);
        assert!(!cfg.disabled);
    }

    /// Malformed RPS value falls back to default; no panic.
    #[test]
    fn from_lookup_rps_malformed_uses_default() {
        let cfg =
            RateLimitConfig::from_lookup(lookup_from(&[(ENV_RATE_LIMIT_RPS, "not-a-number")]));
        assert_eq!(cfg.rate_per_second, DEFAULT_RATE_PER_SECOND);
        assert!(!cfg.disabled);
    }

    /// Burst=0 is ignored; default burst is preserved.
    #[test]
    fn from_lookup_burst_zero_ignored() {
        let cfg = RateLimitConfig::from_lookup(lookup_from(&[(ENV_RATE_LIMIT_BURST, "0")]));
        assert_eq!(cfg.burst, DEFAULT_BURST, "burst=0 must be ignored");
    }

    /// Burst=non-zero sets `burst`.
    #[test]
    fn from_lookup_burst_nonzero_sets_burst() {
        let cfg = RateLimitConfig::from_lookup(lookup_from(&[(ENV_RATE_LIMIT_BURST, "50")]));
        assert_eq!(cfg.burst, 50);
    }

    /// Malformed burst value falls back to default; no panic.
    #[test]
    fn from_lookup_burst_malformed_uses_default() {
        let cfg = RateLimitConfig::from_lookup(lookup_from(&[(ENV_RATE_LIMIT_BURST, "bad")]));
        assert_eq!(cfg.burst, DEFAULT_BURST);
    }

    /// Both RPS and burst can be overridden simultaneously.
    #[test]
    fn from_lookup_both_overridden() {
        let cfg = RateLimitConfig::from_lookup(lookup_from(&[
            (ENV_RATE_LIMIT_RPS, "5"),
            (ENV_RATE_LIMIT_BURST, "15"),
        ]));
        assert_eq!(cfg.rate_per_second, 5);
        assert_eq!(cfg.burst, 15);
        assert!(!cfg.disabled);
    }

    /// `from_env()` reads `std::env` — just assert it returns a valid
    /// config without panicking. The relevant env vars are absent in
    /// the test environment, so defaults should apply.
    #[test]
    fn from_env_returns_valid_config() {
        let cfg = RateLimitConfig::from_env();
        assert!(cfg.rate_per_second > 0 || cfg.disabled);
    }

    /// `is_disabled()` returns `true` when `rate_per_second == 0` even
    /// if `disabled` is `false`. This exercises the `|| self.rate_per_second == 0`
    /// branch of the short-circuit `||`.
    #[test]
    fn is_disabled_when_rate_per_second_is_zero() {
        let cfg = RateLimitConfig {
            rate_per_second: 0,
            burst: DEFAULT_BURST,
            disabled: false,
        };
        assert!(cfg.is_disabled(), "rate_per_second=0 must report disabled");
    }
}
