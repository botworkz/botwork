//! `auth::lease` — typed wrapper around the [`botwork-entity::lease`][lease-schema]
//! SeaORM model, plus the helpers `/auth/login/finish` and
//! `/auth/check` use to insert, validate, and slide lease rows.
//!
//! [lease-schema]: https://github.com/botworkz/botwork/blob/main/db/entity/src/lease.rs
//!
//! Wraps `Model` / `ActiveModel` / `Entity` so the call sites speak in
//! typed values (`Bearer`, `BearerHash`, `LeaseId`, `WrappedExportKey`)
//! rather than `&[u8]` / `Vec<u8>` slices. Pure DB layer: no HTTP
//! types, no OPAQUE types, no in-process state. The bearer-derived
//! KEK plumbing for `wrapped_export_key` lives in
//! [`super::lease_kek`]; this module treats those bytes as opaque
//! payload at insert time and recovers the SessionKey during
//! validation.
//!
//! ## Defaults
//!
//! - [`LEASE_DEFAULT_SECONDS`] = 7d = 604_800. Matches the issue
//!   body and the `lease_seconds_requested` default on
//!   `/auth/login/start`.
//! - [`LEASE_HARD_CAP_SECONDS`] = 30d = 2_592_000. v0 ceiling.
//!   Per-tenant policy (column on `tenant`, separate
//!   `tenant_policy` table, …) is parking-lot follow-up.
//! - [`LEASE_IDLE_WINDOW_SECONDS`] = 1h = 3_600. On each
//!   successful `/auth/check`, `idle_extends_to` is bumped to
//!   `min(expires_at, now + idle_window)`. A user who returns
//!   within the window gets a transparent extension; a user away
//!   for more than an hour has to re-login.

use std::ops::Add;

use botwork_entity::lease;
use chrono::{DateTime, Duration, Utc};
use rand::rngs::SysRng;
use rand::TryRng;
use sea_orm::{
    sea_query::{Expr, OnConflict},
    ActiveModelTrait, ActiveValue, ColumnTrait, ConnectionTrait, DatabaseTransaction, DbErr,
    EntityTrait, IntoActiveModel, QueryFilter, Set, TransactionTrait,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::Zeroizing;

/// Default lease window the client gets when it omits
/// `lease_seconds_requested`. Equals 7 days.
pub const LEASE_DEFAULT_SECONDS: u64 = 7 * 86_400;
/// Hard server-side ceiling on `lease_seconds_requested`. Equals 30
/// days. v0 carries no per-tenant override.
pub const LEASE_HARD_CAP_SECONDS: u64 = 30 * 86_400;
/// Idle-window the sliding-extension uses on each successful
/// `/auth/check`. Equals 1 hour.
pub const LEASE_IDLE_WINDOW_SECONDS: u64 = 3_600;
/// Random byte length for the bearer token. `SysRng`-sourced; encoded
/// as url-safe base64-no-pad for transport.
pub const BEARER_BYTES: usize = 32;
/// SHA-256 digest length, in bytes. Matches the `bearer_hash` column
/// shape in the schema.
pub const BEARER_HASH_LEN: usize = 32;

/// Typed wrapper around the raw 32-byte bearer the
/// `/auth/login/finish` handler mints and the client carries in
/// `Authorization: Bearer`. The plaintext bytes never reach
/// postgres — see [`BearerHash`] for what we actually store.
pub struct Bearer {
    bytes: Zeroizing<[u8; BEARER_BYTES]>,
}

impl Bearer {
    /// Generate a fresh bearer from the OS CSPRNG.
    fn from_bytes(bytes: [u8; BEARER_BYTES]) -> Self {
        Self {
            bytes: Zeroizing::new(bytes),
        }
    }

    pub fn generate() -> Self {
        let mut bytes = [0u8; BEARER_BYTES];
        let mut rng = SysRng;
        rng.try_fill_bytes(&mut bytes)
            .expect("failed to generate bearer token bytes from system RNG");
        Self::from_bytes(bytes)
    }

    /// Construct a bearer from previously-observed bytes (e.g. the
    /// `Authorization: Bearer <token>` header on `/auth/check`).
    /// Returns an error unless the input is exactly 32 bytes.
    pub fn try_from_slice(value: &[u8]) -> Result<Self, BearerLenError> {
        let bytes = value
            .try_into()
            .map_err(|_| BearerLenError::InvalidLength {
                observed: value.len(),
            })?;
        Ok(Self::from_bytes(bytes))
    }

    /// Borrow the raw bytes. Callers that encode the bearer for
    /// transport should immediately base64 it and drop this
    /// reference so the plaintext doesn't linger.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_ref()
    }

    /// Compute the SHA-256 of the bearer — the value the lease
    /// table actually stores in `bearer_hash` and the value
    /// `/auth/check` looks up by.
    pub fn hash(&self) -> BearerHash {
        BearerHash::from_bearer_bytes(self.bytes.as_ref())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BearerLenError {
    #[error("invalid bearer length: expected {BEARER_BYTES} bytes, got {observed}")]
    InvalidLength { observed: usize },
}

/// SHA-256 of a bearer. Newtype to keep callers from confusing it
/// with the raw 32-byte bearer.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct BearerHash([u8; BEARER_HASH_LEN]);

impl BearerHash {
    /// Compute the hash from any byte slice that decodes to a
    /// bearer. Used by `/auth/check` against the incoming
    /// `Authorization: Bearer …` value.
    pub fn from_bearer_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hasher.finalize();
        let mut out = [0u8; BEARER_HASH_LEN];
        out.copy_from_slice(&digest);
        Self(out)
    }

    /// Borrow the digest for use in SeaORM filters / inserts.
    pub fn as_bytes(&self) -> &[u8; BEARER_HASH_LEN] {
        &self.0
    }

    /// Owned `Vec<u8>` view, used at the schema seam where SeaORM
    /// expects a `Vec<u8>` for `bytea` columns.
    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl std::fmt::Debug for BearerHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hashes are not secrets per se but a Debug print landing
        // in a log is still useless noise — collapse to the
        // truncated form auth-broker already uses for bearers.
        write!(f, "BearerHash({}…)", hex_prefix(&self.0, 6))
    }
}

