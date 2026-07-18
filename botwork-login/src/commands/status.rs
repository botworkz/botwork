//! `botwork-login status` — offline keyring-only lease introspection.
//!
//! Reads the keyring entry for `tenant`, prints expiry + remaining
//! time + lease id. Exits 1 (`NoLease` / `LeaseExpired`) when no
//! valid lease is present.

use chrono::Utc;

use crate::error::LoginError;
use crate::keyring_store::{KeyringEntry, KeyringStore};

/// Typed args for `status`.
#[derive(Debug, Clone, Default)]
pub struct StatusArgs {
    /// Tenant name (required).
    pub tenant: String,
}

/// Run the `status` subcommand. Returns user-facing text on success;
/// errors propagate.
pub async fn run(args: StatusArgs) -> Result<String, LoginError> {
    let store = KeyringStore::new();
    let entry = match store.read(&args.tenant)? {
        Some(entry) => entry,
        None => return Err(LoginError::NoLease(args.tenant)),
    };
    let now = Utc::now();
    if entry.is_expired(now) {
        return Err(LoginError::LeaseExpired {
            tenant: args.tenant,
            expires_at: entry.expires_at.to_rfc3339(),
        });
    }
    Ok(format_status(&args.tenant, &entry, now))
}

fn format_status(tenant: &str, entry: &KeyringEntry, now: chrono::DateTime<Utc>) -> String {
    let remaining = entry
        .expires_at
        .signed_duration_since(now)
        .to_std()
        .unwrap_or_default();
    let remaining_rounded = std::time::Duration::from_secs(remaining.as_secs());
    format!(
        "{tenant}: logged in. Lease expires {expires} (in {remaining}).\n       \
         Lease id: {lease}\n       Server: {server}",
        expires = entry.expires_at.to_rfc3339(),
        remaining = humantime::format_duration(remaining_rounded),
        lease = entry.lease_id,
        server = entry.server,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_status_includes_lease_id_and_server() {
        let now = Utc::now();
        let entry = KeyringEntry {
            bearer: "x".into(),
            lease_id: uuid::Uuid::from_u128(0x8f3e_4a00_0000_0000_0000_0000_0000_0001),
            expires_at: now + chrono::Duration::seconds(86_400),
            server: "http://192.168.122.50:9100".into(),
            credential_identifier: "phlax".into(),
            suite_version: botwork_opaque_handshake::SUITE_VERSION,
        };
        let msg = format_status("phlax", &entry, now);
        assert!(msg.contains("phlax: logged in."));
        assert!(msg.contains("Lease id: 8f3e4a00-0000-0000-0000-000000000001"));
        assert!(msg.contains("Server: http://192.168.122.50:9100"));
        assert!(msg.contains("1day"));
    }
}
