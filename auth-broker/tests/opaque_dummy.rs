//! `opaque_dummy` — OPAQUE enumeration-resistance: logging in as a
//! tenant that has no `opaque_password_file` row MUST produce a
//! wire response of the same *shape* as a real tenant's, then fail
//! at `login_finish` on the client side via `OpaqueError::InvalidLogin`.
//!
//! Without this property an attacker could enumerate which tenants
//! have registered by comparing the byte-length of `login_response`
//! (or even just timing). The crate-level acceptance for #133 calls
//! this out explicitly.

use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use botwork_auth_broker::auth::AuthState;
use botwork_auth_broker::{build_router, AppState};
use botwork_entity::tenant;
use botwork_migration::Migrator;
use botwork_opaque_handshake::{client, ServerSetup};
use chrono::Utc;
use reqwest::StatusCode;
use sea_orm::{ActiveModelTrait, Database, DatabaseConnection, Set};
use sea_orm_migration::MigratorTrait;
use serde_json::json;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
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

async fn spawn_minimum() -> Option<(
    String,
    Arc<DatabaseConnection>,
    JoinHandle<()>,
    testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
)> {
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
    let db = Arc::new(Database::connect(&url).await.ok()?);
    Migrator::up(&*db, None).await.ok()?;

    let vault_root = tempdir().unwrap();
    let setup = ServerSetup::generate(&mut rand::thread_rng());
    let auth = AuthState::new_arc(Arc::clone(&db), setup);
    let state = AppState::with_auth(vault_root.path().to_path_buf(), auth);
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // vault_root must outlive the server; box-leak it so the temp
    // dir's drop doesn't fire while the handler still holds the
    // path.
    std::mem::forget(vault_root);
    Some((format!("http://{addr}"), db, server, pg))
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

async fn drive_login_start(
    base: &str,
    tenant: &str,
    cred: &str,
) -> Option<(serde_json::Value, usize)> {
    // Returns (json_body, login_response_byte_length) so the
    // shape-equality assertion can be done at the byte level.
    let mut rng = rand::thread_rng();
    let cl = client::login_start(&mut rng, b"anything-pwd").unwrap();
    let resp = reqwest::Client::new()
        .post(format!("{base}/auth/login/start"))
        .json(&json!({
            "tenant": tenant,
            "credential_identifier": cred,
            "login_request": URL_SAFE_NO_PAD.encode(cl.request.serialize()),
        }))
        .send()
        .await
        .ok()?;
    if resp.status() != StatusCode::OK {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    let bytes_len = URL_SAFE_NO_PAD
        .decode(body["login_response"].as_str()?)
        .ok()?
        .len();
    Some((body, bytes_len))
}

#[tokio::test]
async fn unknown_tenant_login_start_matches_known_tenant_shape() {
    let Some((base, db, server, pg)) = spawn_minimum().await else {
        eprintln!("IGNORED: docker not reachable, skipping opaque_dummy");
        return;
    };
    seed_tenant(&db, "known").await;

    // No register() for the known tenant on purpose — `login_start`
    // against a tenant that *exists* but has no password_file row
    // ALSO has to go through the dummy flow. So we have two
    // wire-comparable cases:
    //   1. tenant exists, password_file present (real registration)
    //   2. tenant exists, password_file missing (dummy)
    //   3. tenant doesn't exist (dummy)
    //
    // The bytes-length contract has to hold for *all three*. This
    // test pins (2) and (3) against each other since the issue
    // body's enumeration concern is exactly \"can the wire
    // distinguish them?\".
    let (_a, len_unknown) = drive_login_start(&base, "definitely-not-a-tenant", "x")
        .await
        .expect("login_start for unknown tenant should still respond 200");
    let (_b, len_known_no_pf) = drive_login_start(&base, "known", "x")
        .await
        .expect("login_start for known-no-pf tenant should still respond 200");

    assert_eq!(
        len_unknown, len_known_no_pf,
        "OPAQUE enumeration: login_response bytes must be the same length \
         regardless of whether the tenant is known on the server side"
    );

    server.abort();
    drop(pg);
}
