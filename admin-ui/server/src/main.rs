//! Production binary for `botwork-admin-ui-server`.
//!
//! Builds the axum router, binds, serves. Exits non-zero on:
//!
//! * bind failure (BOTWORK_ADMIN_UI_BIND can't be opened);
//! * `axum::serve` transport / shutdown failure.
//!
//! There is no DB connection and no upstream — the server is a
//! glorified static-file responder with one liveness probe.

use std::io::Write;
use std::process::ExitCode;

use botwork_admin_ui_server::build_router;
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

const PREFIX: &str = "[admin-ui]";
const BIN_NAME: &str = "botwork-admin-ui-server";
const VERSION: &str = include_str!("../../../VERSION").trim_ascii();

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
    // SECURITY: admin-ui has no in-process authentication in v0.
    // Trust boundary is the docker network: in the supported
    // deployment it joins `botwork-internal` with the `admin_ui`
    // alias and only the ingress envoy (via the future
    // `/admin/*` route) reaches it. The bind port MUST NEVER be
    // `--publish`'d to the host.
    //
    // Default port (9500) follows the workspace numbering
    // convention (config-broker=9200, control-plane=9300/9301,
    // admin-api=9400, admin-ui=9500).
    std::env::var("BOTWORK_ADMIN_UI_BIND").unwrap_or_else(|_| "0.0.0.0:9500".to_string())
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

    let bind = bind_from_env();
    let app = build_router();

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
