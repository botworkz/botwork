//! Production binary for `botwork-api`.
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

use botwork_api::store::sea_orm_impl::SeaOrmApiStore;
use botwork_api::{
    build_router, AppState, ControlPlaneClient, SecretStoreClient, SessionBrokerClient,
};
use botwork_entity::connection::{connect_from_env, ConnectError, DATABASE_URL_ENV};
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

const PREFIX: &str = "[api]";
const BIN_NAME: &str = "botwork-api";
const VERSION: &str = include_str!("../../VERSION").trim_ascii();

fn version_string() -> String {
    botwork_version::format_full(VERSION, botwork_version::GIT_SHA)
}

fn handle_version_flag(args: &[String], mut writer: impl Write) -> Option<i32> {
    match args.get(1).map(String::as_str) {
        Some("--version") | Some("-V") => {
            writeln!(writer, "{BIN_NAME} {}", version_string())
                .expect("failed to write version output");
            Some(0)
        }
        _ => None,
    }
}

fn bind_from_env() -> String {
    // SECURITY: api has no in-process authentication in v0.
    // Trust boundary is the docker network: in the supported deployment
    // it joins `botwork-internal` with the `admin_api` alias and only
    // operator-side curl (via the libvirt SSH tunnel) plus the future
    // ingress envoy `/admin/api/*` route reach it. The bind port
    // MUST NEVER be `--publish`ed to the host.
    //
    // Default port (9400) follows the workspace numbering convention
    // (config-broker=9200, control-plane=9300/9301, api=9400).
    std::env::var("BOTWORK_API_BIND").unwrap_or_else(|_| "0.0.0.0:9400".to_string())
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
    info!("{PREFIX} {BIN_NAME} {}", version_string());

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

    // Live-state coupling target. ControlPlaneClient reads
    // BOTWORK_CONTROL_PLANE_ENDPOINT (default
    // http://control_plane:9300) and the break-glass
    // BOTWORK_API_DISABLE_LIVE_GATE flag. The client builds
    // its own reqwest pool; the construction is cheap.
    let control_plane = ControlPlaneClient::from_env();

    // Secret-store backend. SecretStoreClient reads
    // BOTWORK_SECRET_STORE_ENDPOINT (default
    // http://secret_store:9500) and the break-glass
    // BOTWORK_API_DISABLE_SECRET_STORE flag.
    let secret_store = SecretStoreClient::from_env();
    let ss_endpoint = std::env::var(botwork_api::secret_store::ENDPOINT_ENV)
        .unwrap_or_else(|_| botwork_api::secret_store::ENDPOINT_DEFAULT.to_string());
    info!(
        "{PREFIX} secret-store endpoint={ss_endpoint} disabled={}",
        secret_store.is_disabled(),
    );

    // Session-broker eviction client. SessionBrokerClient reads
    // BOTWORK_SESSION_BROKER_EVICT_ENDPOINT (default
    // http://session_broker:9002) and the break-glass
    // BOTWORK_API_DISABLE_SESSION_BROKER_EVICT flag.
    let session_broker = SessionBrokerClient::from_env();
    let sb_endpoint = std::env::var(botwork_api::session_broker::ENDPOINT_ENV)
        .unwrap_or_else(|_| botwork_api::session_broker::ENDPOINT_DEFAULT.to_string());
    info!(
        "{PREFIX} session-broker evict endpoint={sb_endpoint} disabled={}",
        session_broker.is_disabled(),
    );

    let bind = bind_from_env();
    let app = build_router(AppState {
        store: Arc::new(SeaOrmApiStore::new(db.clone())),
        db,
        control_plane,
        secret_store,
        session_broker,
    });

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
    use super::{handle_version_flag, version_string, BIN_NAME};

    #[test]
    fn version_flags_print_the_shared_version() {
        for flag in ["--version", "-V"] {
            let mut output = Vec::new();
            let args = vec![BIN_NAME.to_string(), flag.to_string()];
            assert_eq!(handle_version_flag(&args, &mut output), Some(0));
            assert_eq!(
                String::from_utf8(output).expect("utf8"),
                format!("{BIN_NAME} {}\n", version_string())
            );
        }
    }
}
