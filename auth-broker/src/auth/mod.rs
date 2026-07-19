//! `auth` — OPAQUE registration + login + lease validation surface
//! for `botwork-auth-broker` (#133, round 1a of #123).
//!
//! Module layout matches the issue body:
//!
//! - [`lease_kek`] — per-bearer KEK derivation + wrap/unwrap of the
//!   OPAQUE SessionKey stored in `lease.wrapped_export_key`.
//! - [`pending`] — in-process map of in-flight handshakes between
//!   `/auth/login/start` and `/auth/login/finish`.
//! - [`lease`] — typed wrapper around the SeaORM `lease` entity:
//!   insert, validate, sliding-extend.
//! - [`opaque`] — `ServerSetup` materialisation, tenant resolution,
//!   `opaque_password_file` CRUD.
//! - [`endpoints`] — the four HTTP handlers + the `/auth/check`
//!   lease-first validation path. Mounted by
//!   [`crate::handler::build_router`].
//!
//! The `/auth/check` legacy path stays *exactly* as it is in
//! `handler.rs` — the lease lookup is a *prefix* on top of the
//! existing flow. This makes round 1a additive: every existing
//! tenant continues to authenticate with the bearer-as-vault-password
//! contract, and tenants who have gone through OPAQUE registration
//! get the lease path on top.

pub mod endpoints;
pub mod lease;
pub mod lease_kek;
pub mod opaque;
pub mod pending;
pub mod rate_limit;

pub use endpoints::{build_auth_router, AuthState, LEASE_EXPORT_KEY_TTL};
pub use lease::{
    cap_lease_seconds, Bearer, BearerHash, LeaseId, LeaseRow, WrappedExportKey,
    LEASE_DEFAULT_SECONDS, LEASE_HARD_CAP_SECONDS, LEASE_IDLE_WINDOW_SECONDS,
};
pub use lease_kek::{
    derive_lease_kek, unwrap_session_key, wrap_session_key, LeaseKekError, LEASE_KEK_LEN,
    LEASE_KEK_NONCE_LEN, LEASE_KEK_TAG_LEN, MIN_LEASE_WRAPPED_LEN,
};
pub use opaque::SERVER_SETUP_FILENAME;
pub use pending::{PendingError, PendingMap, PENDING_TTL};
pub use rate_limit::{RateLimitConfig, RateLimiter};
