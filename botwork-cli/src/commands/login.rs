//! `bw login` — drive an OPAQUE login, persist the bearer
//! to the OS keyring.
//!
//! Wire contract from the broker's `/auth/login/start` +
//! `/auth/login/finish` endpoints; the OPAQUE state machine is run
//! locally via [`botwork_opaque_handshake::client`].

use chrono::Utc;
use std::path::PathBuf;
use zeroize::Zeroizing;

use crate::client::run_login as drive_login;
use crate::commands::input::PasswordSource;
use crate::commands::warn_if_insecure_server;
use crate::config::{Config, DEFAULT_LEASE_SECONDS};
use crate::duration::parse_lease;
use crate::error::LoginError;
use crate::keyring_store::{KeyringEntry, KeyringStore};
use botwork_opaque_handshake::SUITE_VERSION;

/// Typed args for `login`. Held as a struct (rather than the bare
/// `clap`-derived enum variant) so the library entry point can be
/// called without going through `clap`.
#[derive(Debug, Default)]
pub struct LoginArgs {
    /// Tenant name (required).
    pub tenant: String,
    /// `--lease 7d` / `--lease 12h`. `None` = use [`DEFAULT_LEASE_SECONDS`].
    pub lease: Option<String>,
    /// OPAQUE credential identifier override.
    pub credential_identifier: Option<String>,
    /// Server URL override.
    pub server: Option<String>,
    /// Additional PEM CA bundle path.
    pub cacert: Option<PathBuf>,
    /// Read the password from stdin (one line, no echo).
    pub password_stdin: bool,
    /// Library-supplied password. When `Some`, the prompt /
    /// `--password-stdin` paths are skipped entirely — this is how
    /// a non-CLI caller (web UI, integration test, future admin
    /// tool) supplies the password without going through tty
    /// magic.
    ///
    /// The bytes are owned + zeroized on drop so a caller can hand
    /// them in once and let the buffer go.
    pub password: Option<Zeroizing<Vec<u8>>>,
}

/// Run the `login` subcommand. Returns user-facing text the CLI
/// shim prints to stdout on success; errors propagate.
pub async fn run(args: LoginArgs) -> Result<String, LoginError> {
    let config = Config::load()?;
    let resolved = config.resolve(
        &args.tenant,
        args.server.as_deref(),
        args.credential_identifier.as_deref(),
    )?;

    // Password is about to travel over this channel. If it isn't TLS,
    // warn loudly before we send anything.
    warn_if_insecure_server(&resolved.server);

    let lease_seconds = match args.lease.as_deref() {
        Some(value) => parse_lease(value)?,
        None => DEFAULT_LEASE_SECONDS,
    };

    let password = match args.password {
        Some(bytes) => bytes,
        None => PasswordSource::for_login(args.password_stdin).read()?,
    };

    let outcome = drive_login(
        &resolved.server,
        &args.tenant,
        &resolved.credential_identifier,
        password.as_slice(),
        lease_seconds,
        args.cacert.as_deref(),
    )
    .await?;

    let now = Utc::now();
    let entry = KeyringEntry {
        bearer: outcome.bearer.as_str().to_string(),
        lease_id: outcome.lease_id,
        expires_at: outcome.expires_at,
        server: resolved.server.to_string(),
        credential_identifier: resolved.credential_identifier.clone(),
        suite_version: SUITE_VERSION,
    };
    KeyringStore::new().write(&args.tenant, &entry)?;

    // Drop the bearer view explicitly so the Zeroizing wrapper wipes
    // the buffer before this function returns.
    drop(outcome.bearer);

    Ok(format_success(&args.tenant, &entry, now))
}

fn format_success(tenant: &str, entry: &KeyringEntry, now: chrono::DateTime<Utc>) -> String {
    let remaining = entry
        .expires_at
        .signed_duration_since(now)
        .to_std()
        .unwrap_or_default();
    format!(
        "✓ Logged in to {tenant}. Lease expires {expires} (in {remaining}).",
        expires = entry.expires_at.to_rfc3339(),
        remaining = humantime::format_duration(round_seconds(remaining)),
    )
}

fn round_seconds(d: std::time::Duration) -> std::time::Duration {
    // `humantime::format_duration` happily prints microseconds; round
    // to seconds so the user-visible message stays readable.
    std::time::Duration::from_secs(d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_success_emits_expected_shape() {
        let now = Utc::now();
        let entry = KeyringEntry {
            bearer: "abc".into(),
            lease_id: uuid::Uuid::nil(),
            expires_at: now + chrono::Duration::seconds(7 * 86_400),
            server: "http://x:9100".into(),
            credential_identifier: "phlax".into(),
            suite_version: SUITE_VERSION,
        };
        let msg = format_success("phlax", &entry, now);
        assert!(msg.starts_with("✓ Logged in to phlax."), "got {msg}");
        assert!(msg.contains("Lease expires "));
        // Remaining should be ~7d; allow `7days` regardless of
        // humantime's spacing.
        assert!(msg.contains("7days"));
    }

    #[test]
    fn round_seconds_drops_subsecond_remainder() {
        let d = std::time::Duration::from_millis(7 * 86_400_000 + 17);
        assert_eq!(round_seconds(d).as_secs(), 7 * 86_400);
    }

    #[tokio::test]
    async fn invalid_lease_is_rejected_before_network() {
        let err = run(LoginArgs {
            tenant: "phlax".into(),
            lease: Some("definitely-not-a-duration".into()),
            server: Some("https://broker.example".into()),
            password: Some(Zeroizing::new(b"hunter2".to_vec())),
            ..LoginArgs::default()
        })
        .await
        .unwrap_err();
        assert!(
            matches!(err, LoginError::InvalidDuration { ref value, .. } if value == "definitely-not-a-duration")
        );
    }

    #[tokio::test]
    async fn invalid_server_is_rejected_during_resolution() {
        let err = run(LoginArgs {
            tenant: "phlax".into(),
            server: Some("127.0.0.1:9100".into()),
            password: Some(Zeroizing::new(b"hunter2".to_vec())),
            ..LoginArgs::default()
        })
        .await
        .unwrap_err();
        assert!(
            matches!(err, LoginError::InvalidServer { ref value, .. } if value == "127.0.0.1:9100")
        );
    }
}
