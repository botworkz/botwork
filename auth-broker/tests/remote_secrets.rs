//! `remote_secrets` — end-to-end tests for the remote-write endpoints
//! added in this PR:
//!
//!  - `POST  /secrets`
//!  - `DELETE /secrets/<service>/<name>?tenant=<tenant>`
//!
//! Each test follows the same harness pattern as `opaque_e2e`:
//! spin up a real postgres via testcontainers, run migrations, seed a
//! tenant, register + login to mint a bearer, then exercise the new
//! endpoints.
//!
//! ## Docker gating
//!
//! Docker-dependent tests are skipped with an `IGNORED:` line when
//! docker is unreachable so `cargo test --workspace` stays green on
//! dev machines without docker.

use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use botwork_auth_broker::auth::AuthState;
use botwork_auth_broker::{build_router, build_user_api_router, AppState};
use botwork_entity::{lease, tenant};
use botwork_migration::Migrator;
use botwork_opaque_handshake::{client, ServerSetup, SUITE_VERSION};
use botwork_vault::Vault;
use chrono::Utc;
use reqwest::StatusCode;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Database, DatabaseConnection, EntityTrait, QueryFilter, Set,
};
use sea_orm_migration::MigratorTrait;
use serde_json::json;
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
    let url = format!("postgres://botwork:test@{host}:{port}/botwork");
    Ok((container, url))
}

struct Server {
    base: String,
    api_base: String,
    db: Arc<DatabaseConnection>,
    vault_root: std::path::PathBuf,
    _server: JoinHandle<()>,
    _api_server: JoinHandle<()>,
    _pg: testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
}

struct ApiAuthSession {
    bearer: String,
    cookie: String,
    lease_id: Uuid,
}

