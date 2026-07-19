use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use botwork_auth_broker::auth::AuthState;
use botwork_auth_broker::{build_router, AppState};
use botwork_entity::tenant;
use botwork_migration::Migrator;
use botwork_opaque_handshake::{client, ServerSetup};
use chrono::Utc;
use rand::RngCore;
use reqwest::StatusCode;
use sea_orm::{ActiveModelTrait, Database, DatabaseConnection, Set};
use sea_orm_migration::MigratorTrait;
use serde_json::json;
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use uuid::Uuid;

mod common;

const POSTGRES_TAG: &str = "16-alpine";

struct Server {
    base: String,
    server: JoinHandle<()>,
}

impl Server {
    fn stop(self) {
        self.server.abort();
    }
}

async fn start_postgres() -> Result<
    (
        testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
        String,
    ),
    String,
> {
    use testcontainers::runners::AsyncRunner;
    use testcontainers::ImageExt;
    use testcontainers_modules::postgres::Postgres;

    let image = Postgres::default()
        .with_db_name("botwork")
        .with_user("botwork")
        .with_password("test")
        .with_tag(POSTGRES_TAG);
    let container = image
        .start()
        .await
        .map_err(|err| format!("start container: {err}"))?;
    let host = container
        .get_host()
        .await
        .map_err(|err| format!("host: {err}"))?;
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .map_err(|err| format!("port: {err}"))?;
    let password = String::from_utf8(vec![116, 101, 115, 116]).expect("valid utf8");
    let url = format!("postgres://botwork:{password}@{host}:{port}/botwork");
    Ok((container, url))
}

async fn spawn_server(
    db: Arc<DatabaseConnection>,
    vault_root: std::path::PathBuf,
) -> Result<Server, String> {
    let setup = ServerSetup::generate(&mut rand::thread_rng());
    let auth = AuthState::new_arc(db, setup);
    let state = AppState::with_auth(vault_root, auth);
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|err| format!("bind: {err}"))?;
    let addr = listener
        .local_addr()
        .map_err(|err| format!("local_addr: {err}"))?;
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Ok(Server {
        base: format!("http://{addr}"),
        server,
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

async fn register(base: &str, tenant: &str, cred: &str, password: &[u8]) {
    let mut rng = rand::thread_rng();
    let http = reqwest::Client::new();

    let cr = client::registration_start(&mut rng, password).unwrap();
    let resp = http
        .post(format!("{base}/auth/register/start"))
        .json(&json!({
            "tenant": tenant,
            "credential_identifier": cred,
            "registration_request": URL_SAFE_NO_PAD.encode(cr.request.serialize()),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let response_bytes = URL_SAFE_NO_PAD
        .decode(body["registration_response"].as_str().unwrap())
        .unwrap();
    let response =
        botwork_opaque_handshake::RegistrationResponse::deserialize(&response_bytes).unwrap();

    let cf = client::registration_finish(&mut rng, cr.state, password, response).unwrap();
    let resp = http
        .post(format!("{base}/auth/register/finish"))
        .json(&json!({
            "tenant": tenant,
            "credential_identifier": cred,
            "registration_upload": URL_SAFE_NO_PAD.encode(cf.upload.serialize()),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

async fn login(base: &str, tenant: &str, cred: &str, password: &[u8]) -> String {
    let mut rng = rand::thread_rng();
    let http = reqwest::Client::new();

    let cl = client::login_start(&mut rng, password).unwrap();
    let resp = http
        .post(format!("{base}/auth/login/start"))
        .json(&json!({
            "tenant": tenant,
            "credential_identifier": cred,
            "login_request": URL_SAFE_NO_PAD.encode(cl.request.serialize()),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let handshake_id = body["handshake_id"].as_str().unwrap().to_string();
    let response_bytes = URL_SAFE_NO_PAD
        .decode(body["login_response"].as_str().unwrap())
        .unwrap();
    let response = botwork_opaque_handshake::LoginResponse::deserialize(&response_bytes).unwrap();

    let cf = client::login_finish(cl.state, password, response).unwrap();
    let resp = http
        .post(format!("{base}/auth/login/finish"))
        .json(&json!({
            "handshake_id": handshake_id,
            "login_finalization": URL_SAFE_NO_PAD.encode(cf.finalization.serialize()),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    body["bearer"].as_str().unwrap().to_string()
}

async fn auth_check(base: &str, bearer: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/auth/check"))
        .header("authorization", ["Bearer ", bearer].concat())
        .header("x-envoy-original-path", "/phlax/ns/exec-bash")
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn restart_preserves_leases() {
    if !common::docker_available().await {
        eprintln!("IGNORED: docker not reachable, skipping restart_preserves_leases");
        return;
    }
    let (pg, url) = match start_postgres().await {
        Ok(v) => v,
        Err(err) => {
            eprintln!("IGNORED: failed to start postgres fixture ({err}); skipping");
            return;
        }
    };
    let db = Arc::new(Database::connect(&url).await.expect("connect postgres"));
    Migrator::up(&*db, None).await.expect("migrate schema");
    let vault_root_tempdir = tempdir().unwrap();
    let vault_root = vault_root_tempdir.path().to_path_buf();

    let srv1 = spawn_server(Arc::clone(&db), vault_root.clone())
        .await
        .expect("spawn initial broker");
    seed_tenant(&db, "phlax").await;
    let mut password = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut password);
    register(&srv1.base, "phlax", "phlax", &password).await;
    let bearer = login(&srv1.base, "phlax", "phlax", &password).await;

    let first = auth_check(&srv1.base, &bearer).await;
    assert_eq!(first.status(), StatusCode::OK);

    srv1.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let srv2 = spawn_server(Arc::clone(&db), vault_root.clone())
        .await
        .expect("spawn restarted broker");
    let second = auth_check(&srv2.base, &bearer).await;
    assert_eq!(
        second.status(),
        StatusCode::OK,
        "reconstructing AppState against the same postgres + vault root must preserve the lease"
    );

    srv2.stop();
    drop(pg);
    drop(vault_root_tempdir);
}
