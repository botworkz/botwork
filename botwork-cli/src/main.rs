//! `botwork-cli` — CLI entry point.
//!
//! Thin `clap` shim around the typed library entry points in
//! [`botwork_cli::commands`]. The actual flow / error handling
//! lives in the library so a future web / admin UI can reuse it.
//!
//! Exit codes are mapped via
//! [`botwork_cli::error::exit_code_for`]:
//!
//! - 0 — success.
//! - 1 — user-recoverable (wrong password, no lease, expired lease,
//!   bad `--lease` value, malformed config).
//! - 2 — server / network (broker unreachable, unexpected status,
//!   malformed response).
//! - 3 — keyring backend (OS keychain unreachable, file fallback
//!   write failure).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::LazyLock;

use clap::{Parser, Subcommand};

use botwork_cli::commands::{run_env, run_login, run_logout, run_register, run_status};
use botwork_cli::commands::{EnvArgs, LoginArgs, LogoutArgs, RegisterArgs, StatusArgs};
use botwork_cli::error::{exit_code_for, LoginError};

const SSL_CERT_FILE_ENV: &str = "SSL_CERT_FILE";

fn clap_version() -> &'static str {
    static VERSION: LazyLock<String> = LazyLock::new(botwork_version::version_string);
    VERSION.as_str()
}

#[derive(Parser, Debug)]
#[command(
    name = "bw",
    about = "botwork end-user CLI",
    version = clap_version(),
)]
struct Cli {
    /// Tenant name. Required for every subcommand; placed at the top
    /// level so `bw --tenant phlax` (login by default)
    /// stays the user-facing default invocation.
    #[arg(long, global = true)]
    tenant: Option<String>,

    /// Server URL override. Resolution order:
    /// `--server` > `BOTWORK_LOGIN_SERVER` > config file > built-in.
    #[arg(long, global = true)]
    server: Option<String>,

    /// OPAQUE credential identifier override. Defaults to the tenant
    /// name.
    #[arg(long = "credential-identifier", global = true)]
    credential_identifier: Option<String>,

