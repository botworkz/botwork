use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use botwork_control_plane::{
    build_router, run_recovery_with_retries, AdsServer, AppState, SessionStore, ACK_DISABLED_ENV,
    DEFAULT_ACK_WAIT,
};
use botwork_entity::connection::{connect_from_env, ConnectError, DATABASE_URL_ENV};
use tokio::net::TcpListener;
use tonic::transport::Server as TonicServer;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

const PREFIX: &str = "[control-plane]";
const BIN_NAME: &str = "botwork-control-plane";

fn handle_version_flag(args: &[String], mut writer: impl Write) -> Option<i32> {
    match args.get(1).map(String::as_str) {
        Some("--version") | Some("-V") => {
            writeln!(writer, "{BIN_NAME} {}", botwork_version::full()).expect("write version");
            Some(0)
        }
        _ => None,
    }
}

fn http_bind_from_env() -> String {
    // SECURITY: control-plane v0 has no caller authentication. The trust
    // boundary is the docker network: in the supported deployment it
    // joins `botwork-internal` and only session-broker / the egress
    // envoy xDS subscriber (also on `botwork-internal`) can reach it
    // via the `control_plane` alias. The bind ports MUST NEVER be
    // published to the host. Mirrors the auth-broker and
    // config-broker posture.
    //
    // If you instead run control-plane as a bare host process, set
    // BOTWORK_CONTROL_PLANE_BIND=127.0.0.1:9300 and
    // BOTWORK_CONTROL_PLANE_XDS_BIND=127.0.0.1:9301.
    std::env::var("BOTWORK_CONTROL_PLANE_BIND").unwrap_or_else(|_| "0.0.0.0:9300".to_string())
}

fn xds_bind_from_env() -> String {
    // Separate gRPC server for the ADS endpoint. Different protocol
    // stack (tonic h2 vs axum h1), different bind. Same trust
    // boundary (botwork-internal only).
    std::env::var("BOTWORK_CONTROL_PLANE_XDS_BIND").unwrap_or_else(|_| "0.0.0.0:9301".to_string())
}

