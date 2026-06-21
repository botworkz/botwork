//! Production binary for `botwork-admin-api`.
//!
//! Connects to the DB at start-up, builds the axum router, binds and
//! serves. Exits non-zero on:
//!
//! * missing/invalid `BOTWORK_DATABASE_URL` (matches the convention
//!   the other consumers use),
//! * connect failure,
//! * bind failure.

use std::process::ExitCode;
use std::sync::Arc;

use botwork_admin_api::{build_router, AppState, ControlPlaneClient};
use botwork_entity::connection::{connect_from_env, ConnectError, DATABASE_URL_ENV};
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

const PREFIX: &str = "[admin-api]";

fn bind_from_env() -> String {
    // SECURITY: admin-api has no in-process authentication in v0.
    // Trust boundary is the docker network: in the supported deployment
    // it joins `botwork-internal` with the `admin_api` alias and only
    // operator-side curl (via the libvirt SSH tunnel) plus the future
    // ingress envoy `/admin/api/*` route reach it. The bind port
    // MUST NEVER be `--publish`ed to the host.
    //
    // Default port (9400) follows the workspace numbering convention
    // (config-broker=9200, control-plane=9300/9301, admin-api=9400).
    std::env::var("BOTWORK_ADMIN_API_BIND").unwrap_or_else(|_| "0.0.0.0:9400".to_string())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

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
    // BOTWORK_ADMIN_API_DISABLE_LIVE_GATE flag. The client builds
    // its own reqwest pool; the construction is cheap.
    let control_plane = ControlPlaneClient::from_env();

    let bind = bind_from_env();
    let app = build_router(AppState { db, control_plane });

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
