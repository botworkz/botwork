//! In-tree configuration knobs for auth-broker. Resolved once at
//! startup; the resulting [`TtlConfig`] is stamped onto every
//! [`crate::cache::AppState`] and consulted on every cache write.
//!
//! ## Why this exists
//!
//! Issue #126 (Tier 2 in-memory hardening) calls for per-tenant idle
//! TTL + absolute TTL knobs with operator-ratchetable floors, so that
//! a single deploy can tighten the cache lifetime for high-sensitivity
//! tenants without a code change. The TTL constants in
//! [`crate::cache`] continue to define the *defaults*; this module
//! turns them into operator-overridable values.
//!
//! ## Wire shape
//!
//! For v0, configuration is read from environment variables. A
//! richer source (YAML / per-tenant config file) is a natural
//! follow-up — see `SECURITY.md` — but environment variables let
//! operators tighten the floor in a `docker compose` override
//! without redeploying a config-file shipping path that doesn't yet
//! exist.
//!
//! Environment variables (all optional):
//!
//! | name                                  | meaning                                                   |
//! | ------------------------------------- | --------------------------------------------------------- |
//! | `BOTWORK_AUTH_BROKER_IDLE_TTL_SECS`   | default idle TTL applied to every tenant (default `300`)  |
//! | `BOTWORK_AUTH_BROKER_ABS_TTL_SECS`    | default absolute TTL applied to every tenant (default `28800`) |
//! | `BOTWORK_AUTH_BROKER_MIN_IDLE_SECS`   | operator-set floor; rejects any per-tenant value below this (default `1`) |
//! | `BOTWORK_AUTH_BROKER_MIN_ABS_SECS`    | operator-set floor; rejects any per-tenant value below this (default `1`) |
//! | `BOTWORK_AUTH_BROKER_TENANT_IDLE_<T>` | override the idle TTL for tenant `<T>` (uppercased, `-`/`.` → `_`) |
//! | `BOTWORK_AUTH_BROKER_TENANT_ABS_<T>`  | override the absolute TTL for tenant `<T>`                |
//!
//! Per-tenant values are floor-clamped at *load time*: if an operator
//! sets `MIN_IDLE_SECS=60` and a per-tenant override of `30`, the
//! override is silently bumped to `60` and a warning is logged. Floors
//! exist precisely so a deploy can ratchet *down* a permissive default
//! without auditing every per-tenant override.
//!
//! ## What this is NOT
//!
//! - This is a *configuration* surface, not a *security* boundary. A
//!   sufficiently permissive `IDLE_TTL_SECS=86400` widens the in-memory
//!   exposure window — that's the operator's choice, and it's documented
//!   in `SECURITY.md`.
//! - There is no per-tenant *minimum*: floors are global. If you want a
//!   tighter floor for one tenant, ratchet the global floor — every
//!   other tenant's effective TTL is already at-or-above it.

use std::collections::HashMap;
use std::time::Duration;

use tracing::warn;

use crate::cache::{ABSOLUTE_TTL, IDLE_TTL};

const PREFIX: &str = "[auth-broker]";

const ENV_IDLE_TTL: &str = "BOTWORK_AUTH_BROKER_IDLE_TTL_SECS";
const ENV_ABS_TTL: &str = "BOTWORK_AUTH_BROKER_ABS_TTL_SECS";
const ENV_MIN_IDLE: &str = "BOTWORK_AUTH_BROKER_MIN_IDLE_SECS";
const ENV_MIN_ABS: &str = "BOTWORK_AUTH_BROKER_MIN_ABS_SECS";
const ENV_TENANT_IDLE_PREFIX: &str = "BOTWORK_AUTH_BROKER_TENANT_IDLE_";
const ENV_TENANT_ABS_PREFIX: &str = "BOTWORK_AUTH_BROKER_TENANT_ABS_";

const DEFAULT_MIN_IDLE_SECS: u64 = 1;
const DEFAULT_MIN_ABS_SECS: u64 = 1;

/// Resolved TTL configuration applied to every cache insert and every
/// `is_expired` check. Cheap to clone (carries one small `HashMap`
/// keyed by tenant string).
#[derive(Clone, Debug)]
pub struct TtlConfig {
    default_idle: Duration,
    default_absolute: Duration,
    min_idle: Duration,
    min_absolute: Duration,
    per_tenant_idle: HashMap<String, Duration>,
    per_tenant_absolute: HashMap<String, Duration>,
}