async fn spawn() -> Result<Server, String> {
    let (pg, url) = start_postgres().await?;
    let db = Arc::new(
        Database::connect(&url)
            .await
            .map_err(|err| format!("connect: {err}"))?,
    );
    Migrator::up(&*db, None)
        .await
        .map_err(|err| format!("migrate: {err}"))?;
    let vault_root_tempdir = tempdir().unwrap();
    let vault_root = vault_root_tempdir.path().to_path_buf();
    std::mem::forget(vault_root_tempdir);

    let setup = ServerSetup::generate(&mut rand::thread_rng());
    let auth = AuthState::new_arc(Arc::clone(&db), setup);
    let state = AppState::with_auth(vault_root.clone(), auth);
    let app = build_router(state.clone());
    let api_app = build_user_api_router(state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|err| format!("bind: {err}"))?;
    let api_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|err| format!("api bind: {err}"))?;
    let addr = listener
        .local_addr()
        .map_err(|err| format!("local_addr: {err}"))?;
    let api_addr = api_listener
        .local_addr()
        .map_err(|err| format!("api local_addr: {err}"))?;
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let api_server = tokio::spawn(async move {
        axum::serve(api_listener, api_app).await.unwrap();
    });
    Ok(Server {
        base: format!("http://{addr}"),
        api_base: format!("http://{api_addr}"),
        db,
        vault_root,
        _server: server,
        _api_server: api_server,
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
    let inserted = model.insert(db).await.expect("insert tenant");
    inserted.id
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

async fn api_login(base: &str, tenant: &str, cred: &str, password: &[u8]) -> ApiAuthSession {
    let mut rng = rand::thread_rng();
    let http = reqwest::Client::new();

    let cl = client::login_start(&mut rng, password).unwrap();
    let start = http
        .post(format!("{base}/api/auth/login"))
        .json(&json!({
            "tenant": tenant,
            "credential_identifier": cred,
            "opaque_login_request": URL_SAFE_NO_PAD.encode(cl.request.serialize()),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(start.status(), StatusCode::OK);
    let start_body: serde_json::Value = start.json().await.unwrap();
    let handshake_id = start_body["handshake_id"].as_str().unwrap().to_string();
    let response_bytes = URL_SAFE_NO_PAD
        .decode(start_body["opaque_login_response"].as_str().unwrap())
        .unwrap();
    let response = botwork_opaque_handshake::LoginResponse::deserialize(&response_bytes).unwrap();

    let cf = client::login_finish(cl.state, password, response).unwrap();
    let finish = http
        .post(format!("{base}/api/auth/login"))
        .json(&json!({
            "handshake_id": handshake_id,
            "opaque_login_finalization": URL_SAFE_NO_PAD.encode(cf.finalization.serialize()),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(finish.status(), StatusCode::OK);
    let cookie = finish
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let finish_body: serde_json::Value = finish.json().await.unwrap();
    ApiAuthSession {
        bearer: finish_body["bearer"].as_str().unwrap().to_string(),
        cookie,
        lease_id: Uuid::parse_str(finish_body["lease_id"].as_str().unwrap()).unwrap(),
    }
}

async fn init_vault(base: &str, tenant_root: &std::path::Path, bearer: &str) {
    let wek_resp = reqwest::Client::new()
        .get(format!("{base}/auth/lease/wrapped-export-key"))
        .header("authorization", ["Bearer ", bearer].concat())
        .send()
        .await
        .unwrap();
    assert_eq!(wek_resp.status(), StatusCode::OK);
    let wek_body: serde_json::Value = wek_resp.json().await.unwrap();
    let wrapped = URL_SAFE_NO_PAD
        .decode(wek_body["wrapped_export_key"].as_str().unwrap())
        .unwrap();
    let suite = wek_body["suite_version"].as_u64().unwrap() as u8;
    assert_eq!(suite, SUITE_VERSION);
    Vault::create(tenant_root, &wrapped, suite).expect("vault create");
}

async fn auth_check_with_cookie(base: &str, cookie: &str, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/auth/check"))
        .header("cookie", cookie)
        .header("x-envoy-original-path", path)
        .send()
        .await
        .unwrap()
}

/// Remote-store a secret via `POST /secrets`.
async fn remote_store(
    base: &str,
    tenant: &str,
    service: &str,
    name: &str,
    value: &[u8],
    overwrite: bool,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/secrets"))
        .json(&json!({
            "tenant": tenant,
            "service": service,
            "name": name,
            "kind": "api-key",
            "value_b64": STANDARD.encode(value),
            "allowed_consumers": ["exec-bash"],
            "tags": [],
            "overwrite": overwrite,
        }))
        .send()
        .await
        .unwrap()
}

/// Remote-delete a secret via `DELETE /secrets/<service>/<name>?tenant=<tenant>`.
async fn remote_delete(base: &str, tenant: &str, service: &str, name: &str) -> reqwest::Response {
    reqwest::Client::new()
        .delete(format!("{base}/secrets/{service}/{name}?tenant={tenant}"))
        .send()
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// 1. Happy path: register → login → remote store → /secrets/fetch
///    from a plugin → assert plaintext matches what was stored.
#[tokio::test]
async fn store_and_fetch_round_trip() {
    if !docker_available().await {
        eprintln!(
            "IGNORED: docker not reachable, skipping remote_secrets::store_and_fetch_round_trip"
        );
        return;
    }
    let srv = match spawn().await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("IGNORED: failed to spawn fixture ({err}); skipping");
            return;
        }
    };

    seed_tenant(&srv.db, "phlax").await;
    // Init a vault first (needed so unlock_master can open the file).
    register(&srv.base, "phlax", "phlax", b"hunter2").await;
    let bearer = login(&srv.base, "phlax", "phlax", b"hunter2").await;

    // Pre-create the vault using the existing wrapped-export-key path.
    let wek_resp = reqwest::Client::new()
        .get(format!("{}/auth/lease/wrapped-export-key", srv.base))
        .header("authorization", format!("Bearer {bearer}"))
        .send()
        .await
        .unwrap();
    assert_eq!(wek_resp.status(), StatusCode::OK);
    let wek_body: serde_json::Value = wek_resp.json().await.unwrap();
    let wrapped = URL_SAFE_NO_PAD
        .decode(wek_body["wrapped_export_key"].as_str().unwrap())
        .unwrap();
    let suite = wek_body["suite_version"].as_u64().unwrap() as u8;
    assert_eq!(suite, SUITE_VERSION);
    let tenant_root = srv.vault_root.join("phlax");
    Vault::create(&tenant_root, &wrapped, suite).expect("vault create");

    // Remote-store a secret.
    let resp = remote_store(
        &srv.api_base,
        "phlax",
        "github.com",
        "pat",
        b"SECRET_TOKEN",
        false,
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "store: {}",
        resp.text().await.unwrap()
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["stored"], "github.com/pat");
    assert_eq!(body["created"], true);

    // /auth/check to mint a cap.
    let check_resp = reqwest::Client::new()
        .post(format!("{}/auth/check", srv.base))
        .header("authorization", format!("Bearer {bearer}"))
        .header("x-envoy-original-path", "/phlax/mcp/exec-bash")
        .send()
        .await
        .unwrap();
    assert_eq!(check_resp.status(), StatusCode::OK);
    let cap = check_resp
        .headers()
        .get("x-botwork-cap")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();

    // /secrets/fetch returns the stored secret.
    let fetch = reqwest::Client::new()
        .post(format!("{}/secrets/fetch", srv.base))
        .header("x-botwork-cap", &cap)
        .send()
        .await
        .unwrap();
    assert_eq!(fetch.status(), StatusCode::OK);
    let fbody: serde_json::Value = fetch.json().await.unwrap();
    let secrets = fbody["secrets"].as_array().unwrap();
    assert_eq!(secrets.len(), 1);
    let plaintext = STANDARD
        .decode(secrets[0]["value_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(plaintext, b"SECRET_TOKEN");
}

/// 2. Overwrite gate: first store succeeds, second without overwrite→409,
///    third with overwrite:true→200 with created:false.
#[tokio::test]
async fn overwrite_gate() {
    if !docker_available().await {
        eprintln!("IGNORED: docker not reachable, skipping remote_secrets::overwrite_gate");
        return;
    }
    let srv = match spawn().await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("IGNORED: failed to spawn fixture ({err}); skipping");
            return;
        }
    };

    seed_tenant(&srv.db, "phlax").await;
    register(&srv.base, "phlax", "phlax", b"hunter2").await;
    let bearer = login(&srv.base, "phlax", "phlax", b"hunter2").await;

    // Create vault.
    let wek_resp = reqwest::Client::new()
        .get(format!("{}/auth/lease/wrapped-export-key", srv.base))
        .header("authorization", format!("Bearer {bearer}"))
        .send()
        .await
        .unwrap();
    let wek_body: serde_json::Value = wek_resp.json().await.unwrap();
    let wrapped = URL_SAFE_NO_PAD
        .decode(wek_body["wrapped_export_key"].as_str().unwrap())
        .unwrap();
    let suite = wek_body["suite_version"].as_u64().unwrap() as u8;
    Vault::create(srv.vault_root.join("phlax"), &wrapped, suite).unwrap();

    // First store → created:true.
    let r1 = remote_store(&srv.api_base, "phlax", "github.com", "pat", b"v1", false).await;
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(
        r1.json::<serde_json::Value>().await.unwrap()["created"],
        true
    );

    // Second store without overwrite → 409.
    let r2 = remote_store(&srv.api_base, "phlax", "github.com", "pat", b"v2", false).await;
    assert_eq!(r2.status(), StatusCode::CONFLICT);
    let body2: serde_json::Value = r2.json().await.unwrap();
    assert_eq!(body2["error"]["code"], "already_exists");

    // Third store with overwrite:true → 200, created:false.
    let r3 = remote_store(&srv.api_base, "phlax", "github.com", "pat", b"v3", true).await;
    assert_eq!(r3.status(), StatusCode::OK);
    assert_eq!(
        r3.json::<serde_json::Value>().await.unwrap()["created"],
        false
    );
}

/// 3. Delete round-trip: store → delete → fetch → secret missing.
#[tokio::test]
async fn delete_round_trip() {
    if !docker_available().await {
        eprintln!("IGNORED: docker not reachable, skipping remote_secrets::delete_round_trip");
        return;
    }
    let srv = match spawn().await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("IGNORED: failed to spawn fixture ({err}); skipping");
            return;
        }
    };

    seed_tenant(&srv.db, "phlax").await;
    register(&srv.base, "phlax", "phlax", b"hunter2").await;
    let bearer = login(&srv.base, "phlax", "phlax", b"hunter2").await;

    let wek_resp = reqwest::Client::new()
        .get(format!("{}/auth/lease/wrapped-export-key", srv.base))
        .header("authorization", format!("Bearer {bearer}"))
        .send()
        .await
        .unwrap();
    let wek_body: serde_json::Value = wek_resp.json().await.unwrap();
    let wrapped = URL_SAFE_NO_PAD
        .decode(wek_body["wrapped_export_key"].as_str().unwrap())
        .unwrap();
    let suite = wek_body["suite_version"].as_u64().unwrap() as u8;
    Vault::create(srv.vault_root.join("phlax"), &wrapped, suite).unwrap();

    // Store.
    let rs = remote_store(&srv.api_base, "phlax", "aws", "key", b"AWSSECRET", false).await;
    assert_eq!(rs.status(), StatusCode::OK);

    // Delete.
    let rd = remote_delete(&srv.api_base, "phlax", "aws", "key").await;
    assert_eq!(rd.status(), StatusCode::NO_CONTENT);

    // After delete, /auth/check + /secrets/fetch should return no secrets.
    let check_resp = reqwest::Client::new()
        .post(format!("{}/auth/check", srv.base))
        .header("authorization", format!("Bearer {bearer}"))
        .header("x-envoy-original-path", "/phlax/mcp/exec-bash")
        .send()
        .await
        .unwrap();
    assert_eq!(check_resp.status(), StatusCode::OK);
    let cap = check_resp
        .headers()
        .get("x-botwork-cap")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();

    let fetch = reqwest::Client::new()
        .post(format!("{}/secrets/fetch", srv.base))
        .header("x-botwork-cap", &cap)
        .send()
        .await
        .unwrap();
    assert_eq!(fetch.status(), StatusCode::OK);
    let fbody: serde_json::Value = fetch.json().await.unwrap();
    let secrets = fbody["secrets"].as_array().unwrap();
    assert_eq!(secrets.len(), 0, "secret should be absent after delete");
}

/// 4. Returns 503 when tenant has no active lease.
#[tokio::test]
async fn no_active_lease_returns_503() {
    if !docker_available().await {
        eprintln!(
            "IGNORED: docker not reachable, skipping remote_secrets::no_active_lease_returns_503"
        );
        return;
    }
    let srv = match spawn().await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("IGNORED: failed to spawn fixture ({err}); skipping");
            return;
        }
    };

    seed_tenant(&srv.db, "phlax").await;
    let resp = remote_store(
        &srv.api_base,
        "phlax",
        "github.com",
        "pat",
        b"secret",
        false,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "no_active_lease");
}

/// 5. Delete of missing secret returns 404.
#[tokio::test]
async fn delete_missing_returns_404() {
    if !docker_available().await {
        eprintln!(
            "IGNORED: docker not reachable, skipping remote_secrets::delete_missing_returns_404"
        );
        return;
    }
    let srv = match spawn().await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("IGNORED: failed to spawn fixture ({err}); skipping");
            return;
        }
    };

    seed_tenant(&srv.db, "phlax").await;
    register(&srv.base, "phlax", "phlax", b"hunter2").await;
    let bearer = login(&srv.base, "phlax", "phlax", b"hunter2").await;

    let wek_resp = reqwest::Client::new()
        .get(format!("{}/auth/lease/wrapped-export-key", srv.base))
        .header("authorization", format!("Bearer {bearer}"))
        .send()
        .await
        .unwrap();
    let wek_body: serde_json::Value = wek_resp.json().await.unwrap();
    let wrapped = URL_SAFE_NO_PAD
        .decode(wek_body["wrapped_export_key"].as_str().unwrap())
        .unwrap();
    let suite = wek_body["suite_version"].as_u64().unwrap() as u8;
    Vault::create(srv.vault_root.join("phlax"), &wrapped, suite).unwrap();

    let resp = remote_delete(&srv.api_base, "phlax", "nonexistent", "nope").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "not_found");
}

