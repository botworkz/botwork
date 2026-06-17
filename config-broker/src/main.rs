use std::path::PathBuf;

use botwork_config_broker::{build_app_state, build_router};
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

const PREFIX: &str = "[config-broker]";

fn registry_path_from_env() -> PathBuf {
    std::env::var("BOTWORK_PLUGIN_REGISTRY_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/botwork/plugins.yaml"))
}

fn bind_from_env() -> String {
    // SECURITY: config-broker resolves plugin descriptors with no caller
    // authentication in v0. The trust boundary is the docker network.
    // Default is 0.0.0.0:9200 so session-broker (a separate container) can
    // reach it via the `config_broker` network alias on the `botwork`
    // network. The port MUST never be published to the host (no `-p`/
    // `--publish`). Mirrors the auth-broker posture.
    //
    // If you instead run config-broker as a bare host process, set
    // BOTWORK_CONFIG_BROKER_BIND=127.0.0.1:9200 so it is not exposed beyond
    // loopback.
    std::env::var("BOTWORK_CONFIG_BROKER_BIND").unwrap_or_else(|_| "0.0.0.0:9200".to_string())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let registry_path = registry_path_from_env();
    let state = match build_app_state(&registry_path) {
        Ok(state) => state,
        Err(err) => {
            error!(
                "{PREFIX} failed to load plugin registry from {}: {err}",
                registry_path.display()
            );
            std::process::exit(1);
        }
    };

    info!(
        "{PREFIX} loaded plugin registry ({} plugins) from {}",
        state.registry.len(),
        registry_path.display()
    );

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
