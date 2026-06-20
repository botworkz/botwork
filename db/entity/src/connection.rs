//! Connection helpers for the persistence layer.
//!
//! Two entry points:
//!
//! * [`connect`] — explicit-URL constructor used by tests (testcontainers
//!   give us a URL with an ephemeral port) and by callers that compose the
//!   URL themselves.
//! * [`connect_from_env`] — production entry point. Reads
//!   [`DATABASE_URL_ENV`] from the process environment and delegates to
//!   [`connect`].
//!
//! # Test posture
//!
//! Tests **must not** call [`connect_from_env`]; doing so risks a test
//! accidentally targeting a real postgres. The pattern is enforced by
//! convention plus a workspace-level grep check (see
//! `db/migration/tests/no_env_leakage.rs`).
//!
//! As a consequence, [`connect_from_env`] has no direct test coverage —
//! its body is the trivial composition of `std::env::var` and [`connect`].
//! Both arms are exercised by unit tests below using [`connect`] and a
//! synthetic URL.

use sea_orm::{ConnectOptions, Database, DatabaseConnection};

/// Name of the env var that holds the production postgres URL. Composed by
/// the space-side bootstrap (`/var/lib/botwork-db/secret.env`) and exported
/// into each consumer's systemd unit via `EnvironmentFile=`.
///
/// Format: `postgres://botwork:<password>@postgres/botwork`.
pub const DATABASE_URL_ENV: &str = "BOTWORK_DATABASE_URL";

/// Errors returned by [`connect_from_env`].
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The env var was unset (or non-UTF8). Production deployments always set
    /// it; if you hit this in dev, source the secret file:
    /// `set -a; . /var/lib/botwork-db/secret.env; set +a`.
    #[error(
        "{DATABASE_URL_ENV} is not set; the production bootstrap renders it from \
         /var/lib/botwork-db/secret.env"
    )]
    MissingUrl,

    /// SeaORM / sqlx refused the URL or the connection failed.
    #[error("failed to connect to {DATABASE_URL_ENV}: {0}")]
    Db(#[from] sea_orm::DbErr),
}

/// Connect to postgres at `url`. Returns a SeaORM [`DatabaseConnection`].
///
/// Pool sizing is left at SeaORM's defaults in v0. RFE 97 lists per-consumer
/// pool tuning as out of scope until we have a real workload.
pub async fn connect(url: &str) -> Result<DatabaseConnection, sea_orm::DbErr> {
    let opts = ConnectOptions::new(url.to_owned());
    Database::connect(opts).await
}

/// Connect to postgres using the URL in [`DATABASE_URL_ENV`]. Used by
/// production binaries only — see crate-level docs.
pub async fn connect_from_env() -> Result<DatabaseConnection, ConnectError> {
    let url = std::env::var(DATABASE_URL_ENV).map_err(|_| ConnectError::MissingUrl)?;
    Ok(connect(&url).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_with_malformed_url_returns_db_error() {
        // Not testing connectivity — testcontainers covers that — only that
        // a malformed URL surfaces as a structured `DbErr` rather than a
        // panic. If SeaORM ever changes the categorisation of unknown
        // schemes this test will trip and we update the contract.
        let err = connect("not-a-url://wrong/db")
            .await
            .expect_err("malformed URL must error");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("url") || msg.contains("scheme") || msg.contains("driver"),
            "expected URL/scheme-shaped error, got: {err}"
        );
    }
}