impl Default for TtlConfig {
    fn default() -> Self {
        Self {
            default_idle: IDLE_TTL,
            default_absolute: ABSOLUTE_TTL,
            min_idle: Duration::from_secs(DEFAULT_MIN_IDLE_SECS),
            min_absolute: Duration::from_secs(DEFAULT_MIN_ABS_SECS),
            per_tenant_idle: HashMap::new(),
            per_tenant_absolute: HashMap::new(),
        }
    }
}

impl TtlConfig {
    /// Resolve a [`TtlConfig`] from `std::env`. Unknown variables and
    /// parse failures are logged at `warn!` and fall back to defaults
    /// — this is deliberately non-fatal so a typo in one knob doesn't
    /// take the broker down.
    pub fn from_env() -> Self {
        let lookup = |k: &str| std::env::var(k).ok();
        let env: HashMap<String, String> = std::env::vars().collect();
        Self::from_lookup(lookup, &env)
    }

    /// Build a [`TtlConfig`] from an explicit lookup function plus the
    /// full environment map (used for the per-tenant prefix scan).
    /// Exposed for tests so they don't have to mutate process-global
    /// state.
    pub fn from_lookup<F: Fn(&str) -> Option<String>>(
        lookup: F,
        env: &HashMap<String, String>,
    ) -> Self {
        let mut config = TtlConfig::default();

        if let Some(value) = parse_secs_env(&lookup, ENV_IDLE_TTL) {
            config.default_idle = value;
        }
        if let Some(value) = parse_secs_env(&lookup, ENV_ABS_TTL) {
            config.default_absolute = value;
        }
        if let Some(value) = parse_secs_env(&lookup, ENV_MIN_IDLE) {
            config.min_idle = value;
        }
        if let Some(value) = parse_secs_env(&lookup, ENV_MIN_ABS) {
            config.min_absolute = value;
        }

        for (key, value) in env {
            if let Some(tenant) = strip_prefix_to_tenant(key, ENV_TENANT_IDLE_PREFIX) {
                if let Some(parsed) = parse_secs(value, key) {
                    config.per_tenant_idle.insert(tenant, parsed);
                }
            } else if let Some(tenant) = strip_prefix_to_tenant(key, ENV_TENANT_ABS_PREFIX) {
                if let Some(parsed) = parse_secs(value, key) {
                    config.per_tenant_absolute.insert(tenant, parsed);
                }
            }
        }

        config.normalise();
        config
    }

    /// Effective idle TTL for `tenant`: per-tenant override if set,
    /// else the default. Result is guaranteed to be ≥ `min_idle`.
    pub fn idle_for(&self, tenant: &str) -> Duration {
        let raw = self
            .per_tenant_idle
            .get(tenant)
            .copied()
            .unwrap_or(self.default_idle);
        raw.max(self.min_idle)
    }

    /// Effective absolute TTL for `tenant`: per-tenant override if
    /// set, else the default. Result is guaranteed to be ≥
    /// `min_absolute`.
    pub fn absolute_for(&self, tenant: &str) -> Duration {
        let raw = self
            .per_tenant_absolute
            .get(tenant)
            .copied()
            .unwrap_or(self.default_absolute);
        raw.max(self.min_absolute)
    }

    /// The global idle floor — surfaced so callers (tests, metrics)
    /// don't have to round-trip through `idle_for`.
    pub fn min_idle(&self) -> Duration {
        self.min_idle
    }

    /// The global absolute floor — surfaced so callers (tests,
    /// metrics) don't have to round-trip through `absolute_for`.
    pub fn min_absolute(&self) -> Duration {
        self.min_absolute
    }

    /// Builder-style override for the global default idle TTL.
    pub fn with_default_idle(mut self, ttl: Duration) -> Self {
        self.default_idle = ttl;
        self.normalise();
        self
    }

    /// Builder-style override for the global default absolute TTL.
    pub fn with_default_absolute(mut self, ttl: Duration) -> Self {
        self.default_absolute = ttl;
        self.normalise();
        self
    }

