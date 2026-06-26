//! Production binary for `botwork-config-broker`.
//!
//! Connects to the DB at start-up, builds the axum router, binds and
//! serves. Exits non-zero on:
//!
//! * missing/invalid `BOTWORK_DATABASE_URL` (matches the convention
//!   the other consumers use),
//! * connect failure,
//! * bind failure.

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use botwork_config_broker::{build_router, AppState};
use botwork_entity::connection::{connect_from_env, ConnectError, DATABASE_URL_ENV};
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

const PREFIX: &str = "[config-broker]";
const BIN_NAME: &str = "botwork-config-broker";

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

fn bind_from_env() -> String {
    // SECURITY: config-broker resolves plugin descriptors with no caller
    // authentication in v0. The trust boundary is the docker network.
    // Default is 0.0.0.0:9200 so session-broker (a separate container) can
    // reach it via the `config_broker` network alias on the
    // `botwork-internal` network. The port MUST never be published to the
    // host (no `-p` / `--publish`).
    std::env::var("BOTWORK_CONFIG_BROKER_BIND").unwrap_or_else(|_| "0.0.0.0:9200".to_string())
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
        Ok(db) => Arc::new(db),
        Err(ConnectError::MissingUrl) => {
            error!("{PREFIX} {DATABASE_URL_ENV} is not set");
            return ExitCode::from(2);
        }
        Err(ConnectError::Db(err)) => {
            error!("{PREFIX} failed to connect to postgres: {err}");
            return ExitCode::from(3);
        }
    };

    let bind = bind_from_env();
    let app = build_router(AppState { db });

    let listener = match TcpListener::bind(&bind).await {
        Ok(listener) => listener,
        Err(err) => {
            error!("{PREFIX} failed to bind {bind}: {err}");
            return ExitCode::from(4);
        }
    };

    info!(
        "{PREFIX} starting on {}",
        listener.local_addr().expect("local addr")
    );

    if let Err(err) = axum::serve(listener, app).await {
        error!("{PREFIX} server error: {err}");
        return ExitCode::from(5);
    }
    ExitCode::SUCCESS
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
