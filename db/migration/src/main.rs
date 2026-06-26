//! Production binary for `botwork-migration`.
//!
//! Runs [`Migrator::up`] against the URL in `BOTWORK_DATABASE_URL`, then
//! exits. This is the binary that the `botwork/db-migrate:local` container
//! invokes as its CMD.
//!
//! Why this is its own deterministic binary rather than re-exporting the
//! full `sea-orm-migration` CLI (`up`/`down`/`status`/`fresh`/`refresh`/
//! `reset`):
//!
//! 1. **Deterministic exit semantics.** Production must only ever run `up`;
//!    surfacing the full CLI as the container CMD would make a misconfigured
//!    systemd unit catastrophic.
//! 2. **Single readable log line on success.** The CLI dumps SeaORM's full
//!    pretty output; the production binary emits one structured `info!` so
//!    the journal stays scannable.
//! 3. **`up` is idempotent — the only post-condition we care about is the
//!    `seaql_migrations` tracking table existing.** That is true after the
//!    first run regardless of whether any migrations are present.
//!
//! Operator diagnostics (`status` etc.) are deferred until there are real
//! migrations whose state is worth inspecting. See RFE 97 (out-of-scope).

use std::io::Write;
use std::process::ExitCode;

use botwork_entity::connection::{connect_from_env, ConnectError, DATABASE_URL_ENV};
use botwork_migration::Migrator;
use sea_orm_migration::MigratorTrait;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

const PREFIX: &str = "[db-migrate]";
const BIN_NAME: &str = "botwork-migration";

fn handle_version_flag(args: &[String], mut writer: impl Write) -> Option<i32> {
    match args.get(1).map(String::as_str) {
        Some("--version") | Some("-V") => {
            writeln!(writer, "{BIN_NAME} {}", botwork_version::full())
                .expect("failed to write version output");
            Some(0)
        }
        _ => None,
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = handle_version_flag(&args, std::io::stdout()) {
        return ExitCode::from(code as u8);
    }

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
    info!("{PREFIX} {BIN_NAME} {}", botwork_version::full());

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

    match Migrator::up(&db, None).await {
        Ok(()) => {
            info!("{PREFIX} migrations applied (none pending is a valid result)");
            ExitCode::SUCCESS
        }
        Err(err) => {
            error!("{PREFIX} migration run failed: {err}");
            ExitCode::from(4)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{handle_version_flag, BIN_NAME};

    #[test]
    fn version_flags_print_the_shared_version() {
        for flag in ["--version", "-V"] {
            let mut output = Vec::new();
            let args = vec![BIN_NAME.to_string(), flag.to_string()];
            assert_eq!(handle_version_flag(&args, &mut output), Some(0));
            assert_eq!(
                String::from_utf8(output).expect("utf8"),
                format!("{BIN_NAME} {}\n", botwork_version::full())
            );
        }
    }
}
