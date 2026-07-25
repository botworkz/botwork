//! `auth::invitation` — OTP generation, hashing, and DB CRUD for tenant
//! claim credentials (botworkz/botwork#346).
//!
//! ## Responsibilities
//!
//! 1. **OTP generation** — [`generate_otp`] produces a UUID-formatted random
//!    token (UUID v4, uppercase with dashes: `A1B2-…`). UUID v4 gives 122
//!    bits of randomness; the admin-relayed-to-human format is 36 chars.
//!
//! 2. **Normalisation + hashing** — [`normalize_otp`] strips dashes and
//!    whitespace, uppercases; [`hash_otp`] computes SHA-256 of the
//!    normalised form. The hash is stored; plaintext is returned once and
//!    never persisted. Normalisation lets the tenant type the OTP in any
//!    case and with or without separating dashes.
//!
//! 3. **Insert** — [`insert_invitation`] INSERTs a row with the given hash
//!    and expiry, returning the new invitation UUID.
//!
//! 4. **Verify + consume** — [`verify_and_consume`] finds the row, checks
//!    expiry + consumed state, and atomically UPDATEs `consumed_at`. Returns
//!    structured errors so the register endpoint can map to appropriate
//!    HTTP responses.
//!
//! ## Default TTL
//!
//! [`INVITATION_DEFAULT_TTL_SECONDS`] (7 days). Generous but bounded — the
//! admin hands the OTP to a human out-of-band, so a sub-hour TTL would be
//! operationally painful. The TTL is enforced at verify time regardless of
//! whether a GC reaper is running (fast-follow, not v1).

use botwork_entity::invitation;
use chrono::{DateTime, Utc};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DbErr, EntityTrait,
    QueryFilter,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Default TTL for new invitations: 7 days.
pub const INVITATION_DEFAULT_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;

// ---------------------------------------------------------------------------
// OTP generation and hashing
// ---------------------------------------------------------------------------

/// Generate a fresh random OTP.
///
/// Format: UUID v4 uppercased (`A1B2C3D4-E5F6-7890-ABCD-EF1234567890`).
/// 36 chars with dashes; typeable, unambiguous, 122 bits of randomness.
/// The admin relays this string to the tenant out-of-band.
pub fn generate_otp() -> String {
    Uuid::new_v4().to_string().to_uppercase()
}

/// Normalise an OTP string before hashing: strip dashes and whitespace,
/// uppercase.
///
/// This allows the tenant to type the OTP in any case and with or without
/// separator dashes — the hash comparison stays correct.
pub fn normalize_otp(otp: &str) -> String {
    otp.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect::<String>()
        .to_uppercase()
}

/// Hash a (possibly un-normalised) OTP using SHA-256 of its normalised form.
/// Returns a 64-char lowercase hex string.
///
/// Called at both mint time (to store the hash) and verify time (to compare).
pub fn hash_otp(otp: &str) -> String {
    use std::fmt::Write as _;
    let normalised = normalize_otp(otp);
    let digest = Sha256::digest(normalised.as_bytes());
    let mut out = String::with_capacity(64);
    for b in digest.iter() {
        write!(out, "{b:02x}").unwrap();
    }
    out
}

// ---------------------------------------------------------------------------
// DB operations
// ---------------------------------------------------------------------------

/// Error variants for the verify-and-consume step.
#[derive(Debug)]
pub enum OtpVerifyError {
    /// No matching invitation found for the tenant, or the OTP hash does not
    /// match any invitation row. Treated as opaque "invalid OTP" to avoid
    /// leaking enumeration information.
    InvalidOtp,
    /// A matching invitation was found but its `expires_at` is in the past.
    Expired,
    /// A matching invitation was found but `consumed_at` is already set
    /// (single-use invariant; typically a replay or race).
    AlreadyConsumed,
    /// Underlying database error.
    Db(DbErr),
}

impl std::fmt::Display for OtpVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidOtp => write!(f, "invalid or unknown OTP"),
            Self::Expired => write!(f, "invitation OTP has expired"),
            Self::AlreadyConsumed => write!(f, "invitation OTP has already been used"),
            Self::Db(err) => write!(f, "database error: {err}"),
        }
    }
}

/// INSERT a new `invitation` row.
///
/// `otp_hash` is the caller-computed SHA-256 hex string (see [`hash_otp`]).
/// Returns the UUID of the newly inserted row.
pub async fn insert_invitation(
    db: &impl ConnectionTrait,
    tenant_id: Uuid,
    otp_hash: &str,
    expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Uuid, DbErr> {
    let id = Uuid::new_v4();
    invitation::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant_id),
        otp_hash: Set(otp_hash.to_owned()),
        expires_at: Set(expires_at),
        consumed_at: Set(None),
        created_at: Set(now),
    }
    .insert(db)
    .await?;
    Ok(id)
}