fn hex_prefix(bytes: &[u8], chars: usize) -> String {
    let mut out = String::with_capacity(chars);
    for byte in bytes.iter().take(chars.div_ceil(2)) {
        out.push_str(&format!("{byte:02x}"));
        if out.len() >= chars {
            break;
        }
    }
    out.truncate(chars);
    out
}

/// Owned, opaque wrapper around the bytes stored in
/// `lease.wrapped_export_key`. Construction is via
/// [`super::lease_kek::wrap_session_key`]; the unwrap step happens
/// at validate time inside [`validate_and_extend`].
pub struct WrappedExportKey(pub Vec<u8>);

impl WrappedExportKey {
    /// View as a byte slice for SeaORM inserts.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// UUID of a row in the `lease` table.
pub type LeaseId = Uuid;

/// Lease row as returned by [`insert_lease`] / [`validate_and_extend`].
///
/// Lightweight projection over `lease::Model` that hides the
/// `bearer_hash` / `wrapped_export_key` byte buffers from callers
/// that only want the (`id`, `tenant_id`, `expires_at`) trio (the
/// `/auth/check` and `/auth/login/finish` hot paths).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseRow {
    pub id: LeaseId,
    pub tenant_id: Uuid,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub idle_extends_to: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl From<lease::Model> for LeaseRow {
    fn from(m: lease::Model) -> Self {
        Self {
            id: m.id,
            tenant_id: m.tenant_id,
            issued_at: m.issued_at,
            expires_at: m.expires_at,
            idle_extends_to: m.idle_extends_to,
            revoked_at: m.revoked_at,
        }
    }
}

/// Cap the requested lease window at [`LEASE_HARD_CAP_SECONDS`].
///
/// Pulled out as a free function so the test suite can pin the cap
/// semantics without a postgres round-trip.
pub fn cap_lease_seconds(requested: u64) -> u64 {
    requested.min(LEASE_HARD_CAP_SECONDS)
}

/// Insert a fresh lease row.
///
/// Caller has already minted [`Bearer`] + [`WrappedExportKey`] and
/// has the parent `tenant_id` in hand. Returns the projected
/// [`LeaseRow`].
///
/// Conflict handling: the `ux_lease_bearer_hash` UNIQUE index on
/// `bearer_hash` makes a same-hash insert race-safe. A duplicate
/// surfaces as a [`DbErr::RecordNotInserted`] equivalent — the
/// handler maps that to a 500 because a bearer collision means our
/// RNG is broken, not anything the client can fix.
pub async fn insert_lease<C: ConnectionTrait>(
    conn: &C,
    tenant_id: Uuid,
    bearer_hash: &BearerHash,
    wrapped_export_key: &WrappedExportKey,
    lease_seconds: u64,
    now: DateTime<Utc>,
) -> Result<LeaseRow, DbErr> {
    let capped = cap_lease_seconds(lease_seconds);
    let expires_at = now.add(Duration::seconds(capped as i64));
    let idle_extends_to = compute_idle_extends_to(now, expires_at);

    let model = lease::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        tenant_id: Set(tenant_id),
        bearer_hash: Set(bearer_hash.to_vec()),
        wrapped_export_key: Set(wrapped_export_key.as_bytes().to_vec()),
        issued_at: Set(now),
        expires_at: Set(expires_at),
        idle_extends_to: Set(idle_extends_to),
        revoked_at: Set(None),
    };

