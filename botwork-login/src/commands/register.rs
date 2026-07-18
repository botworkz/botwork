//! `botwork-login register` — operator-flow OPAQUE registration.
//!
//! Used once per tenant by an admin to seed the
//! `opaque_password_file` row. Subsequent `login` then drives the
//! lease-issuing handshake against the persisted row.

use std::path::PathBuf;
use zeroize::Zeroizing;

use crate::client::run_register as drive_register;
use crate::commands::input::PasswordSource;
use crate::commands::warn_if_insecure_server;
use crate::config::Config;
use crate::error::LoginError;

/// Typed args for `register`.
#[derive(Debug, Default)]
pub struct RegisterArgs {
    /// Tenant name (required).
    pub tenant: String,
    /// OPAQUE credential identifier override.
    pub credential_identifier: Option<String>,
    /// Server URL override.
    pub server: Option<String>,
    /// Additional PEM CA bundle path.
    pub cacert: Option<PathBuf>,
    /// Read the password from stdin.
    pub password_stdin: bool,
    /// Library-supplied password — same shape as
    /// [`crate::commands::login::LoginArgs::password`].
    pub password: Option<Zeroizing<Vec<u8>>>,
}

/// Run the `register` subcommand. Returns user-facing text the CLI
/// shim prints on success; errors propagate.
pub async fn run(args: RegisterArgs) -> Result<String, LoginError> {
    let config = Config::load()?;
    let resolved = config.resolve(
        &args.tenant,
        args.server.as_deref(),
        args.credential_identifier.as_deref(),
    )?;

    // Password is about to travel over this channel. If it isn't TLS,
    // warn loudly before we send anything.
    warn_if_insecure_server(&resolved.server);

    let password = match args.password {
        Some(bytes) => bytes,
        None => PasswordSource::for_register(args.password_stdin).read()?,
    };

    let outcome = drive_register(
        &resolved.server,
        &args.tenant,
        &resolved.credential_identifier,
        password.as_slice(),
        args.cacert.as_deref(),
    )
    .await?;

    Ok(format!(
        "✓ Registered tenant '{tenant}' (suite v{suite}). \
         Run `botwork-login --tenant {tenant}` to mint a lease.",
        tenant = outcome.tenant,
        suite = outcome.suite_version
    ))
}
