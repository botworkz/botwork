use botwork_control_plane::{build_app_state, build_router};
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

const PREFIX: &str = "[control-plane]";

fn bind_from_env() -> String {
    // SECURITY: control-plane v0 has no caller authentication. The trust
    // boundary is the docker network: in the supported deployment it
    // joins `botwork-internal` and only session-broker / future xDS
    // subscribers (also on `botwork-internal`) can reach it via the
    // `control_plane` alias. The bind port MUST NEVER be published to
    // the host (no `-p`/`--publish`). Mirrors the auth-broker and
    // config-broker posture.
    //
    // If you instead run control-plane as a bare host process, set
    // BOTWORK_CONTROL_PLANE_BIND=127.0.0.1:9300 so it is not exposed
    // beyond loopback.
    std::env::var("BOTWORK_CONTROL_PLANE_BIND").unwrap_or_else(|_| "0.0.0.0:9300".to_string())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let state = build_app_state();
    info!("{PREFIX} session store initialised (empty)");

    let bind = bind_from_env();
    let app = build_router(state);

    let listener = match TcpListener::bind(&bind).await {
        Ok(listener) => listener,
        Err(err) => {
            error!("{PREFIX} failed to bind {bind}: {err}");
            std::process::exit(1);
        }
    };

    info!(
        "{PREFIX} starting on {}",
        listener.local_addr().expect("local addr")
    );

    if let Err(err) = axum::serve(listener, app).await {
        error!("{PREFIX} server error: {err}");
        std::process::exit(1);
    }
}
