use std::io::Write;
use std::path::{Path, PathBuf};

use botwork_auth_broker::{
    auth::{opaque, AuthState, RateLimitConfig},
    build_router, build_user_api_router, spawn_prune_task, AppState, TtlConfig,
};
use sea_orm::{Database, DatabaseConnection};
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const PREFIX: &str = "[auth-broker]";
const VERSION: &str = include_str!("../../VERSION").trim_ascii();

/// Check whether the first argument after the binary name is a version
/// flag. If so, write `"botwork-auth-broker <full()>\n"` to `out` and
/// return `Some(0)`; otherwise return `None`.
///
/// Extracted into a testable helper so the dispatch logic can be
/// exercised against a `Vec<u8>` writer without spawning the whole
/// daemon.
fn handle_version_flag(args: &[String], out: &mut impl Write) -> Option<i32> {
    match args.first().map(String::as_str) {
        Some("--version") | Some("-V") => {
            let _ = writeln!(
                out,
                "botwork-auth-broker {}",
                botwork_version::format_full(VERSION, botwork_version::GIT_SHA)
            );
            Some(0)
        }
        _ => None,
    }
}

fn vault_root_from_env() -> PathBuf {
    vault_root_from_lookup(|k| std::env::var(k).ok())
}

fn vault_root_from_lookup(lookup: impl Fn(&str) -> Option<String>) -> PathBuf {
    lookup("BOTWORK_VAULT_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/botwork/vault"))
}

fn bind_from_env() -> String {
    bind_from_lookup(|k| std::env::var(k).ok())
}

fn bind_from_lookup(lookup: impl Fn(&str) -> Option<String>) -> String {
    // SECURITY: auth-broker trusts `x-envoy-original-path` for tenant identity, so it
    // MUST only ever be reachable via Envoy. In the supported deployment it runs as a
    // container on the `botwork` docker network and its port is NEVER published to the
    // host (no `-p`/`--publish`) -- the docker network namespace is the trust boundary,
    // not the bind address. Therefore the default is 0.0.0.0:9100 so Envoy (a *separate*
    // container) can reach it via the `auth_broker` network-alias; a 127.0.0.1 default
    // would bind only the container's own loopback and silently break ext_authz.
    //
    // DO NOT add a port publish for this service. If you instead run auth-broker as a
    // bare host process, set BOTWORK_AUTH_BROKER_BIND=127.0.0.1:9100 so it is not exposed
    // beyond loopback -- anything that can reach this port can attempt to unlock any
    // tenant vault it has the password for.
    lookup("BOTWORK_AUTH_BROKER_BIND").unwrap_or_else(|| "0.0.0.0:9100".to_string())
}

fn api_bind_from_env(auth_bind: &str) -> String {
    // Internal-only listener consumed by the `api` service as the
    // secret_store backend over the docker internal network. This
    // port is never user-facing and should not be routed by Envoy.
    std::env::var("BOTWORK_AUTH_BROKER_API_BIND").unwrap_or_else(|_| default_api_bind(auth_bind))
}

fn default_api_bind(auth_bind: &str) -> String {
    match auth_bind.parse::<std::net::SocketAddr>() {
        Ok(addr) => {
            let api_port = addr.port().saturating_add(1);
            format!("{}:{api_port}", addr.ip())
        }
        Err(_) => "0.0.0.0:9101".to_string(),
    }
}