fn ack_disabled_from_env() -> bool {
    // Twin of recovery_disabled_from_env: when set truthy, the HTTP
    // mutation handlers (`POST /sessions`, `DELETE /sessions/<id>`)
    // skip the wait-for-xDS-ACK step and return success as soon as
    // the in-memory store mutation completes. This restores the
    // pre-#92 behaviour where a 201 from control-plane meant "the
    // store knows about the record" rather than "envoy has the
    // policy live."
    //
    // Setting this is an explicit decision to accept the cold-start
    // race where a freshly spawned plugin's first tool call may 403
    // because xDS hasn't caught up.
    matches!(
        std::env::var(ACK_DISABLED_ENV).as_deref().map(str::trim),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn ack_wait_from_env() -> Duration {
    // BOTWORK_CONTROL_PLANE_ACK_WAIT_MS: override the default 5s ack
    // wait. Lower values surface a slow / disconnected envoy faster
    // (useful in CI smoke); higher values forgive more boot latency.
    // 0 is rejected -- a zero timeout is functionally the same as
    // ack_disabled but harder to spot, so refuse it loudly.
    match std::env::var("BOTWORK_CONTROL_PLANE_ACK_WAIT_MS") {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => {
                error!(
                    "{PREFIX} BOTWORK_CONTROL_PLANE_ACK_WAIT_MS=0 is not allowed; \
                     set BOTWORK_CONTROL_PLANE_DISABLE_ACK_WAIT=1 to skip the gate"
                );
                std::process::exit(1);
            }
            Ok(ms) => Duration::from_millis(ms),
            Err(err) => {
                error!("{PREFIX} invalid BOTWORK_CONTROL_PLANE_ACK_WAIT_MS={raw}: {err}");
                std::process::exit(1);
            }
        },
        Err(_) => DEFAULT_ACK_WAIT,
    }
}

fn recovery_disabled_from_env() -> bool {
    // Operator escape hatch. The default is "fail to start if recovery
    // cannot reach postgres after MAX_ATTEMPTS"; this flag restores
    // the previous "start with an empty store no matter what"
    // behaviour. Intended for break-glass scenarios where the DB is
    // unrecoverable and control-plane needs to come up empty so new
    // spawns can be reconciled by hand. Not part of the supported
    // posture; setting this is an explicit decision to start with an
    // unknown live state.
    //
    // Name kept verbatim from the pre-round-3 implementation (was
    // documented as "session-broker unreachable"); the underlying
    // semantics are the same — "I accept the consequences of starting
    // empty" — and operator runbooks already grep for it.
    matches!(
        std::env::var("BOTWORK_CONTROL_PLANE_DISABLE_RECOVERY")
            .as_deref()
            .map(str::trim),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = handle_version_flag(&args, std::io::stdout()) {
        std::process::exit(code);
    }

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
    info!("{PREFIX} {BIN_NAME} {}", botwork_version::full());

    // Build the store first; recovery seeds INTO it before the HTTP
    // and xDS servers start accepting requests. AppState holds the
    // same `Arc<SessionStore>` so handlers and the xDS feeder see the
    // seeded state.
    let store = Arc::new(SessionStore::new());
    let ack_disabled = ack_disabled_from_env();
    let ack_wait = ack_wait_from_env();
    if ack_disabled {
        warn!(
            "{PREFIX} {ACK_DISABLED_ENV}=1 -- mutation handlers will NOT wait for xDS ACK; \
             accepting the cold-start race in exchange for non-blocking spawns"
        );
    } else {
        info!("{PREFIX} synchronous xDS ack gate enabled (wait={ack_wait:?})");
    }
    let state = AppState {
        sessions: store.clone(),
        ack_wait,
        ack_disabled,
    };
    info!("{PREFIX} session store initialised (empty)");

    // RFE #105 round-3 follow-up: connect to postgres + run recovery.
    // BOTWORK_DATABASE_URL is required in production (rendered by
    // botwork-db-init.service into /var/lib/botwork-db/secret.env
    // and surfaced to this unit via EnvironmentFile=). The
    // BOTWORK_CONTROL_PLANE_DISABLE_RECOVERY break-glass keeps its
    // pre-round-3 semantics ("I accept the consequences of starting
    // empty") — when set, we never touch postgres, which is what the
    // per-service container CI smoke (`control-plane/smoke.sh`)
    // relies on: there is no postgres sidecar in that step, the
    // smoke only proves the binary boots and binds both ports.
    let db = if recovery_disabled_from_env() {
        warn!(
            "{PREFIX} BOTWORK_CONTROL_PLANE_DISABLE_RECOVERY=1 -- skipping cold-start recovery \
             sync AND postgres connect; starting with empty store"
        );
        None
    } else {
        let db = match connect_from_env().await {
            Ok(db) => Arc::new(db),
            Err(ConnectError::MissingUrl) => {
                error!(
                    "{PREFIX} {DATABASE_URL_ENV} is not set; recovery now reads from postgres \
                     directly — add EnvironmentFile=/var/lib/botwork-db/secret.env to the \
                     botwork-control-plane.service unit, or set \
                     BOTWORK_CONTROL_PLANE_DISABLE_RECOVERY=1 to skip recovery (break-glass)"
                );
                std::process::exit(2);
            }
            Err(ConnectError::Db(err)) => {
                error!("{PREFIX} failed to connect to postgres: {err}");
                std::process::exit(3);
            }
        };
        match run_recovery_with_retries(store.clone(), db.as_ref()).await {
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
        Some(db)
    };

    let http_bind = http_bind_from_env();
    let app = build_router(state);

    let http_listener = match TcpListener::bind(&http_bind).await {
        Ok(listener) => listener,
        Err(err) => {
            error!("{PREFIX} failed to bind HTTP {http_bind}: {err}");
            std::process::exit(1);
        }
    };

    info!(
        "{PREFIX} starting HTTP on {}",
        http_listener.local_addr().expect("local addr")
    );

    let xds_bind = xds_bind_from_env();
    let xds_addr: SocketAddr = match xds_bind.parse() {
        Ok(a) => a,
        Err(err) => {
            error!("{PREFIX} failed to parse xDS bind {xds_bind}: {err}");
            std::process::exit(1);
        }
    };

    info!("{PREFIX} starting xDS gRPC on {xds_addr}");

    let ads_server = AdsServer::new(store.clone());
    let xds_future = TonicServer::builder()
        .add_service(ads_server.into_grpc_service())
        .serve(xds_addr);

    // Run both servers concurrently. If either exits we tear the
    // whole binary down so systemd restarts cleanly — partial
    // availability (HTTP up, xDS down, or vice versa) is worse than
    // a clean restart because the egress envoy would silently
    // operate on stale config.
    tokio::select! {
        result = axum::serve(http_listener, app) => {
            if let Err(err) = result {
                error!("{PREFIX} HTTP server error: {err}");
            } else {
                error!("{PREFIX} HTTP server exited unexpectedly");
            }
        }
        result = xds_future => {
            if let Err(err) = result {
                error!("{PREFIX} xDS server error: {err}");
            } else {
                error!("{PREFIX} xDS server exited unexpectedly");
            }
        }
    }
    // Drop the DB handle explicitly so any pool drain happens before
    // the process exits 1. Cosmetic — process exit closes connections
    // either way — but greppable if a future debug shows a pool
    // leak.
    drop(db);
    std::process::exit(1);
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