    /// Builder-style override for the global idle floor. Subsequent
    /// `with_tenant_idle` calls (and the existing per-tenant table)
    /// are re-clamped against the new floor.
    pub fn with_min_idle(mut self, ttl: Duration) -> Self {
        self.min_idle = ttl;
        self.normalise();
        self
    }

    /// Builder-style override for the global absolute floor.
    pub fn with_min_absolute(mut self, ttl: Duration) -> Self {
        self.min_absolute = ttl;
        self.normalise();
        self
    }

    /// Builder-style per-tenant idle override.
    pub fn with_tenant_idle(mut self, tenant: impl Into<String>, ttl: Duration) -> Self {
        self.per_tenant_idle.insert(tenant.into(), ttl);
        self.normalise();
        self
    }

    /// Builder-style per-tenant absolute override.
    pub fn with_tenant_absolute(mut self, tenant: impl Into<String>, ttl: Duration) -> Self {
        self.per_tenant_absolute.insert(tenant.into(), ttl);
        self.normalise();
        self
    }

    /// Walk the per-tenant overrides and emit a warning for any value
    /// that had to be ratcheted up to meet the floor. Idempotent;
    /// callers can re-run after every builder mutation.
    fn normalise(&mut self) {
        for (tenant, value) in self.per_tenant_idle.iter_mut() {
            if *value < self.min_idle {
                #[rustfmt::skip]
                warn!("{PREFIX} ttl-config: per-tenant idle override raised tenant={} from={}s to={}s (min_idle floor)", tenant, value.as_secs(), self.min_idle.as_secs());
                *value = self.min_idle;
            }
        }
        for (tenant, value) in self.per_tenant_absolute.iter_mut() {
            if *value < self.min_absolute {
                #[rustfmt::skip]
                warn!("{PREFIX} ttl-config: per-tenant absolute override raised tenant={} from={}s to={}s (min_absolute floor)", tenant, value.as_secs(), self.min_absolute.as_secs());
                *value = self.min_absolute;
            }
        }
        if self.default_idle < self.min_idle {
            #[rustfmt::skip]
            warn!("{PREFIX} ttl-config: default idle raised from={}s to={}s (min_idle floor)", self.default_idle.as_secs(), self.min_idle.as_secs());
            self.default_idle = self.min_idle;
        }
        if self.default_absolute < self.min_absolute {
            #[rustfmt::skip]
            warn!("{PREFIX} ttl-config: default absolute raised from={}s to={}s (min_absolute floor)", self.default_absolute.as_secs(), self.min_absolute.as_secs());
            self.default_absolute = self.min_absolute;
        }
    }
}

fn parse_secs(raw: &str, key: &str) -> Option<Duration> {
    match raw.parse::<u64>() {
        Ok(0) => {
            warn!("{PREFIX} ttl-config: ignoring {key}=0 (must be > 0)");
            None
        }
        Ok(n) => Some(Duration::from_secs(n)),
        Err(err) => {
            warn!("{PREFIX} ttl-config: ignoring {key}={raw:?} (parse: {err})");
            None
        }
    }
}

fn parse_secs_env<F: Fn(&str) -> Option<String>>(lookup: F, key: &str) -> Option<Duration> {
    lookup(key).and_then(|raw| parse_secs(&raw, key))
}

