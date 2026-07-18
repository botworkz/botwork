//! `bw logout` — keyring-only entry removal.
//!
//! v0 doesn't call any admin revoke endpoint (none exists yet). When
//! the broker grows `/admin/api/v1/leases/{id}` / equivalent,
//! `logout --revoke` lands as a follow-up and the body of `run`
//! gains a server-side leg.

use crate::error::LoginError;
use crate::keyring_store::KeyringStore;

/// Typed args for `logout`.
#[derive(Debug, Clone, Default)]
pub struct LogoutArgs {
    /// Tenant name (required).
    pub tenant: String,
}

/// Run the `logout` subcommand. Returns the success message; errors
/// propagate.
pub async fn run(args: LogoutArgs) -> Result<String, LoginError> {
    let removed = KeyringStore::new().delete(&args.tenant)?;
    if removed {
        Ok(format!("✓ Removed keyring entry for {}.", args.tenant))
    } else {
        // No-op is not an error — `logout` is meant to be idempotent
        // so the user can run it confidently without checking
        // whether they were logged in.
        Ok(format!("(no keyring entry for {})", args.tenant))
    }
}