/// 6. Concurrent writes for the same tenant serialise correctly.
#[tokio::test]
async fn concurrent_writes_serialise() {
    if !docker_available().await {
        eprintln!(
            "IGNORED: docker not reachable, skipping remote_secrets::concurrent_writes_serialise"
        );
        return;
    }
    let srv = match spawn().await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("IGNORED: failed to spawn fixture ({err}); skipping");
            return;
        }
    };

    seed_tenant(&srv.db, "phlax").await;
    register(&srv.base, "phlax", "phlax", b"hunter2").await;
    let bearer = login(&srv.base, "phlax", "phlax", b"hunter2").await;

    let wek_resp = reqwest::Client::new()
        .get(format!("{}/auth/lease/wrapped-export-key", srv.base))
        .header("authorization", format!("Bearer {bearer}"))
        .send()
        .await
        .unwrap();
    let wek_body: serde_json::Value = wek_resp.json().await.unwrap();
    let wrapped = URL_SAFE_NO_PAD
        .decode(wek_body["wrapped_export_key"].as_str().unwrap())
        .unwrap();
    let suite = wek_body["suite_version"].as_u64().unwrap() as u8;
    Vault::create(srv.vault_root.join("phlax"), &wrapped, suite).unwrap();

    // Fire two concurrent puts for different keys — both should succeed.
    let base1 = srv.api_base.clone();
    let base2 = srv.api_base.clone();

    let (r1, r2) = tokio::join!(
        tokio::spawn(async move {
            remote_store(&base1, "phlax", "svc-a", "key1", b"val1", false).await
        }),
        tokio::spawn(async move {
            remote_store(&base2, "phlax", "svc-b", "key2", b"val2", false).await
        }),
    );
    let r1 = r1.unwrap();
    let r2 = r2.unwrap();
    assert_eq!(
        r1.status(),
        StatusCode::OK,
        "r1 failed: {}",
        r1.text().await.unwrap()
    );
    assert_eq!(
        r2.status(),
        StatusCode::OK,
        "r2 failed: {}",
        r2.text().await.unwrap()
    );

    // Verify both secrets are in the vault.
    let vault_root = srv.vault_root.join("phlax");
    let mut vault = Vault::new(&vault_root);
    vault.unlock(&wrapped, suite).unwrap();
    let secrets = vault.list_secrets().unwrap();
    assert_eq!(secrets.len(), 2, "expected both secrets to be stored");
}