    let inserted = model.insert(conn).await?;
    Ok(LeaseRow::from(inserted))
}

/// Look up a lease by `bearer_hash`, validate that it is live,
/// extend `idle_extends_to`, and return the unwrapped export key
/// alongside the lease row metadata.
///
/// Returns `Ok(None)` for the *expected* miss cases:
///   - row not found
///   - row found, live by both timestamps, but
///     [`super::lease_kek::unwrap_session_key`] rejects the
///     `wrapped_export_key` (wrong bearer or a stale pre-cutover row).
///
/// Returns `Err` for the expired-by-timestamp cases so the caller
/// can pick a distinct `ErrorCode::ExpiredLease`. The split is
/// what the structured-401 wire contract from #125 expects:
/// `ErrorCode::InvalidBearer` for everything in the `None` arm,
/// `ErrorCode::ExpiredLease` for everything in the `Err(_Expired)` arm.
pub async fn validate_and_extend(
    db: &sea_orm::DatabaseConnection,
    bearer: &Bearer,
    now: DateTime<Utc>,
) -> Result<Option<ValidatedLease>, ValidationError> {
    let txn = db.begin().await.map_err(ValidationError::Db)?;
    let result = validate_and_extend_inner(&txn, bearer, now).await;
    match &result {
        Ok(_) => txn.commit().await.map_err(ValidationError::Db)?,
        Err(_) => {
            // Best-effort rollback; we propagate the original error
            // from `result` either way.
            let _ = txn.rollback().await;
        }
    }
    result
}

/// Errors returned by [`validate_and_extend`].
///
/// `Expired` distinguishes "row exists but past `expires_at` /
/// `idle_extends_to`" from "row doesn't exist or doesn't decrypt",
/// so the HTTP handler can map onto `ErrorCode::ExpiredLease` vs
/// `ErrorCode::InvalidBearer` without re-querying.
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("lease expired")]
    Expired,
    #[error("lease revoked")]
    Revoked,
    #[error("database error: {0}")]
    Db(#[from] DbErr),
}

/// Result of a successful [`validate_and_extend`]. Carries the
/// recovered per-lease SessionKey bytes (`Zeroizing` so they're
/// wiped on drop) plus the lease metadata.
pub struct ValidatedLease {
    pub lease: LeaseRow,
    pub export_key: Zeroizing<Vec<u8>>,
}