/// Returns `true` if the tenant has at least one unconsumed, unexpired
/// invitation row.
pub async fn has_active_invitation(
    db: &impl ConnectionTrait,
    tenant_id: Uuid,
    now: DateTime<Utc>,
) -> Result<bool, DbErr> {
    let row = invitation::Entity::find()
        .filter(invitation::Column::TenantId.eq(tenant_id))
        .filter(invitation::Column::ConsumedAt.is_null())
        .filter(invitation::Column::ExpiresAt.gt(now))
        .one(db)
        .await?;
    Ok(row.is_some())
}

/// Verify an OTP for a tenant and atomically mark the matching invitation
/// as consumed.
///
/// # Algorithm
///
/// 1. SELECT the invitation for the tenant whose `otp_hash` matches
///    `hash_otp(otp)`.
/// 2. Check `expires_at` → [`OtpVerifyError::Expired`].
/// 3. Check `consumed_at IS NULL` → [`OtpVerifyError::AlreadyConsumed`].
/// 4. UPDATE `consumed_at = now` WHERE `id = row.id AND consumed_at IS NULL`.
///    If 0 rows are updated (concurrent consumption) →
///    [`OtpVerifyError::AlreadyConsumed`].
///
/// A `NOT NULL consumed_at` in step 3 is reported as `AlreadyConsumed`
/// rather than `InvalidOtp` so the caller (register endpoint) can give a
/// specific error message without revealing OTP validity to an eavesdropper.
///
/// # Security
///
/// Only the hash is compared; the plaintext OTP is never stored or logged.
pub async fn verify_and_consume(
    db: &impl ConnectionTrait,
    tenant_id: Uuid,
    otp: &str,
    now: DateTime<Utc>,
) -> Result<(), OtpVerifyError> {
    let otp_hash = hash_otp(otp);

    // Step 1: find the row matching (tenant_id, otp_hash).
    let row = invitation::Entity::find()
        .filter(invitation::Column::TenantId.eq(tenant_id))
        .filter(invitation::Column::OtpHash.eq(&otp_hash))
        .one(db)
        .await
        .map_err(OtpVerifyError::Db)?;

    let row = match row {
        Some(r) => r,
        None => return Err(OtpVerifyError::InvalidOtp),
    };

    // Step 2: check expiry.
    if row.expires_at <= now {
        return Err(OtpVerifyError::Expired);
    }

    // Step 3: check not already consumed.
    if row.consumed_at.is_some() {
        return Err(OtpVerifyError::AlreadyConsumed);
    }

    // Step 4: atomic consume — UPDATE only if consumed_at IS still NULL
    // (guards against a concurrent consume race).
    let affected = invitation::Entity::update_many()
        .col_expr(invitation::Column::ConsumedAt, Expr::value(now))
        .filter(invitation::Column::Id.eq(row.id))
        .filter(invitation::Column::ConsumedAt.is_null())
        .exec(db)
        .await
        .map_err(OtpVerifyError::Db)?;

    if affected.rows_affected == 0 {
        // Lost a consume race.
        return Err(OtpVerifyError::AlreadyConsumed);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_otp_strips_dashes_and_whitespace() {
        let otp = "550e8400-e29b-41d4-a716-446655440000";
        let normalised = normalize_otp(otp);
        assert_eq!(normalised, "550E8400E29B41D4A716446655440000");
    }

    #[test]
    fn normalize_otp_idempotent_on_uppercase_no_dashes() {
        let otp = "550E8400E29B41D4A716446655440000";
        assert_eq!(normalize_otp(otp), otp);
    }

    #[test]
    fn hash_otp_is_deterministic_across_formats() {
        // Lowercase + dashes, uppercase + dashes, no dashes — all must hash
        // identically after normalisation.
        let otp_lower = "550e8400-e29b-41d4-a716-446655440000";
        let otp_upper = "550E8400-E29B-41D4-A716-446655440000";
        let otp_nodash = "550E8400E29B41D4A716446655440000";
        assert_eq!(hash_otp(otp_lower), hash_otp(otp_upper));
        assert_eq!(hash_otp(otp_upper), hash_otp(otp_nodash));
    }

    #[test]
    fn hash_otp_returns_hex_sha256() {
        let hash = hash_otp("SOME-OTP");
        // SHA-256 produces 32 bytes = 64 hex chars.
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        // Must be lowercase.
        assert_eq!(hash, hash.to_lowercase());
    }

    #[test]
    fn generate_otp_produces_uuid_format() {
        let otp = generate_otp();
        // UUID v4 format: 8-4-4-4-12 chars with dashes = 36 total.
        assert_eq!(otp.len(), 36);
        let parts: Vec<&str> = otp.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert!(otp
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '-' || c.is_ascii_digit()));
    }
}