#[tokio::test]
async fn api_auth_login_whoami_and_cookie_check_round_trip() {
    if !docker_available().await {
        eprintln!(
            "IGNORED: docker not reachable, skipping remote_secrets::api_auth_login_whoami_and_cookie_check_round_trip"
        );
        return;
    }
    let srv = match spawn().await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("IGNORED: failed to spawn fixture ({err}); skipping");
            return;
        }
    };

    seed_tenant(&srv.db, "phlax").await;
    register(&srv.base, "phlax", "phlax", b"hunter2").await;
    let session = api_login(&srv.base, "phlax", "phlax", b"hunter2").await;

    let whoami = reqwest::Client::new()
        .get(format!("{}/api/auth/whoami", srv.base))
        .header("cookie", &session.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(whoami.status(), StatusCode::OK);
    let whoami_body: serde_json::Value = whoami.json().await.unwrap();
    assert_eq!(whoami_body["tenant"], "phlax");
    assert_eq!(whoami_body["lease_id"], session.lease_id.to_string());
    assert!(whoami_body["expires_at"].is_string());
    assert!(whoami_body["idle_extends_to"].is_string());

    let check = auth_check_with_cookie(&srv.base, &session.cookie, "/phlax/mcp/exec-bash").await;
    assert_eq!(check.status(), StatusCode::OK);
    assert_eq!(
        check
            .headers()
            .get("x-botwork-tenant")
            .and_then(|v| v.to_str().ok()),
        Some("phlax")
    );
    assert!(check.headers().get("x-botwork-cap").is_some());
}

