//! Production binary for `botwork-bootstrap`.
//!
//! Runs once at boot via the systemd oneshot
//! `botwork-bootstrap.service`, ordered between db-migrate and
//! config-broker. The container CMD is this binary with no args; the
//! config path is read from `BOTWORK_BOOTSTRAP_CONFIG`, defaulting to
//! `/etc/botwork/bootstrap.yaml`.
//!
//! Exit codes (the systemd unit and CI smoke key on these):
//!
//! | Code | Meaning                                                       |
//! |------|---------------------------------------------------------------|
//! | 0    | apply succeeded (no-op or mutations both count as success)    |
//! | 2    | required env var missing (BOTWORK_DATABASE_URL)               |
//! | 3    | postgres connect failed                                       |
//! | 4    | bootstrap config file missing / read failure                  |
//! | 5    | bootstrap config validation failure (yaml / refs / uniqueness)|
//! | 6    | database mutation failed mid-apply                            |

use std::path::PathBuf;
use std::process::ExitCode;

use botwork_admin_core::BootstrapConfig;
use botwork_bootstrap::{apply, BootstrapError};
use botwork_entity::connection::{connect_from_env, ConnectError, DATABASE_URL_ENV};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

const PREFIX: &str = "[bootstrap]";

/// Env var that points at the bootstrap config. Override-only; production
/// systemd renders the default into `/etc/botwork/bootstrap.yaml`.
const CONFIG_PATH_ENV: &str = "BOTWORK_BOOTSTRAP_CONFIG";
const DEFAULT_CONFIG_PATH: &str = "/etc/botwork/bootstrap.yaml";

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let config_path: PathBuf = std::env::var(CONFIG_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_CONFIG_PATH));

    let config = match BootstrapConfig::load(&config_path) {
        Ok(cfg) => cfg,
        Err(err) => {
            // BootstrapConfig::load returns admin-core's LoadError; lift
            // into BootstrapError so the exit-code switch (file IO →
            // exit 4 vs. validation → exit 5) keeps working through the
            // admin-core extraction.
            let err: BootstrapError = err.into();
            match err {
                BootstrapError::ConfigNotFound(_) | BootstrapError::ConfigRead { .. } => {
                    error!("{PREFIX} config read failed: {err}");
                    return ExitCode::from(4);
                }
                _ => {
                    error!("{PREFIX} config validation failed: {err}");
                    return ExitCode::from(5);
                }
            }
        }
    };

    let db = match connect_from_env().await {
        Ok(db) => db,
        Err(ConnectError::MissingUrl) => {
            error!("{PREFIX} {DATABASE_URL_ENV} is not set");
            return ExitCode::from(2);
        }
        Err(ConnectError::Db(err)) => {
            error!("{PREFIX} failed to connect to postgres: {err}");
            return ExitCode::from(3);
        }
    };

    match apply(&db, &config).await {
        Ok(stats) => {
            info!(
                "{PREFIX} applied bootstrap from {} ({} tenants / {} workspaces / {} plugins / {} bindings)",
                config_path.display(),
                stats.tenants_inserted + stats.tenants_updated,
                stats.workspaces_inserted + stats.workspaces_updated,
                stats.plugins_inserted + stats.plugins_updated,
                stats.bindings_inserted + stats.bindings_updated,
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            error!("{PREFIX} apply failed: {err}");
            ExitCode::from(6)
        }
    }
}
