use std::sync::Arc;

use botwork_control_plane::{build_router, run_recovery_with_retries, AppState, SessionStore};
use tokio::net::TcpListener;
use tracing::{error, info, warn};
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

fn session_broker_endpoint_from_env() -> String {
    // session-broker's admin server: same alias session-broker registers
    // on `botwork-internal` (`session_broker`), port 9002 (set by
    // `BOTWORK_SESSION_BROKER_ADMIN_ADDR` in session-broker's lib.rs).
    // Override via env when running control-plane out of the canonical
    // docker network -- e.g. local iteration where session-broker is on
    // a loopback port.
    std::env::var("BOTWORK_SESSION_BROKER_ENDPOINT")
        .unwrap_or_else(|_| "http://session_broker:9002".to_string())
}

fn recovery_disabled_from_env() -> bool {
    // Operator escape hatch. The default is "fail to start if recovery
    // cannot reach session-broker after MAX_ATTEMPTS"; this flag
    // restores the previous "start with an empty store no matter what"
    // behaviour. Intended for break-glass scenarios where session-broker
    // is unrecoverable and control-plane needs to come up empty so new
    // spawns can be reconciled by hand. Not part of the supported
    // posture; setting this is an explicit decision to start with an
    // unknown live state.
    matches!(
        std::env::var("BOTWORK_CONTROL_PLANE_DISABLE_RECOVERY")
            .as_deref()
            .map(str::trim),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    // Build the store first; recovery seeds INTO it before the HTTP
    // server starts accepting requests. AppState holds the same
    // `Arc<SessionStore>` so the handlers see the seeded state.
    let store = Arc::new(SessionStore::new());
    let state = AppState {
        sessions: store.clone(),
    };
    info!("{PREFIX} session store initialised (empty)");

    if recovery_disabled_from_env() {
        warn!(
            "{PREFIX} BOTWORK_CONTROL_PLANE_DISABLE_RECOVERY=1 -- skipping cold-start recovery sync; \
             starting with empty store"
        );
    } else {
        let endpoint = session_broker_endpoint_from_env();
        info!("{PREFIX} session-broker endpoint: {endpoint}");
        match run_recovery_with_retries(store.clone(), &endpoint).await {
            Ok(count) => {
                info!("{PREFIX} cold-start recovery complete: {count} session(s) seeded");
            }
            Err(err) => {
                // The whole point of "refuse to start on uncertainty":
                // an empty store would silently break the xDS feeder.
                // systemd's Restart=always picks up from here.
                error!(
                    "{PREFIX} cold-start recovery failed after all retries: {err}; \
                     refusing to start (set BOTWORK_CONTROL_PLANE_DISABLE_RECOVERY=1 to override)"
                );
                std::process::exit(1);
            }
        }
    }

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