async fn validate_and_extend_inner(
    txn: &DatabaseTransaction,
    bearer: &Bearer,
    now: DateTime<Utc>,
) -> Result<Option<ValidatedLease>, ValidationError> {
    let bearer_hash = bearer.hash();
    let model = lease::Entity::find()
        .filter(lease::Column::BearerHash.eq(bearer_hash.to_vec()))
        .one(txn)
        .await
        .map_err(ValidationError::Db)?;

    let Some(model) = model else {
        return Ok(None);
    };

    if model.revoked_at.is_some() {
        return Err(ValidationError::Revoked);
    }
    if now >= model.expires_at || now >= model.idle_extends_to {
        return Err(ValidationError::Expired);
    }

    let export_key = match crate::auth::lease_kek::unwrap_session_key(
        bearer.as_bytes(),
        &model.wrapped_export_key,
    ) {
        Ok(bytes) => bytes,
        Err(_) => {
            // Two expected miss cases collapse here:
            //   (a) wrong bearer for this row (including the
            //       vanishingly-improbable SHA-256 collision case),
            //   (b) stale row written by the deleted server-global
            //       wrapping-key design before cutover.
            // Caller treats both as `invalid_bearer`.
            return Ok(None);
        }
    };

    // Sliding extension: bump idle_extends_to to
    // `min(expires_at, now + idle_window)`. Bounded by the absolute
    // expires_at so the window can never extend past the hard ceiling.
    let candidate = compute_idle_extends_to(now, model.expires_at);
    let new_idle = candidate.max(model.idle_extends_to);

    let row = LeaseRow::from(model.clone());
    let mut active = model.into_active_model();
    active.idle_extends_to = Set(new_idle);
    active.update(txn).await.map_err(ValidationError::Db)?;

    Ok(Some(ValidatedLease {
        lease: LeaseRow {
            idle_extends_to: new_idle,
            ..row
        },
        export_key,
    }))
}

fn compute_idle_extends_to(now: DateTime<Utc>, expires_at: DateTime<Utc>) -> DateTime<Utc> {
    let window = Duration::seconds(LEASE_IDLE_WINDOW_SECONDS as i64);
    let candidate = now.add(window);
    candidate.min(expires_at)
}

/// Mark a lease as revoked. Sets `revoked_at = now()` for the row
/// whose `bearer_hash` matches. Idempotent — calling on an
/// already-revoked row keeps `revoked_at` unchanged.
///
/// Called by both the self-service `POST /api/auth/logout` endpoint
/// (bearer known from the request) and the admin
/// `DELETE /admin/api/v1/leases/:id` endpoint (via [`revoke_by_id`]).
pub async fn revoke<C: ConnectionTrait>(
    conn: &C,
    bearer_hash: &BearerHash,
    now: DateTime<Utc>,
) -> Result<u64, DbErr> {
    let res = lease::Entity::update_many()
        .col_expr(lease::Column::RevokedAt, Expr::value(Some(now)))
        .filter(lease::Column::BearerHash.eq(bearer_hash.to_vec()))
        .filter(lease::Column::RevokedAt.is_null())
        .exec(conn)
        .await?;
    Ok(res.rows_affected)
}

/// Mark a lease as revoked by its primary key (`id`). Idempotent —
/// calling on an already-revoked row keeps `revoked_at` unchanged.
///
/// Used by the admin `DELETE /admin/api/v1/leases/:id` endpoint where
/// the operator knows only the lease UUID, not the raw bearer token.
pub async fn revoke_by_id<C: ConnectionTrait>(
    conn: &C,
    lease_id: Uuid,
    now: DateTime<Utc>,
) -> Result<u64, DbErr> {
    let res = lease::Entity::update_many()
        .col_expr(lease::Column::RevokedAt, Expr::value(Some(now)))
        .filter(lease::Column::Id.eq(lease_id))
        .filter(lease::Column::RevokedAt.is_null())
        .exec(conn)
        .await?;
    Ok(res.rows_affected)
}

