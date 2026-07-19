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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyring_store::{KeyringEntry, KeyringStore};
    use chrono::Utc;
    use std::sync::MutexGuard;
    use tempfile::TempDir;

    fn lock_env() -> MutexGuard<'static, ()> {
        crate::test_env_lock::lock_env()
    }

    fn fixture_entry() -> KeyringEntry {
        KeyringEntry {
            bearer: "token".into(),
            lease_id: uuid::Uuid::nil(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            server: "https://broker.example".into(),
            credential_identifier: "phlax".into(),
            suite_version: botwork_opaque_handshake::SUITE_VERSION,
        }
    }

    fn run_async<F: std::future::Future<Output = T>, T>(future: F) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn run_removes_existing_entry() {
        let _lock = lock_env();
        let dir = TempDir::new().unwrap();
        std::env::set_var("BOTWORK_LOGIN_KEYRING_DIR", dir.path());
        KeyringStore::new()
            .write("phlax", &fixture_entry())
            .unwrap();

        let output = run_async(run(LogoutArgs {
            tenant: "phlax".into(),
        }))
        .unwrap();
        assert_eq!(output, "✓ Removed keyring entry for phlax.");
        assert!(KeyringStore::new().read("phlax").unwrap().is_none());

        std::env::remove_var("BOTWORK_LOGIN_KEYRING_DIR");
    }

    #[test]
    fn run_is_idempotent_when_entry_is_missing() {
        let _lock = lock_env();
        let dir = TempDir::new().unwrap();
        std::env::set_var("BOTWORK_LOGIN_KEYRING_DIR", dir.path());

        let output = run_async(run(LogoutArgs {
            tenant: "phlax".into(),
        }))
        .unwrap();
        assert_eq!(output, "(no keyring entry for phlax)");

        std::env::remove_var("BOTWORK_LOGIN_KEYRING_DIR");
    }
}