    /// Path to a PEM CA certificate (bundle) to trust in addition to the system roots; overrides $SSL_CERT_FILE.
    #[arg(long, global = true, value_name = "PATH")]
    cacert: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Drive an OPAQUE login. (Default subcommand.)
    Login {
        /// Lease window (humantime: `7d`, `30d`, `12h`).
        #[arg(long, default_value = "7d")]
        lease: String,
        /// Read the password from stdin (no prompt).
        #[arg(long)]
        password_stdin: bool,
    },
    /// Operator-flow OPAQUE registration. Run once per tenant.
    Register {
        /// Read the password from stdin (no prompt, no confirm).
        #[arg(long)]
        password_stdin: bool,
    },
    /// Show the current lease state from the keyring. Offline.
    Status,
    /// Print `export <VAR>='<bearer>'` for shell consumption.
    Env {
        /// Override the env var name (`BOTWORK_BEARER` by default).
        #[arg(long = "token-env")]
        token_env: Option<String>,
    },
    /// Remove the keyring entry for the tenant. Keyring-only in v0.
    Logout,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = dispatch(cli).await;
    match result {
        Ok(success) => {
            if !success.is_empty() {
                println!("{success}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(exit_code_for(&err) as u8)
        }
    }
}

async fn dispatch(cli: Cli) -> Result<String, LoginError> {
    let command = cli.command.unwrap_or(Command::Login {
        lease: "7d".to_string(),
        password_stdin: false,
    });
    let tenant = match cli.tenant {
        Some(t) => t,
        None => {
            return Err(LoginError::Config(
                "missing `--tenant` (every subcommand requires a tenant)".to_string(),
            ));
        }
    };

    let cacert = resolve_cacert_path(cli.cacert);

    match command {
        Command::Login {
            lease,
            password_stdin,
        } => {
            run_login(LoginArgs {
                tenant,
                lease: Some(lease),
                credential_identifier: cli.credential_identifier,
                server: cli.server,
                cacert: cacert.clone(),
                password_stdin,
                ..LoginArgs::default()
            })
            .await
        }
        Command::Register { password_stdin } => {
            run_register(RegisterArgs {
                tenant,
                credential_identifier: cli.credential_identifier,
                server: cli.server,
                cacert,
                password_stdin,
                ..RegisterArgs::default()
            })
            .await
        }
        Command::Status => run_status(StatusArgs { tenant }).await,
        Command::Env { token_env } => run_env(EnvArgs { tenant, token_env }).await,
        Command::Logout => run_logout(LogoutArgs { tenant }).await,
    }
}

fn resolve_cacert_path(cli_cacert: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = cli_cacert {
        return Some(path);
    }
    match std::env::var(SSL_CERT_FILE_ENV) {
        Ok(value) if !value.trim().is_empty() => Some(PathBuf::from(value)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use botwork_cli::keyring_store::{KeyringEntry, KeyringStore};
    use chrono::Utc;
    use clap::error::ErrorKind;
    use std::sync::Mutex;
    use tempfile::TempDir;

    fn ssl_cert_file_env_lock() -> &'static Mutex<()> {
        static LOCK: Mutex<()> = Mutex::new(());
        &LOCK
    }

    fn keyring_env_lock() -> &'static Mutex<()> {
        static LOCK: Mutex<()> = Mutex::new(());
        &LOCK
    }

    fn run_async<F: std::future::Future<Output = T>, T>(future: F) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn version_flag_long_reports_display_version() {
        let err = Cli::try_parse_from(["bw", "--version"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        let msg = err.to_string();
        let version = clap_version();
        assert!(
            msg.contains(version),
            "expected version string {version}, got: {msg:?}",
        );
    }

    #[test]
    fn version_flag_short_reports_display_version() {
        let err = Cli::try_parse_from(["bw", "-V"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
    }

    #[test]
    fn resolve_cacert_prefers_cli_over_env() {
        let _lock = ssl_cert_file_env_lock().lock().unwrap();
        std::env::set_var(SSL_CERT_FILE_ENV, "/tmp/from-env.pem");
        let resolved = resolve_cacert_path(Some(PathBuf::from("/tmp/from-cli.pem")));
        assert_eq!(resolved, Some(PathBuf::from("/tmp/from-cli.pem")));
        std::env::remove_var(SSL_CERT_FILE_ENV);
    }

    #[test]
    fn resolve_cacert_uses_env_when_cli_missing() {
        let _lock = ssl_cert_file_env_lock().lock().unwrap();
        std::env::set_var(SSL_CERT_FILE_ENV, "/tmp/from-env.pem");
        let resolved = resolve_cacert_path(None);
        assert_eq!(resolved, Some(PathBuf::from("/tmp/from-env.pem")));
        std::env::remove_var(SSL_CERT_FILE_ENV);
    }

    #[test]
    fn resolve_cacert_ignores_empty_env() {
        let _lock = ssl_cert_file_env_lock().lock().unwrap();
        std::env::set_var(SSL_CERT_FILE_ENV, "   ");
        let resolved = resolve_cacert_path(None);
        assert_eq!(resolved, None);
        std::env::remove_var(SSL_CERT_FILE_ENV);
    }

    #[test]
    fn dispatch_requires_tenant() {
        let err = run_async(dispatch(Cli {
            tenant: None,
            server: None,
            credential_identifier: None,
            cacert: None,
            command: Some(Command::Status),
        }))
        .unwrap_err();
        assert!(matches!(err, LoginError::Config(ref msg) if msg.contains("missing `--tenant`")));
    }

    #[test]
    fn dispatch_routes_offline_subcommands() {
        let _lock = keyring_env_lock().lock().unwrap();
        let dir = TempDir::new().unwrap();
        std::env::set_var("BOTWORK_LOGIN_KEYRING_DIR", dir.path());
        KeyringStore::new()
            .write(
                "phlax",
                &KeyringEntry {
                    bearer: "ABCDEF0123456789".into(),
                    lease_id: uuid::Uuid::nil(),
                    expires_at: Utc::now() + chrono::Duration::hours(1),
                    server: "https://broker.example".into(),
                    credential_identifier: "phlax@example.com".into(),
                    suite_version: botwork_opaque_handshake::SUITE_VERSION,
                },
            )
            .unwrap();

        let env_output = run_async(dispatch(Cli {
            tenant: Some("phlax".into()),
            server: None,
            credential_identifier: None,
            cacert: None,
            command: Some(Command::Env {
                token_env: Some("ALT_TOKEN".into()),
            }),
        }))
        .unwrap();
        assert_eq!(env_output, "export ALT_TOKEN='ABCDEF0123456789'");

        let status_output = run_async(dispatch(Cli {
            tenant: Some("phlax".into()),
            server: None,
            credential_identifier: None,
            cacert: None,
            command: Some(Command::Status),
        }))
        .unwrap();
        assert!(status_output.contains("phlax: logged in."));

        let logout_output = run_async(dispatch(Cli {
            tenant: Some("phlax".into()),
            server: None,
            credential_identifier: None,
            cacert: None,
            command: Some(Command::Logout),
        }))
        .unwrap();
        assert_eq!(logout_output, "✓ Removed keyring entry for phlax.");

        std::env::remove_var("BOTWORK_LOGIN_KEYRING_DIR");
    }

    #[test]
    fn dispatch_routes_login_and_register_errors_without_network() {
        let login_err = run_async(dispatch(Cli {
            tenant: Some("phlax".into()),
            server: Some("https://broker.example".into()),
            credential_identifier: None,
            cacert: None,
            command: Some(Command::Login {
                lease: "not-a-duration".into(),
                password_stdin: false,
            }),
        }))
        .unwrap_err();
        assert!(
            matches!(login_err, LoginError::InvalidDuration { ref value, .. } if value == "not-a-duration")
        );

        let register_err = run_async(dispatch(Cli {
            tenant: Some("phlax".into()),
            server: Some("127.0.0.1:9100".into()),
            credential_identifier: None,
            cacert: None,
            command: Some(Command::Register {
                password_stdin: false,
            }),
        }))
        .unwrap_err();
        assert!(
            matches!(register_err, LoginError::InvalidServer { ref value, .. } if value == "127.0.0.1:9100")
        );
    }
}
