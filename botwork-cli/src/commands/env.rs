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

    #[test]
    fn format_export_shape() {
        assert_eq!(
            format_export("BOTWORK_BEARER", "ABCDEF0123456789"),
            "export BOTWORK_BEARER='ABCDEF0123456789'"
        );
    }
}