/// Recover a tenant string from an env var key like
/// `BOTWORK_AUTH_BROKER_TENANT_IDLE_PHLAX`. The tenant is returned
/// lowercased so the comparison against the path-parsed tenant string
/// is case-insensitive. Returns `None` for keys that don't start with
/// the prefix or that have an empty tenant suffix.
fn strip_prefix_to_tenant(key: &str, prefix: &str) -> Option<String> {
    let suffix = key.strip_prefix(prefix)?;
    if suffix.is_empty() {
        return None;
    }
    Some(suffix.to_ascii_lowercase().replace('_', "-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup_from(env: &HashMap<String, String>) -> impl Fn(&str) -> Option<String> + '_ {
        |k: &str| env.get(k).cloned()
    }

    #[test]
    fn defaults_match_constants() {
        let cfg = TtlConfig::default();
        assert_eq!(cfg.idle_for("any"), IDLE_TTL);
        assert_eq!(cfg.absolute_for("any"), ABSOLUTE_TTL);
        assert_eq!(cfg.min_idle(), Duration::from_secs(DEFAULT_MIN_IDLE_SECS));
        assert_eq!(
            cfg.min_absolute(),
            Duration::from_secs(DEFAULT_MIN_ABS_SECS)
        );
    }

    #[test]
    fn from_env_overrides_global_defaults() {
        let mut env = HashMap::new();
        env.insert(ENV_IDLE_TTL.to_string(), "120".to_string());
        env.insert(ENV_ABS_TTL.to_string(), "3600".to_string());
        let cfg = TtlConfig::from_lookup(lookup_from(&env), &env);
        assert_eq!(cfg.idle_for("any"), Duration::from_secs(120));
        assert_eq!(cfg.absolute_for("any"), Duration::from_secs(3600));
    }

    #[test]
    fn per_tenant_override_beats_default() {
        let mut env = HashMap::new();
        env.insert(ENV_IDLE_TTL.to_string(), "300".to_string());
        env.insert(format!("{ENV_TENANT_IDLE_PREFIX}PHLAX"), "30".to_string());
        let cfg = TtlConfig::from_lookup(lookup_from(&env), &env);
        assert_eq!(cfg.idle_for("phlax"), Duration::from_secs(30));
        assert_eq!(cfg.idle_for("other"), Duration::from_secs(300));
    }

    #[test]
    fn floor_clamps_per_tenant_override() {
        let mut env = HashMap::new();
        env.insert(ENV_MIN_IDLE.to_string(), "60".to_string());
        env.insert(format!("{ENV_TENANT_IDLE_PREFIX}PHLAX"), "30".to_string());
        let cfg = TtlConfig::from_lookup(lookup_from(&env), &env);
        assert_eq!(cfg.idle_for("phlax"), Duration::from_secs(60));
    }

    #[test]
    fn floor_clamps_default_when_below() {
        let mut env = HashMap::new();
        env.insert(ENV_MIN_ABS.to_string(), "7200".to_string());
        env.insert(ENV_ABS_TTL.to_string(), "1".to_string());
        let cfg = TtlConfig::from_lookup(lookup_from(&env), &env);
        assert_eq!(cfg.absolute_for("any"), Duration::from_secs(7200));
    }

    #[test]
    fn zero_value_is_ignored_not_fatal() {
        let mut env = HashMap::new();
        env.insert(ENV_IDLE_TTL.to_string(), "0".to_string());
        let cfg = TtlConfig::from_lookup(lookup_from(&env), &env);
        assert_eq!(cfg.idle_for("any"), IDLE_TTL);
    }

    #[test]
    fn malformed_value_is_ignored_not_fatal() {
        let mut env = HashMap::new();
        env.insert(ENV_IDLE_TTL.to_string(), "banana".to_string());
        let cfg = TtlConfig::from_lookup(lookup_from(&env), &env);
        assert_eq!(cfg.idle_for("any"), IDLE_TTL);
    }

    #[test]
    fn tenant_env_var_recovery_lowercases_and_swaps_underscore_for_hyphen() {
        let mut env = HashMap::new();
        env.insert(
            format!("{ENV_TENANT_ABS_PREFIX}MY_TENANT"),
            "60".to_string(),
        );
        let cfg = TtlConfig::from_lookup(lookup_from(&env), &env);
        // `MY_TENANT` recovers as `my-tenant`. Tenant strings in the
        // path-parser regex permit `[A-Za-z0-9._-]+`; underscores are
        // not a valid path segment, so the env-side spelling MUST
        // recover to the hyphen form.
        assert_eq!(cfg.absolute_for("my-tenant"), Duration::from_secs(60));
    }

    #[test]
    fn builder_round_trip() {
        let cfg = TtlConfig::default()
            .with_default_idle(Duration::from_secs(120))
            .with_default_absolute(Duration::from_secs(3600))
            .with_min_idle(Duration::from_secs(30))
            .with_min_absolute(Duration::from_secs(60))
            .with_tenant_idle("phlax", Duration::from_secs(10))
            .with_tenant_absolute("phlax", Duration::from_secs(20));
        assert_eq!(cfg.idle_for("phlax"), Duration::from_secs(30));
        assert_eq!(cfg.absolute_for("phlax"), Duration::from_secs(60));
        assert_eq!(cfg.idle_for("other"), Duration::from_secs(120));
        assert_eq!(cfg.absolute_for("other"), Duration::from_secs(3600));
    }

    #[test]
    fn empty_prefix_suffix_is_ignored() {
        let mut env = HashMap::new();
        env.insert(ENV_TENANT_IDLE_PREFIX.to_string(), "60".to_string());
        let cfg = TtlConfig::from_lookup(lookup_from(&env), &env);
        // Setting the prefix alone is a no-op; the tenant is empty.
        assert!(cfg.per_tenant_idle.is_empty());
    }

    /// `from_env()` reads `std::env` variables. With none of the
    /// relevant env vars set, it should return defaults ≥ the
    /// hard-coded minimum floors.
    #[test]
    fn from_env_returns_a_ttl_config_with_sane_defaults() {
        let cfg = TtlConfig::from_env();
        assert!(
            cfg.idle_for("any") >= Duration::from_secs(DEFAULT_MIN_IDLE_SECS),
            "idle_for must be ≥ MIN_IDLE"
        );
        assert!(
            cfg.absolute_for("any") >= Duration::from_secs(DEFAULT_MIN_ABS_SECS),
            "absolute_for must be ≥ MIN_ABS"
        );
    }

    /// `normalise()` clamps `default_idle` up to `min_idle` when it
    /// falls below the floor. This builder sequence hits the warn+clamp
    /// path in `normalise()` that the existing tests miss.
    #[test]
    fn with_default_idle_below_min_idle_is_clamped() {
        let floor = Duration::from_secs(120);
        let cfg = TtlConfig::default()
            .with_min_idle(floor)
            .with_default_idle(Duration::from_secs(10)); // 10 < 120 → clamped
        assert_eq!(
            cfg.idle_for("any"),
            floor,
            "default_idle below min_idle must be clamped to the floor"
        );
    }

    /// `normalise()` clamps `default_absolute` up to `min_absolute`
    /// when set via a builder call.
    #[test]
    fn with_default_absolute_below_min_absolute_is_clamped() {
        let floor = Duration::from_secs(240);
        let cfg = TtlConfig::default()
            .with_min_absolute(floor)
            .with_default_absolute(Duration::from_secs(5)); // 5 < 240 → clamped
        assert_eq!(
            cfg.absolute_for("any"),
            floor,
            "default_absolute below min_absolute must be clamped to the floor"
        );
    }

    /// Raising `min_idle` after per-tenant entries are already present
    /// re-clamps those entries through `normalise()`.
    #[test]
    fn with_min_idle_reclamping_existing_per_tenant_entries() {
        let cfg = TtlConfig::default()
            .with_tenant_idle("acme", Duration::from_secs(30))
            .with_min_idle(Duration::from_secs(90)); // raises floor above existing entry
        assert_eq!(
            cfg.idle_for("acme"),
            Duration::from_secs(90),
            "existing per-tenant entry must be re-clamped when min_idle rises"
        );
    }

    /// Raising `min_absolute` after per-tenant entries are already present
    /// re-clamps those entries through `normalise()`.
    #[test]
    fn with_min_absolute_reclamping_existing_per_tenant_entries() {
        let cfg = TtlConfig::default()
            .with_tenant_absolute("acme", Duration::from_secs(30))
            .with_min_absolute(Duration::from_secs(120)); // raises floor above existing entry
        assert_eq!(
            cfg.absolute_for("acme"),
            Duration::from_secs(120),
            "existing per-tenant entry must be re-clamped when min_absolute rises"
        );
    }

    /// `with_tenant_absolute` below the current `min_absolute` floor is
    /// clamped immediately at insert time.
    #[test]
    fn with_tenant_absolute_below_floor_is_clamped() {
        let floor = Duration::from_secs(180);
        let cfg = TtlConfig::default()
            .with_min_absolute(floor)
            .with_tenant_absolute("acme", Duration::from_secs(10)); // 10 < 180 → clamped
        assert_eq!(
            cfg.absolute_for("acme"),
            floor,
            "per-tenant absolute below floor must be clamped"
        );
    }
}
