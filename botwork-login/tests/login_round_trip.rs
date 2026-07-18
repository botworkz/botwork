//! Full round-trip: register → login → status → env → /auth/check.
//!
//! Spawns a real `botwork-auth-broker` against a testcontainers
//! postgres and drives the entire CLI library surface end-to-end:
//!
//! 1. `register` seeds an `opaque_password_file` row.
//! 2. `login` mints a bearer + lease and writes a keyring entry
//!    (file fallback under a `tempdir`-rooted
//!    `$BOTWORK_LOGIN_KEYRING_DIR`).
//! 3. `status` reads the keyring entry, prints expiry + remaining.
//! 4. `env` prints `export <VAR>='<bearer>'`.
//! 5. `/auth/check` accepts the bearer (i.e. goose-side substitution
//!    of `${BOTWORK_BEARER}` would succeed).
//! 6. A second `login` rotates the bearer; the old bearer continues
//!    to authenticate via the broker's still-live lease row.
//!
//! Gated on `docker_available()`; log-skips when docker isn't
//! reachable so `cargo test --workspace` stays green on dev
//! machines without docker.

use std::time::Duration;

use botwork_auth_broker::auth::AuthState;
use botwork_auth_broker::{build_router, AppState};
use botwork_entity::tenant;
use botwork_login::commands::{
    env::EnvArgs, login::LoginArgs, logout::LogoutArgs, register::RegisterArgs, status::StatusArgs,
};
use botwork_login::commands::{run_env, run_login, run_logout, run_register, run_status};
use botwork_migration::Migrator;
use botwork_opaque_handshake::ServerSetup;
use chrono::Utc;
use reqwest::StatusCode;
use sea_orm::{ActiveModelTrait, Database, DatabaseConnection, Set};
use sea_orm_migration::MigratorTrait;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use uuid::Uuid;
use zeroize::Zeroizing;

const POSTGRES_TAG: &str = "16-alpine";

async fn docker_available() -> bool {
    use testcontainers::core::WaitFor;
    use testcontainers::runners::AsyncRunner;
    use testcontainers::GenericImage;
    let probe =
        GenericImage::new("testcontainers/helloworld", "1.3.0").with_wait_for(WaitFor::seconds(1));
    match tokio::time::timeout(Duration::from_secs(5), probe.start()).await {
        Ok(Ok(container)) => {
            let _ = container.rm().await;
            true
        }
        _ => false,
    }
}

struct Server {
    base: String,
    db: DatabaseConnection,
    _server: JoinHandle<()>,
    _pg: testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
}

async fn spawn() -> Option<Server> {
    if !docker_available().await {
        return None;
    }
    use testcontainers::runners::AsyncRunner;
    use testcontainers::ImageExt;
    use testcontainers_modules::postgres::Postgres;

    let image = Postgres::default()
        .with_db_name("botwork")
        .with_user("botwork")
        .with_password("test")
        .with_tag(POSTGRES_TAG);
    let pg = image.start().await.ok()?;
    let host = pg.get_host().await.ok()?;
    let port = pg.get_host_port_ipv4(5432).await.ok()?;
    let url = format!("postgres://botwork:test@{host}:{port}/botwork");
    let db = Database::connect(&url).await.ok()?;
    Migrator::up(&db, None).await.ok()?;

    let vault_root = tempfile::tempdir().ok()?;
    let setup = ServerSetup::generate(&mut rand::thread_rng());
    let auth = AuthState::new(db.clone(), setup);
    let state = AppState::with_auth(vault_root.path().to_path_buf(), auth);
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Leak the temp dir so the handler keeps a valid path. Same
    // pattern `opaque_e2e` uses for its vault root.
    std::mem::forget(vault_root);
    Some(Server {
        base: format!("http://{addr}"),
        db,
        _server: server,
        _pg: pg,
    })
}

async fn seed_tenant(db: &DatabaseConnection, name: &str) -> Uuid {
    let now = Utc::now();
    let model = tenant::ActiveModel {
        id: Set(Uuid::new_v4()),
        name: Set(name.to_string()),
        created_at: Set(now),
        updated_at: Set(now),
    };
    model.insert(db).await.expect("insert tenant").id
}

/// Set up an isolated keyring fallback directory + config-file
/// override for the duration of one test. Returned guard restores
/// the env on drop.
struct EnvGuard {
    _keyring_dir: TempDir,
    saved_keyring: Option<String>,
    saved_config: Option<String>,
}

