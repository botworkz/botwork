pub mod admin;
pub mod auth;
pub mod cache;
pub mod caps;
pub mod config;
pub mod error_response;
pub mod grammar;
pub mod handler;
pub mod metrics;
pub mod secrets;
pub mod store;

use std::path::PathBuf;

pub use admin::build_admin_router;
pub use auth::{
    build_auth_router, derive_lease_kek, unwrap_session_key, wrap_session_key, AuthState, Bearer,
    BearerHash, LeaseKekError, LeaseRow, PendingMap, RateLimitConfig, RateLimiter,
    LEASE_EXPORT_KEY_TTL,
};
pub use cache::{
    evict_caps_for_lease, spawn_prune_task, AppState, AppStateMetricsSnapshot, CacheEntry,
    ABSOLUTE_TTL, IDLE_TTL, PRUNE_INTERVAL,
};
pub use caps::{CapEntry, CapId, CAP_TTL};
pub use config::TtlConfig;
pub use error_response::{ErrorCode, ErrorResponse, DOCS_URL};
pub use grammar::{
    normalise_name, validate_plugin_name, validate_tenant_name, validate_workspace_name, NameError,
    NAME_REGEX, RESERVED_TENANT_NAMES,
};
pub use handler::{build_router, build_user_api_router, cache_key, check, fetch};
pub use metrics::{Metrics, MetricsSnapshot};

/// Construct an [`AppState`] from a pre-built [`AuthState`] using
/// the in-tree TTL defaults.
///
/// Round 1b: the auth-less constructor is gone. Every `AppState`
/// the broker produces carries a live `AuthState`; the legacy
/// bearer-as-vault-password path that the previous
/// `build_app_state(vault_root, enforce_minimum)` shape was
/// designed to keep alive is no longer wired.
pub fn build_app_state(vault_root: PathBuf, auth: AuthState) -> AppState {
    AppState::with_auth(vault_root, auth)
}

/// Construct an [`AppState`] using an explicit [`TtlConfig`].
pub fn build_app_state_with_ttl_config(
    vault_root: PathBuf,
    auth: AuthState,
    ttl_config: TtlConfig,
) -> AppState {
    AppState::with_auth_and_ttl_config(vault_root, auth, ttl_config)
}
