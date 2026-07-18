//! Subcommands. Each `commands::*::run` is the library entry point
//! the `bin/botwork-login` shim calls; the same entry points are
//! what a future web / admin UI would invoke without shelling out.

use url::Url;

pub mod env;
pub mod input;
pub mod login;
pub mod logout;
pub mod register;
pub mod status;

pub use env::{run as run_env, EnvArgs};
pub use login::{run as run_login, LoginArgs};
pub use logout::{run as run_logout, LogoutArgs};
pub use register::{run as run_register, RegisterArgs};
pub use status::{run as run_status, StatusArgs};

/// Warn on stderr when the resolved broker URL uses a plaintext
/// `http://` channel rather than `https://`.
///
/// Called by `login` and `register` right before the password leaves
/// the process: over plaintext, the OPAQUE handshake's confidentiality
/// guarantees don't cover the surrounding transport, and any bearer
/// token minted by the broker rides back in the clear. The scheme is
/// already validated to be `http` or `https` by
/// [`crate::config::Config::resolve_server`], so this only ever fires
/// for the `http` arm.
///
/// The notice goes to stderr (never stdout) so it can't corrupt the
/// `env` subcommand's `export …` output that callers pipe into `eval`.
/// It's emitted via `tracing::warn!` so a caller that installs a
/// subscriber routes it through their logging pipeline; because the
/// CLI installs no subscriber, we also print a plain stderr line so
/// the warning is always visible.
pub fn warn_if_insecure_server(server: &Url) {
    if server.scheme() != "https" {
        let msg = "authenticating over an unencrypted channel will expose your secrets";
        tracing::warn!(server = %server, "{msg}");
        eprintln!("warning: {msg} ({server})");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_is_not_flagged() {
        // A pure smoke test: the function returns `()` and must not
        // panic for an https URL. (The stderr side-effect only fires
        // for http, which we can't easily capture here.)
        let url = Url::parse("https://broker.example:9100").unwrap();
        warn_if_insecure_server(&url);
    }

    #[test]
    fn http_is_not_a_panic() {
        // Confirm the http path executes cleanly (it emits to stderr).
        let url = Url::parse("http://127.0.0.1:9100").unwrap();
        warn_if_insecure_server(&url);
    }
}