impl EnvGuard {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("keyring dir");
        let saved_keyring = std::env::var("BOTWORK_LOGIN_KEYRING_DIR").ok();
        let saved_config = std::env::var("BOTWORK_LOGIN_CONFIG").ok();
        std::env::set_var("BOTWORK_LOGIN_KEYRING_DIR", dir.path());
        // Point at a never-existing config file so the CLI doesn't
        // pick up the host's `~/.config/botspace/config.toml` during
        // the test.
        let config_path = dir.path().join("config.toml");
        std::env::set_var("BOTWORK_LOGIN_CONFIG", &config_path);
        Self {
            _keyring_dir: dir,
            saved_keyring,
            saved_config,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.saved_keyring.take() {
            Some(v) => std::env::set_var("BOTWORK_LOGIN_KEYRING_DIR", v),
            None => std::env::remove_var("BOTWORK_LOGIN_KEYRING_DIR"),
        }
        match self.saved_config.take() {
            Some(v) => std::env::set_var("BOTWORK_LOGIN_CONFIG", v),
            None => std::env::remove_var("BOTWORK_LOGIN_CONFIG"),
        }
    }
}

fn password(bytes: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    Some(Zeroizing::new(bytes.to_vec()))
}

#[tokio::test(flavor = "multi_thread")]
async fn full_register_login_status_env_round_trip() {
    let Some(srv) = spawn().await else {
        eprintln!(
            "IGNORED: docker not reachable, skipping \
             login_round_trip::full_register_login_status_env_round_trip"
        );
        return;
    };
    let _guard = EnvGuard::new();
    seed_tenant(&srv.db, "phlax").await;

    // register -----------------------------------------------------
    let msg = run_register(RegisterArgs {
        tenant: "phlax".into(),
        server: Some(srv.base.clone()),
        password: password(b"hunter2"),
        password_stdin: false,
        credential_identifier: None,
        cacert: None,
    })
    .await
    .expect("register");
    assert!(msg.contains("Registered tenant 'phlax'"), "got {msg}");

    // login --------------------------------------------------------
    let msg = run_login(LoginArgs {
        tenant: "phlax".into(),
        server: Some(srv.base.clone()),
        password: password(b"hunter2"),
        password_stdin: false,
        lease: Some("1h".into()),
        credential_identifier: None,
        cacert: None,
    })
    .await
    .expect("login");
    assert!(msg.starts_with("✓ Logged in to phlax."), "got {msg}");

    // status -------------------------------------------------------
    let msg = run_status(StatusArgs {
        tenant: "phlax".into(),
    })
    .await
    .expect("status");
    assert!(msg.contains("phlax: logged in."), "got {msg}");
    assert!(msg.contains("Lease id:"), "got {msg}");
    assert!(msg.contains(&srv.base), "status must echo server URL");

    // env ----------------------------------------------------------
    let msg = run_env(EnvArgs {
        tenant: "phlax".into(),
        token_env: None,
    })
    .await
    .expect("env");
    let export_prefix = "export BOTWORK_BEARER='";
    assert!(msg.starts_with(export_prefix), "got {msg}");
    let bearer = msg
        .strip_prefix(export_prefix)
        .and_then(|s| s.strip_suffix('\''))
        .expect("env export shape");

    // /auth/check via the broker --------------------------------
    let response = reqwest::Client::new()
        .post(format!("{}/auth/check", srv.base))
        .header("authorization", format!("Bearer {bearer}"))
        .header("x-envoy-original-path", "/phlax/ns/exec-bash")
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the bearer from `env` must authenticate against /auth/check"
    );

    // re-login rotates the bearer; the old one is still valid.
    let _ = run_login(LoginArgs {
        tenant: "phlax".into(),
        server: Some(srv.base.clone()),
        password: password(b"hunter2"),
        password_stdin: false,
        lease: Some("1h".into()),
        credential_identifier: None,
        cacert: None,
    })
    .await
    .expect("re-login");
    let msg2 = run_env(EnvArgs {
        tenant: "phlax".into(),
        token_env: None,
    })
    .await
    .expect("env after re-login");
    let bearer2 = msg2
        .strip_prefix(export_prefix)
        .and_then(|s| s.strip_suffix('\''))
        .expect("env export shape");
    assert_ne!(
        bearer, bearer2,
        "re-login must mint a distinct bearer (lease rotation)"
    );

    // The previous bearer continues to authenticate — the broker
    // didn't revoke the row, only the keyring rotated.
    let response = reqwest::Client::new()
        .post(format!("{}/auth/check", srv.base))
        .header("authorization", format!("Bearer {bearer}"))
        .header("x-envoy-original-path", "/phlax/ns/exec-bash")
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the previous bearer must still authenticate until its lease expires server-side"
    );

    // logout ------------------------------------------------------
    let msg = run_logout(LogoutArgs {
        tenant: "phlax".into(),
    })
    .await
    .expect("logout");
    assert!(
        msg.starts_with("✓ Removed keyring entry for phlax."),
        "got {msg}"
    );

    // After logout, status / env must surface NoLease.
    let err = run_status(StatusArgs {
        tenant: "phlax".into(),
    })
    .await
    .expect_err("status after logout");
    assert!(
        matches!(&err, botwork_login::error::LoginError::NoLease(name) if name == "phlax"),
        "got {err:?}"
    );
}
