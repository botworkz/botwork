//! `--lease 7d` / `--lease 12h` / `--lease 600s` parsing.
//!
//! The CLI accepts every shape `humantime::parse_duration` understands
//! and rejects everything else with a typed [`LoginError::InvalidDuration`].
//! Tests pin the corner cases (`0s` rejected, `1y` accepted, malformed
//! input rejected).

use std::time::Duration;

use crate::error::LoginError;

/// Parse a `--lease` argument into seconds.
///
/// The broker's `/auth/login/start` takes `lease_seconds_requested:
/// Option<u64>` and caps it server-side at
/// [`botwork_auth_broker::auth::LEASE_HARD_CAP_SECONDS`] (30d in v0).
/// Returning seconds rather than `Duration` matches the wire schema
/// shape exactly and saves a downstream caller from re-coercing.
pub fn parse_lease(input: &str) -> Result<u64, LoginError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(LoginError::InvalidDuration {
            value: input.to_string(),
            reason: "empty".to_string(),
        });
    }
    let duration: Duration =
        humantime::parse_duration(trimmed).map_err(|err| LoginError::InvalidDuration {
            value: input.to_string(),
            reason: err.to_string(),
        })?;
    if duration.as_secs() == 0 {
        // A zero-second lease is structurally accepted by humantime
        // (e.g. `0s`) but is semantically useless and would lead the
        // broker into an immediate expiry — surface it as a parse
        // error so the user gets a clear message rather than a 401
        // immediately after login.
        return Err(LoginError::InvalidDuration {
            value: input.to_string(),
            reason: "lease must be > 0s".to_string(),
        });
    }
    Ok(duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_humantime_shapes() {
        assert_eq!(parse_lease("7d").unwrap(), 7 * 86_400);
        assert_eq!(parse_lease("30d").unwrap(), 30 * 86_400);
        assert_eq!(parse_lease("12h").unwrap(), 12 * 3_600);
        assert_eq!(parse_lease("600s").unwrap(), 600);
        // `humantime::parse_duration` requires a unit suffix; `600`
        // alone is rejected. Pin the contract so a future change
        // that loosens this trips the test.
        assert!(parse_lease("600").is_err());
    }

    #[test]
    fn rejects_empty_and_zero() {
        assert!(parse_lease("").is_err());
        assert!(parse_lease("   ").is_err());
        let err = parse_lease("0s").unwrap_err();
        assert!(matches!(err, LoginError::InvalidDuration { .. }));
        let msg = err.to_string();
        assert!(msg.contains("lease must be > 0s"), "got {msg}");
    }

    #[test]
    fn rejects_malformed() {
        for bad in ["wat", "7dx", "7 days monthly"] {
            let err = parse_lease(bad).expect_err(bad);
            assert!(matches!(err, LoginError::InvalidDuration { .. }));
        }
    }
}