#[tokio::test]
async fn api_auth_logout_revokes_lease_and_evicts_caps() {
    if !docker_available().await {
        eprintln!(
            "IGNORED: docker not reachable, skipping remote_secrets::api_auth_logout_revokes_lease_and_evicts_caps"
        );
        return;
    }
    let srv = match spawn().await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("IGNORED: failed to spawn fixture ({err}); skipping");
            return;
        }
    };

    seed_tenant(&srv.db, "phlax").await;
    register(&srv.base, "phlax", "phlax", b"hunter2").await;
    let session = api_login(&srv.base, "phlax", "phlax", b"hunter2").await;
    init_vault(&srv.base, &srv.vault_root.join("phlax"), &session.bearer).await;

    let stored = remote_store(
        &srv.api_base,
        "phlax",
        "github.com",
        "pat",
        b"SECRET_TOKEN",
        false,
    )
    .await;
    assert_eq!(stored.status(), StatusCode::OK);

    let check = auth_check_with_cookie(&srv.base, &session.cookie, "/phlax/mcp/exec-bash").await;
    assert_eq!(check.status(), StatusCode::OK);
    let cap = check
        .headers()
        .get("x-botwork-cap")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();

    let fetch = reqwest::Client::new()
        .post(format!("{}/secrets/fetch", srv.base))
        .header("x-botwork-cap", &cap)
        .send()
        .await
        .unwrap();
    assert_eq!(fetch.status(), StatusCode::OK);

    let logout = reqwest::Client::new()
        .post(format!("{}/api/auth/logout", srv.base))
        .header("cookie", &session.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    assert!(logout
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .unwrap()
        .contains("Max-Age=0"));

    let whoami = reqwest::Client::new()
        .get(format!("{}/api/auth/whoami", srv.base))
        .header("cookie", &session.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(whoami.status(), StatusCode::UNAUTHORIZED);

    let fetch_after_logout = reqwest::Client::new()
        .post(format!("{}/secrets/fetch", srv.base))
        .header("x-botwork-cap", &cap)
        .send()
        .await
        .unwrap();
    assert_eq!(fetch_after_logout.status(), StatusCode::UNAUTHORIZED);

    let revoked = lease::Entity::find()
        .filter(lease::Column::Id.eq(session.lease_id))
        .one(&*srv.db)
        .await
        .unwrap()
        .unwrap();
    assert!(revoked.revoked_at.is_some());
}

#[tokio::test]
async fn api_auth_logout_is_idempotent() {
    // POST /api/auth/logout → logout → logout again must return 204 + cookie-clear both times.
    // A second POST /api/auth/logout call (with an already-revoked lease) must NOT return 401.
    if !docker_available().await {
        eprintln!(
            "IGNORED: docker not reachable, skipping remote_secrets::api_auth_logout_is_idempotent"
        );
        return;
    }
    let srv = match spawn().await {
        Ok(s) => s,
        Err(err) => {
            eprintln!("IGNORED: failed to spawn fixture ({err}); skipping");
            return;
        }
    };

    seed_tenant(&srv.db, "phlax").await;
    register(&srv.base, "phlax", "phlax", b"hunter2").await;
    let session = api_login(&srv.base, "phlax", "phlax", b"hunter2").await;

    let http = reqwest::Client::new();

    // First logout → NO_CONTENT (204) + cookie-clear.
    let logout1 = http
        .post(format!("{}/api/auth/logout", srv.base))
        .header("cookie", &session.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(
        logout1.status(),
        StatusCode::NO_CONTENT,
        "first logout should return NO_CONTENT (204)"
    );
    assert!(
        logout1
            .headers()
            .get("set-cookie")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .contains("Max-Age=0"),
        "first logout should clear the cookie"
    );

    // Second logout (lease already revoked) → NO_CONTENT (204) + cookie-clear, not UNAUTHORIZED (401).
    let logout2 = http
        .post(format!("{}/api/auth/logout", srv.base))
        .header("cookie", &session.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(
        logout2.status(),
        StatusCode::NO_CONTENT,
        "second logout (already-revoked lease) should still return NO_CONTENT (204), not UNAUTHORIZED (401)"
    );
    assert!(
        logout2
            .headers()
            .get("set-cookie")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .contains("Max-Age=0"),
        "second logout should still clear the cookie"
    );
}