/// Build the OPAQUE [`AuthState`] from the runtime environment.
///
/// Round 1b: this is the only auth surface. `BOTWORK_DATABASE_URL`
/// is the production input that wires the lease lookup. When unset
/// (or set to an empty string) the broker boots with a
/// lazily-connected pool pointing at a black-hole URL: every code
/// path that rejects *upstream* of the lease lookup (bare path,
/// missing/malformed bearer, malformed cap) continues to 401 with
/// the structured #125 envelope; any path that would actually
/// validate a lease 401s as `invalid_bearer` once the lazy
/// connect attempt times out. We log a loud warning at boot so an
/// operator who forgot the env var sees the misconfiguration.
///
/// This split keeps the container smoke test happy (the test
/// `docker run`s the image with no env and probes `GET /` for a
/// 401 + JSON envelope) without re-introducing the legacy
/// bearer-as-vault-password code path: a misconfigured broker
/// cannot mint a cap, cannot unlock a vault, cannot serve
/// secrets. It just emits structured 401s.
async fn build_auth_state(vault_root: &Path) -> AuthState {
    let offline = !matches!(std::env::var("BOTWORK_DATABASE_URL"), Ok(value) if !value.is_empty());

    let db = if offline {
        warn!(
            "{PREFIX} BOTWORK_DATABASE_URL unset — booting in offline mode. Every lease \
             lookup will fail with `invalid_bearer`; the broker can still serve structured \
             401s for misconfigured-bearer paths. Set BOTWORK_DATABASE_URL to a postgres \
             URL for the botwork database to enable lease validation."
        );
        offline_database_pool()
    } else {
        let value = std::env::var("BOTWORK_DATABASE_URL").expect("checked above");
        match Database::connect(&value).await {
            Ok(conn) => conn,
            Err(err) => {
                eprintln!("{PREFIX} failed to connect to database: {err}");
                std::process::exit(1);
            }
        }
    };

    // OPAQUE setup persistence: in production we write to
    // `<vault_root>/opaque_server_setup` so a broker restart picks
    // up the same setup (any other choice would invalidate every
    // existing `opaque_password_file` row). In offline mode
    // (BOTWORK_DATABASE_URL unset → CI container smoke test) we
    // generate a setup in memory — the distroless container runs
    // as non-root and the default vault root `/var/lib/botwork/vault`
    // isn't writable, so trying to persist would exit the broker
    // before the smoke test's `GET /` probe ever lands. The
    // in-memory setup is throwaway because no lease lookup can
    // succeed against the black-hole DB anyway.
    let setup = if offline {
        botwork_opaque_handshake::ServerSetup::generate(&mut rand::rng())
    } else {
        match opaque::load_or_generate_server_setup(vault_root).await {
            Ok(setup) => setup,
            Err(err) => {
                eprintln!("{PREFIX} failed to load/generate OPAQUE server setup: {err}");
                std::process::exit(1);
            }
        }
    };

    info!("{PREFIX} OPAQUE auth state wired; lease KEKs are derived from live bearers");
    // NOTE: do not reintroduce a server-side wrapping key here.
    // The previous design held a process-local key that sealed every
    // lease's session_key; every broker restart silently invalidated
    // every active lease (forcing re-register/re-login/re-init for
    // every tenant), and an operator with full server state could
    // decrypt every active lease's vault from a postgres dump.
    //
    // The current design (auth::lease_kek) derives the per-lease KEK
    // from the bearer on every request. The server has no persistent
    // key material. Broker restarts are non-destructive; operator
    // reading postgres + vault disk yields no plaintext.
    AuthState::new(db, setup)
}

/// Read the per-instance rate-limit config from environment variables.
///
/// - `BOTWORK_AUTH_BROKER_RATE_LIMIT_RPS` — sustained rate (tokens/sec);
///   `0` disables limiting entirely.
/// - `BOTWORK_AUTH_BROKER_RATE_LIMIT_BURST` — burst capacity (tokens).
///
/// See `SECURITY.md` for full details on the in-memory/per-instance
/// nature of the limiter and the safe defaults.
fn rate_limit_config_from_env() -> RateLimitConfig {
    let config = RateLimitConfig::from_env();
    if config.is_disabled() {
        info!("{PREFIX} rate limiting disabled (BOTWORK_AUTH_BROKER_RATE_LIMIT_RPS=0)");
    } else {
        info!(
            "{PREFIX} rate limiting enabled: rps={} burst={}",
            config.rate_per_second, config.burst
        );
    }
    config
}

