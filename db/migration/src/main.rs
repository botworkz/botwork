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

use std::process::ExitCode;

use botwork_entity::connection::{connect_from_env, ConnectError, DATABASE_URL_ENV};
use botwork_migration::Migrator;
use sea_orm_migration::MigratorTrait;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

const PREFIX: &str = "[db-migrate]";

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

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
