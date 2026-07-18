//! Wire-error mapping integration tests.
//!
//! Each test spins up a real `botwork-auth-broker` against a
//! testcontainers postgres, then drives the CLI's library entry
//! points to confirm the broker's status codes map onto the
//! expected typed [`LoginError`] arms:
//!
//! - 401 wrong-password against a real tenant → `InvalidLogin`
//! - 404 on `register/start` against an unknown tenant → `UnknownTenant`
//! - 409 on `register/finish` for a tenant that already has a
//!   `password_file` row → `AlreadyRegistered`
//!
//! All four tests gate on `docker_available()` and log-skip cleanly
//! when docker isn't reachable, so `cargo test --workspace` stays
//! green on dev machines without docker. The full proof runs in CI.

use std::time::Duration;

use botwork_auth_broker::auth::AuthState;
use botwork_auth_broker::{build_router, AppState};
use botwork_entity::tenant;
use botwork_login::client::{run_login, run_register};
use botwork_login::error::LoginError;
use botwork_migration::Migrator;
use botwork_opaque_handshake::ServerSetup;
use chrono::Utc;
use sea_orm::{ActiveModelTrait, Database, DatabaseConnection, Set};
use sea_orm_migration::MigratorTrait;
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use url::Url;
use uuid::Uuid;

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
    base: Url,
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

    let vault_root = tempdir().ok()?;
    let setup = ServerSetup::generate(&mut rand::thread_rng());
    let auth = AuthState::new(db.clone(), setup);
    let state = AppState::with_auth(vault_root.path().to_path_buf(), auth);
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // The vault_root tempdir must outlive the running server; leak
    // it so the temp dir's Drop doesn't fire before the handler
    // releases the path.
    std::mem::forget(vault_root);
    Some(Server {
        base: format!("http://{addr}")
            .parse()
            .expect("local server addr is a valid http:// base url"),
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

#[tokio::test]
async fn login_against_unknown_tenant_surfaces_invalidlogin() {
    let Some(srv) = spawn().await else {
        eprintln!(
            "IGNORED: docker not reachable, skipping \
             error_mapping::login_against_unknown_tenant_surfaces_invalidlogin"
        );
        return;
    };
    // No tenant rows seeded — the broker drives the OPAQUE dummy
    // flow on `/auth/login/start` and the *client*-side
    // `login_finish` catches `InvalidLogin` before the wire path
    // gets a chance to 401. That's the canonical wrong-password
    // arm and the only path that should produce `InvalidLogin`.
    let err = run_login(&srv.base, "ghost", "ghost", b"hunter2", 600, None)
        .await
        .expect_err("unknown tenant must error");
    assert!(matches!(&err, LoginError::InvalidLogin(_)), "got {err:?}");
}

#[tokio::test]
async fn register_against_unknown_tenant_surfaces_unknown_tenant() {
    let Some(srv) = spawn().await else {
        eprintln!(
            "IGNORED: docker not reachable, skipping \
             error_mapping::register_against_unknown_tenant_surfaces_unknown_tenant"
        );
        return;
    };
    let err = run_register(&srv.base, "ghost", "ghost", b"hunter2", None)
        .await
        .expect_err("unknown tenant on register must error");
    assert!(
        matches!(&err, LoginError::UnknownTenant(name) if name == "ghost"),
        "got {err:?}"
    );
}

#[tokio::test]
async fn register_then_register_again_surfaces_already_registered() {
    let Some(srv) = spawn().await else {
        eprintln!(
            "IGNORED: docker not reachable, skipping \
             error_mapping::register_then_register_again_surfaces_already_registered"
        );
        return;
    };
    seed_tenant(&srv.db, "phlax").await;

    // First registration must succeed.
    let first = run_register(&srv.base, "phlax", "phlax", b"hunter2", None)
        .await
        .expect("first register");
    assert_eq!(first.tenant, "phlax");

    // Second registration against the same tenant must 409 →
    // AlreadyRegistered.
    let err = run_register(&srv.base, "phlax", "phlax", b"hunter2", None)
        .await
        .expect_err("second register must error");
    assert!(
        matches!(&err, LoginError::AlreadyRegistered(name) if name == "phlax"),
        "got {err:?}"
    );
}

#[tokio::test]
async fn login_with_wrong_password_against_real_tenant_surfaces_invalidlogin() {
    let Some(srv) = spawn().await else {
        eprintln!(
            "IGNORED: docker not reachable, skipping \
             error_mapping::login_with_wrong_password_against_real_tenant_surfaces_invalidlogin"
        );
        return;
    };
    seed_tenant(&srv.db, "phlax").await;
    run_register(&srv.base, "phlax", "phlax", b"hunter2", None)
        .await
        .expect("register");

    let err = run_login(
        &srv.base,
        "phlax",
        "phlax",
        b"the-wrong-password",
        600,
        None,
    )
    .await
    .expect_err("wrong password must error");
    assert!(
        matches!(&err, LoginError::InvalidLogin(name) if name == "phlax"),
        "got {err:?}"
    );
}
