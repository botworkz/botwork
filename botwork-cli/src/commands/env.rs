//! `bw env` — emit `export <VAR>='<bearer>'`.
//!
//! Designed for shell consumption:
//!
//! ```sh
//! eval "$(bw env --tenant phlax)"
//! ```
//!
//! On no-lease / expired-lease, prints *nothing* to stdout (so the
//! `eval` doesn't try to `export ''=`) and an error message to stderr.
//! The CLI shim is responsible for writing stderr.

use chrono::Utc;

use crate::config::Config;
use crate::error::LoginError;
use crate::keyring_store::KeyringStore;

/// Typed args for `env`.
#[derive(Debug, Clone, Default)]
pub struct EnvArgs {
    /// Tenant name (required).
    pub tenant: String,
    /// Override the env var name. When `None`, resolves through
    /// [`Config::resolve_token_env`].
    pub token_env: Option<String>,
}

/// Run the `env` subcommand. Returns the `export <VAR>='<bearer>'`
/// line for the CLI shim to print to stdout; errors propagate so
/// stdout stays clean.
pub async fn run(args: EnvArgs) -> Result<String, LoginError> {
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
    let token_env = match args.token_env {
        Some(value) => value,
        None => Config::load()?.resolve_token_env(),
    };
    Ok(format_export(&token_env, &entry.bearer))
}

fn format_export(token_env: &str, bearer: &str) -> String {
    // Single-quote the value because the bearer is URL-safe-base64
    // (no shell-meaningful characters) so single quotes are the
    // right escape: they survive `eval` and don't interpolate.
    format!("export {token_env}='{bearer}'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyring_store::{KeyringEntry, KeyringStore};
    use std::sync::Mutex;
    use tempfile::TempDir;

    #[test]
    fn format_export_shape() {
        assert_eq!(
            format_export("BOTWORK_BEARER", "ABCDEF0123456789"),
            "export BOTWORK_BEARER='ABCDEF0123456789'"
        );
    }

    fn env_lock() -> &'static Mutex<()> {
        crate::test_env_lock::env_lock()
    }

    fn fixture_entry(expires_at: chrono::DateTime<Utc>) -> KeyringEntry {
        KeyringEntry {
            bearer: "ABCDEF0123456789".into(),
            lease_id: uuid::Uuid::nil(),
            expires_at,
            server: "https://broker.example".into(),
            credential_identifier: "phlax@example.com".into(),
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
    fn run_uses_explicit_token_env_override() {
        let _lock = env_lock().lock().unwrap();
        let dir = TempDir::new().unwrap();
        std::env::set_var("BOTWORK_LOGIN_KEYRING_DIR", dir.path());
        KeyringStore::new()
            .write(
                "phlax",
                &fixture_entry(Utc::now() + chrono::Duration::hours(1)),
            )
            .unwrap();

        let output = run_async(run(EnvArgs {
            tenant: "phlax".into(),
            token_env: Some("CUSTOM_TOKEN".into()),
        }))
        .unwrap();
        assert_eq!(output, "export CUSTOM_TOKEN='ABCDEF0123456789'");

        std::env::remove_var("BOTWORK_LOGIN_KEYRING_DIR");
    }

    #[test]
    fn run_reads_token_env_from_config_when_not_overridden() {
        let _lock = env_lock().lock().unwrap();
        let dir = TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "token_env = \"ALT_TOKEN\"\n").unwrap();
        std::env::set_var("BOTWORK_LOGIN_CONFIG", &config);
        std::env::set_var("BOTWORK_LOGIN_KEYRING_DIR", dir.path().join("keyring"));
        KeyringStore::new()
            .write(
                "phlax",
                &fixture_entry(Utc::now() + chrono::Duration::hours(1)),
            )
            .unwrap();

        let output = run_async(run(EnvArgs {
            tenant: "phlax".into(),
            token_env: None,
        }))
        .unwrap();
        assert_eq!(output, "export ALT_TOKEN='ABCDEF0123456789'");

        std::env::remove_var("BOTWORK_LOGIN_CONFIG");
        std::env::remove_var("BOTWORK_LOGIN_KEYRING_DIR");
    }

    #[test]
    fn run_errors_for_missing_or_expired_lease() {
        let _lock = env_lock().lock().unwrap();
        let dir = TempDir::new().unwrap();
        std::env::set_var("BOTWORK_LOGIN_KEYRING_DIR", dir.path());

        let missing = run_async(run(EnvArgs {
            tenant: "phlax".into(),
            token_env: None,
        }))
        .unwrap_err();
        assert!(matches!(missing, LoginError::NoLease(ref tenant) if tenant == "phlax"));

        KeyringStore::new()
            .write(
                "phlax",
                &fixture_entry(Utc::now() - chrono::Duration::seconds(1)),
            )
            .unwrap();
        let expired = run_async(run(EnvArgs {
            tenant: "phlax".into(),
            token_env: None,
        }))
        .unwrap_err();
        assert!(
            matches!(expired, LoginError::LeaseExpired { ref tenant, .. } if tenant == "phlax")
        );

        std::env::remove_var("BOTWORK_LOGIN_KEYRING_DIR");
    }
}