/// Insert with explicit conflict handling — sea-query is happy to
/// emit `ON CONFLICT DO NOTHING` when we want it. v0 doesn't need
/// this because `ux_lease_bearer_hash` covers the race-safety
/// posture, but the helper exists so a future admin-API "rotate
/// bearer for the same lease" can swap rows without re-doing the
/// FK plumbing.
///
/// Marked `#[allow(dead_code)]` because there's no current caller;
/// removed when the rotation API lands.
#[allow(dead_code)]
pub async fn insert_lease_ignoring_conflict<C: ConnectionTrait>(
    conn: &C,
    tenant_id: Uuid,
    bearer_hash: &BearerHash,
    wrapped_export_key: &WrappedExportKey,
    lease_seconds: u64,
    now: DateTime<Utc>,
) -> Result<(), DbErr> {
    let capped = cap_lease_seconds(lease_seconds);
    let expires_at = now.add(Duration::seconds(capped as i64));
    let idle_extends_to = compute_idle_extends_to(now, expires_at);

    let model = lease::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        tenant_id: Set(tenant_id),
        bearer_hash: Set(bearer_hash.to_vec()),
        wrapped_export_key: Set(wrapped_export_key.as_bytes().to_vec()),
        issued_at: Set(now),
        expires_at: Set(expires_at),
        idle_extends_to: Set(idle_extends_to),
        revoked_at: Set(None),
    };
    lease::Entity::insert(model)
        .on_conflict(
            OnConflict::column(lease::Column::BearerHash)
                .do_nothing()
                .to_owned(),
        )
        .exec(conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_lease_seconds_pins_default_and_hard_cap() {
        assert_eq!(cap_lease_seconds(0), 0);
        assert_eq!(
            cap_lease_seconds(LEASE_DEFAULT_SECONDS),
            LEASE_DEFAULT_SECONDS
        );
        assert_eq!(
            cap_lease_seconds(LEASE_HARD_CAP_SECONDS),
            LEASE_HARD_CAP_SECONDS
        );
        assert_eq!(
            cap_lease_seconds(LEASE_HARD_CAP_SECONDS + 1),
            LEASE_HARD_CAP_SECONDS
        );
        assert_eq!(cap_lease_seconds(u64::MAX), LEASE_HARD_CAP_SECONDS);
    }

    #[test]
    fn bearer_hash_is_sha256_of_bytes() {
        // Cross-check against a freshly computed SHA-256 so a future
        // refactor that swaps the hash function trips this test.
        let bearer = b"deterministic-bearer";
        let h = BearerHash::from_bearer_bytes(bearer);
        let mut hasher = Sha256::new();
        hasher.update(bearer);
        let expected = hasher.finalize();
        assert_eq!(h.as_bytes(), &expected[..]);
    }

    #[test]
    fn bearer_round_trips_through_try_from_slice() {
        let original = Bearer::generate();
        let copy = Bearer::try_from_slice(original.as_bytes()).expect("exact-length bearer");
        assert_eq!(original.hash().as_bytes(), copy.hash().as_bytes());
    }

    #[test]
    fn bearer_try_from_slice_rejects_short_and_long_lengths() {
        let short = [0u8; BEARER_BYTES - 1];
        let long = [0u8; BEARER_BYTES + 1];

        assert!(matches!(
            Bearer::try_from_slice(&short),
            Err(BearerLenError::InvalidLength { observed }) if observed == short.len()
        ));
        assert!(matches!(
            Bearer::try_from_slice(&long),
            Err(BearerLenError::InvalidLength { observed }) if observed == long.len()
        ));
    }

    #[test]
    fn compute_idle_extends_caps_at_expires_at() {
        let now = Utc::now();
        let expires = now + Duration::seconds(120);
        // window 3600s, expires in 120s → cap at expires.
        assert_eq!(compute_idle_extends_to(now, expires), expires);
        // expires far away → cap at window.
        let far = now + Duration::seconds(86400);
        assert_eq!(
            compute_idle_extends_to(now, far),
            now + Duration::seconds(LEASE_IDLE_WINDOW_SECONDS as i64)
        );
    }

    #[test]
    fn bearer_hash_debug_does_not_print_full_digest() {
        let h = BearerHash::from_bearer_bytes(b"x");
        let dbg = format!("{h:?}");
        assert!(dbg.starts_with("BearerHash("));
        assert!(dbg.ends_with("…)"));
        // The full 64-char hex digest must not be present.
        let full = h
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        assert!(!dbg.contains(&full));
    }
}