/// Build a sea-orm `DatabaseConnection` backed by a sqlx postgres
/// pool with `min_connections=0` and a black-hole URL. The pool is
/// constructed (so the type system is satisfied and the broker
/// boots cleanly) but no TCP connect fires until a query is
/// actually run; the first such query will time out and surface as
/// a `validate_and_extend` DB error, which `try_lease_path` turns
/// into a `LeasePathOutcome::Miss` → 401 with `invalid_bearer`.
///
/// Same trick the auth-broker test harness in
/// `auth-broker/tests/common::offline_auth_state` uses — kept here
/// so the production binary has a usable degraded-mode boot when
/// `BOTWORK_DATABASE_URL` is unset (CI's container smoke test
/// runs without any env vars and just probes `GET /` for the
/// structured 401 envelope).
fn offline_database_pool() -> DatabaseConnection {
    use sea_orm::SqlxPostgresConnector;
    use std::time::Duration;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .min_connections(0)
        .max_connections(1)
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy("postgres://nobody@127.0.0.1:1/none")
        .expect("connect_lazy never fails on a parseable URL");
    SqlxPostgresConnector::from_sqlx_postgres_pool(pool)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = handle_version_flag(&args, &mut std::io::stdout()) {
        std::process::exit(code);
    }

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    info!(
        "{PREFIX} botwork-auth-broker {}",
        botwork_version::format_full(VERSION, botwork_version::GIT_SHA)
    );

    let bind = bind_from_env();
    let api_bind = api_bind_from_env(&bind);
    let vault_root = vault_root_from_env();
    let ttl_config = TtlConfig::from_env();
    let rate_limit_config = rate_limit_config_from_env();
    let auth = build_auth_state(&vault_root)
        .await
        .with_rate_limiter(rate_limit_config);
    let mut state = AppState::with_auth_and_ttl_config(vault_root, auth, ttl_config);
    if let Ok(admin_key) = std::env::var("BOTWORK_ADMIN_API_KEY") {
        if !admin_key.is_empty() {
            info!("{PREFIX} admin API key configured; DELETE /admin/api/v1/leases/:id enabled");
            state = state.with_admin_api_key(admin_key);
        } else {
            warn!("{PREFIX} BOTWORK_ADMIN_API_KEY is set but empty — admin surface disabled");
        }
    } else {
        info!("{PREFIX} BOTWORK_ADMIN_API_KEY unset — admin surface disabled");
    }
    let _prune_task = spawn_prune_task(state.clone());
    let app = build_router(state.clone());
    let api_app = build_user_api_router(state);

    let listener = TcpListener::bind(&bind).await.unwrap_or_else(|e| {
        eprintln!("{PREFIX} failed to bind {bind}: {e}");
        std::process::exit(1);
    });

    info!(
        "{PREFIX} starting on {}",
        listener.local_addr().expect("local addr")
    );
    let api_listener = TcpListener::bind(&api_bind).await.unwrap_or_else(|e| {
        eprintln!("{PREFIX} failed to bind {api_bind}: {e}");
        std::process::exit(1);
    });
    info!(
        "{PREFIX} internal secret-store API starting on {}",
        api_listener.local_addr().expect("local addr")
    );

    if let Err(err) = tokio::try_join!(
        axum::serve(listener, app),
        axum::serve(api_listener, api_app)
    ) {
        eprintln!("{PREFIX} server error: {err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_flag_long_writes_version_and_returns_zero() {
        let args = vec!["--version".to_string()];
        let mut out = Vec::new();
        let result = handle_version_flag(&args, &mut out);
        assert_eq!(result, Some(0));
        let output = String::from_utf8(out).unwrap();
        assert!(
            output.starts_with("botwork-auth-broker "),
            "expected output to start with 'botwork-auth-broker ', got: {output:?}"
        );
        assert!(
            output.contains(VERSION),
            "expected output to contain version {VERSION}, got: {output:?}",
        );
    }

    #[test]
    fn version_flag_short_writes_version_and_returns_zero() {
        let args = vec!["-V".to_string()];
        let mut out = Vec::new();
        let result = handle_version_flag(&args, &mut out);
        assert_eq!(result, Some(0));
        let output = String::from_utf8(out).unwrap();
        assert!(output.starts_with("botwork-auth-broker "));
    }

    #[test]
    fn no_version_flag_returns_none() {
        let args = vec!["--bind".to_string(), "0.0.0.0:9100".to_string()];
        let mut out = Vec::new();
        let result = handle_version_flag(&args, &mut out);
        assert_eq!(result, None);
        assert!(out.is_empty());
    }

    #[test]
    fn empty_args_returns_none() {
        let mut out = Vec::new();
        let result = handle_version_flag(&[], &mut out);
        assert_eq!(result, None);
        assert!(out.is_empty());
    }

    // ------------------------------------------------------------------
    // vault_root_from_lookup
    // ------------------------------------------------------------------

    #[test]
    fn vault_root_from_lookup_uses_provided_value() {
        let root = vault_root_from_lookup(|_| Some("/tmp/test-vault".to_string()));
        assert_eq!(root, PathBuf::from("/tmp/test-vault"));
    }

    #[test]
    fn vault_root_from_lookup_default_when_absent() {
        let root = vault_root_from_lookup(|_| None);
        assert_eq!(root, PathBuf::from("/var/lib/botwork/vault"));
    }

    // ------------------------------------------------------------------
    // bind_from_lookup
    // ------------------------------------------------------------------

    #[test]
    fn bind_from_lookup_uses_provided_value() {
        let bind = bind_from_lookup(|_| Some("127.0.0.1:9200".to_string()));
        assert_eq!(bind, "127.0.0.1:9200");
    }

    #[test]
    fn bind_from_lookup_default_when_absent() {
        let bind = bind_from_lookup(|_| None);
        assert_eq!(bind, "0.0.0.0:9100");
    }

    // ------------------------------------------------------------------
    // default_api_bind
    // ------------------------------------------------------------------

    #[test]
    fn default_api_bind_increments_port_for_valid_addr() {
        assert_eq!(default_api_bind("0.0.0.0:9100"), "0.0.0.0:9101");
        assert_eq!(default_api_bind("127.0.0.1:8080"), "127.0.0.1:8081");
    }

    #[test]
    fn default_api_bind_falls_back_for_invalid_addr() {
        assert_eq!(default_api_bind("not-a-socket-addr"), "0.0.0.0:9101");
    }

    #[test]
    fn default_api_bind_saturates_at_u16_max() {
        // port u16::MAX saturating_add(1) stays at u16::MAX.
        let addr = format!("0.0.0.0:{}", u16::MAX);
        let result = default_api_bind(&addr);
        assert_eq!(result, format!("0.0.0.0:{}", u16::MAX));
    }

    // ------------------------------------------------------------------
    // offline_database_pool
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn offline_database_pool_does_not_panic() {
        // Just verify it constructs without panicking; no actual connect.
        let _pool = offline_database_pool();
    }
}
