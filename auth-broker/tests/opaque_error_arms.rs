//! Offline coverage for the OPAQUE endpoint error and validation arms in
//! `auth/endpoints.rs`.
//!
//! Gate: `required-features = ["test-support"]`

use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use botwork_auth_broker::auth::pending::Pending;
use botwork_auth_broker::auth::{build_auth_router, AuthState, RateLimitConfig, PENDING_TTL};
use botwork_auth_broker::store::mock::{MockLeaseStore, MockPasswordFileStore, MockTenantStore};
use botwork_entity::{opaque_password_file, tenant};
use botwork_opaque_handshake::{
    client, server, ClientLoginState, LoginResponse, PasswordFile, ServerSetup,
};
use chrono::Utc;
use rand::Rng;
use sea_orm::{DatabaseBackend, DbErr, MockDatabase, MockExecResult};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::{advance, pause};
use tower::ServiceExt;
use uuid::Uuid;

fn random_password() -> Vec<u8> {
    let mut password = vec![0u8; 32];
    rand::rng().fill_bytes(&mut password);
    password
}

fn make_password_file(setup: &ServerSetup, password: &[u8]) -> PasswordFile {
    let mut rng = rand::rng();
    let started =
        client::registration_start(&mut rng, password).expect("client registration_start");
    let response = server::registration_start(setup, started.request, b"alice")
        .expect("server registration_start");
    let finished =
        client::registration_finish(&mut rng, started.state, password, response.response)
            .expect("client registration_finish");
    server::registration_finish(finished.upload)
}

fn make_registration_request_b64() -> String {
    let mut rng = rand::rng();
    let password = random_password();
    let started =
        client::registration_start(&mut rng, &password).expect("client registration_start");
    URL_SAFE_NO_PAD.encode(started.request.serialize())
}

fn make_registration_upload_b64(setup: &ServerSetup, password: &[u8]) -> String {
    let mut rng = rand::rng();
    let started =
        client::registration_start(&mut rng, password).expect("client registration_start");
    let response = server::registration_start(setup, started.request, b"alice")
        .expect("server registration_start");
    let finished =
        client::registration_finish(&mut rng, started.state, password, response.response)
            .expect("client registration_finish");
    URL_SAFE_NO_PAD.encode(finished.upload.serialize())
}

fn tenant_model(id: Uuid, name: &str) -> tenant::Model {
    let now = Utc::now();
    tenant::Model {
        id,
        name: name.to_string(),
        created_at: now,
        updated_at: now,
    }
}

fn password_file_model(
    tenant_id: Uuid,
    password_file: &PasswordFile,
) -> opaque_password_file::Model {
    let now = Utc::now();
    opaque_password_file::Model {
        id: Uuid::new_v4(),
        tenant_id,
        password_file: password_file.as_bytes().to_vec(),
        suite_version: 1,
        created_at: now,
        updated_at: now,
    }
}

fn mock_auth_with_known_tenant(setup: ServerSetup, tenant_id: Uuid) -> AuthState {
    AuthState::from_stores(
        Arc::new(MockLeaseStore::new()),
        Arc::new(MockTenantStore::with_tenant("acme", tenant_id)),
        Arc::new(MockPasswordFileStore::new()),
        setup,
    )
}

async fn send_json(
    app: &axum::Router,
    uri: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> axum::http::Response<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    app.clone()
        .oneshot(builder.body(Body::from(body.to_string())).expect("request"))
        .await
        .expect("response")
}

async fn response_json(response: axum::http::Response<Body>) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&body).expect("json body")
}

fn assert_json_error(body: &Value, code: &str, expected_message: &str) {
    assert_eq!(body["error"]["code"], code);
    assert_eq!(body["error"]["message"], expected_message);
}

fn assert_json_error_starts_with(body: &Value, code: &str, prefix: &str) {
    assert_eq!(body["error"]["code"], code);
    let message = body["error"]["message"].as_str().expect("string message");
    assert!(
        message.starts_with(prefix),
        "expected message prefix `{prefix}`, got `{message}`"
    );
}

#[derive(Debug, Deserialize)]
struct LoginStartResponse {
    handshake_id: Uuid,
    login_response: String,
}

async fn start_login(
    app: &axum::Router,
    tenant: &str,
    password: &[u8],
) -> (ClientLoginState, LoginStartResponse) {
    let mut rng = rand::rng();
    let started = client::login_start(&mut rng, password).expect("client login_start");
    let response = send_json(
        app,
        "/auth/login/start",
        &[],
        json!({
            "tenant": tenant,
            "credential_identifier": "alice",
            "login_request": URL_SAFE_NO_PAD.encode(started.request.serialize()),
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        started.state,
        serde_json::from_slice(&body).expect("login start response"),
    )
}

fn make_login_finalization_b64(
    state: ClientLoginState,
    password: &[u8],
    login_response_b64: &str,
) -> String {
    let response_bytes = URL_SAFE_NO_PAD
        .decode(login_response_b64)
        .expect("login_response base64");
    let response = LoginResponse::deserialize(&response_bytes).expect("login_response deserialize");
    let finished = client::login_finish(state, password, response).expect("client login_finish");
    URL_SAFE_NO_PAD.encode(finished.finalization.serialize())
}

#[tokio::test]
async fn register_start_rate_limit_returns_429_with_retry_after() {
    let mut rng = rand::rng();
    let auth = AuthState::from_stores(
        Arc::new(MockLeaseStore::new()),
        Arc::new(MockTenantStore::new()),
        Arc::new(MockPasswordFileStore::new()),
        ServerSetup::generate(&mut rng),
    )
    .with_rate_limiter(RateLimitConfig {
        rate_per_second: 1,
        burst: 2,
        disabled: false,
    });
    let app = build_auth_router(auth);
    let body = json!({
        "tenant": "acme",
        "credential_identifier": "alice",
        "registration_request": "ignored-by-rate-limit",
    });

    for _ in 0..2 {
        let _ = send_json(&app, "/auth/register/start", &[], body.clone()).await;
    }

    let response = send_json(&app, "/auth/register/start", &[], body).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after = response
        .headers()
        .get("retry-after")
        .expect("retry-after")
        .to_str()
        .expect("ascii")
        .parse::<u64>()
        .expect("integer");
    assert!(retry_after >= 1);
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "rate_limited");
}

#[tokio::test]
async fn register_start_unknown_tenant_returns_404() {
    let mut rng = rand::rng();
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![Vec::<tenant::Model>::new()])
        .into_connection();
    let app = build_auth_router(AuthState::new(db, ServerSetup::generate(&mut rng)));

    let response = send_json(
        &app,
        "/auth/register/start",
        &[],
        json!({
            "tenant": "acme",
            "credential_identifier": "alice",
            "registration_request": make_registration_request_b64(),
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = response_json(response).await;
    assert_json_error(&body, "not_found", "unknown tenant 'acme'");
}

#[tokio::test]
async fn register_start_tenant_store_db_error_returns_500() {
    let mut rng = rand::rng();
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_errors(vec![DbErr::Custom("tenant lookup failed".to_string())])
        .into_connection();
    let app = build_auth_router(AuthState::new(db, ServerSetup::generate(&mut rng)));

    let response = send_json(
        &app,
        "/auth/register/start",
        &[],
        json!({
            "tenant": "acme",
            "credential_identifier": "alice",
            "registration_request": make_registration_request_b64(),
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = response_json(response).await;
    assert_json_error_starts_with(&body, "internal", "database error:");
    assert!(body["error"]["message"]
        .as_str()
        .expect("message")
        .contains("tenant lookup failed"));
}

#[tokio::test]
async fn register_start_bad_base64_returns_400() {
    let mut rng = rand::rng();
    let app = build_auth_router(mock_auth_with_known_tenant(
        ServerSetup::generate(&mut rng),
        Uuid::new_v4(),
    ));

    let response = send_json(
        &app,
        "/auth/register/start",
        &[],
        json!({
            "tenant": "acme",
            "credential_identifier": "alice",
            "registration_request": "%%%not-base64%%%",
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_json_error(
        &body,
        "bad_request",
        "`registration_request` is not valid url-safe-base64",
    );
}

#[tokio::test]
async fn register_start_malformed_request_returns_400() {
    let mut rng = rand::rng();
    let app = build_auth_router(mock_auth_with_known_tenant(
        ServerSetup::generate(&mut rng),
        Uuid::new_v4(),
    ));

    let response = send_json(
        &app,
        "/auth/register/start",
        &[],
        json!({
            "tenant": "acme",
            "credential_identifier": "alice",
            "registration_request": "",
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_json_error_starts_with(&body, "bad_request", "malformed `registration_request`:");
}

#[tokio::test]
async fn register_finish_rate_limit_returns_429_with_retry_after() {
    let mut rng = rand::rng();
    let auth = AuthState::from_stores(
        Arc::new(MockLeaseStore::new()),
        Arc::new(MockTenantStore::new()),
        Arc::new(MockPasswordFileStore::new()),
        ServerSetup::generate(&mut rng),
    )
    .with_rate_limiter(RateLimitConfig {
        rate_per_second: 1,
        burst: 2,
        disabled: false,
    });
    let app = build_auth_router(auth);
    let body = json!({
        "tenant": "acme",
        "registration_upload": "ignored-by-rate-limit",
    });

    for _ in 0..2 {
        let _ = send_json(&app, "/auth/register/finish", &[], body.clone()).await;
    }

    let response = send_json(&app, "/auth/register/finish", &[], body).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().contains_key("retry-after"));
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "rate_limited");
}

#[tokio::test]
async fn register_finish_unknown_tenant_returns_404() {
    let mut rng = rand::rng();
    let password = random_password();
    let setup = ServerSetup::generate(&mut rng);
    let upload = make_registration_upload_b64(&setup, &password);
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![Vec::<tenant::Model>::new()])
        .into_connection();
    let app = build_auth_router(AuthState::new(db, setup));

    let response = send_json(
        &app,
        "/auth/register/finish",
        &[],
        json!({
            "tenant": "acme",
            "registration_upload": upload,
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = response_json(response).await;
    assert_json_error(&body, "not_found", "unknown tenant 'acme'");
}

#[tokio::test]
async fn register_finish_tenant_store_db_error_returns_500() {
    let mut rng = rand::rng();
    let password = random_password();
    let setup = ServerSetup::generate(&mut rng);
    let upload = make_registration_upload_b64(&setup, &password);
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_errors(vec![DbErr::Custom("tenant read failed".to_string())])
        .into_connection();
    let app = build_auth_router(AuthState::new(db, setup));

    let response = send_json(
        &app,
        "/auth/register/finish",
        &[],
        json!({
            "tenant": "acme",
            "registration_upload": upload,
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = response_json(response).await;
    assert_json_error_starts_with(&body, "internal", "database error:");
    assert!(body["error"]["message"]
        .as_str()
        .expect("message")
        .contains("tenant read failed"));
}

#[tokio::test]
async fn register_finish_bad_base64_returns_400() {
    let mut rng = rand::rng();
    let app = build_auth_router(mock_auth_with_known_tenant(
        ServerSetup::generate(&mut rng),
        Uuid::new_v4(),
    ));

    let response = send_json(
        &app,
        "/auth/register/finish",
        &[],
        json!({
            "tenant": "acme",
            "registration_upload": "%%%not-base64%%%",
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_json_error(
        &body,
        "bad_request",
        "`registration_upload` is not valid url-safe-base64",
    );
}

#[tokio::test]
async fn register_finish_conflict_returns_409() {
    let mut rng = rand::rng();
    let tenant_id = Uuid::new_v4();
    let setup = ServerSetup::generate(&mut rng);
    let upload_password = random_password();
    let upload = make_registration_upload_b64(&setup, &upload_password);
    let stored_password = random_password();
    let stored = make_password_file(&setup, &stored_password);
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![tenant_model(tenant_id, "acme")]])
        .append_exec_results(vec![MockExecResult {
            last_insert_id: 0,
            rows_affected: 0,
        }])
        .append_query_results(vec![vec![password_file_model(tenant_id, &stored)]])
        .into_connection();
    let app = build_auth_router(AuthState::new(db, setup));

    let response = send_json(
        &app,
        "/auth/register/finish",
        &[],
        json!({
            "tenant": "acme",
            "registration_upload": upload,
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = response_json(response).await;
    assert_json_error(
        &body,
        "conflict",
        "a password file already exists for tenant 'acme'",
    );
}

#[tokio::test]
async fn register_finish_db_error_returns_500() {
    let mut rng = rand::rng();
    let tenant_id = Uuid::new_v4();
    let password = random_password();
    let setup = ServerSetup::generate(&mut rng);
    let upload = make_registration_upload_b64(&setup, &password);
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![tenant_model(tenant_id, "acme")]])
        .append_exec_errors(vec![DbErr::Custom("write failed".to_string())])
        .into_connection();
    let app = build_auth_router(AuthState::new(db, setup));

    let response = send_json(
        &app,
        "/auth/register/finish",
        &[],
        json!({
            "tenant": "acme",
            "registration_upload": upload,
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = response_json(response).await;
    assert_json_error_starts_with(&body, "internal", "database error:");
    assert!(body["error"]["message"]
        .as_str()
        .expect("message")
        .contains("write failed"));
}

#[tokio::test]
async fn login_start_bad_base64_returns_400() {
    let mut rng = rand::rng();
    let app = build_auth_router(AuthState::from_stores(
        Arc::new(MockLeaseStore::new()),
        Arc::new(MockTenantStore::new()),
        Arc::new(MockPasswordFileStore::new()),
        ServerSetup::generate(&mut rng),
    ));

    let response = send_json(
        &app,
        "/auth/login/start",
        &[],
        json!({
            "tenant": "acme",
            "credential_identifier": "alice",
            "login_request": "%%%not-base64%%%",
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_json_error(
        &body,
        "bad_request",
        "`login_request` is not valid url-safe-base64",
    );
}

#[tokio::test]
async fn login_start_malformed_request_returns_400() {
    let mut rng = rand::rng();
    let app = build_auth_router(AuthState::from_stores(
        Arc::new(MockLeaseStore::new()),
        Arc::new(MockTenantStore::new()),
        Arc::new(MockPasswordFileStore::new()),
        ServerSetup::generate(&mut rng),
    ));

    let response = send_json(
        &app,
        "/auth/login/start",
        &[],
        json!({
            "tenant": "acme",
            "credential_identifier": "alice",
            "login_request": "",
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_json_error_starts_with(&body, "bad_request", "malformed `login_request`:");
}

#[tokio::test]
async fn login_start_tenant_store_db_error_returns_500() {
    let mut rng = rand::rng();
    let password = random_password();
    let started = client::login_start(&mut rng, &password).expect("client login_start");
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_errors(vec![DbErr::Custom("tenant lookup failed".to_string())])
        .into_connection();
    let app = build_auth_router(AuthState::new(db, ServerSetup::generate(&mut rng)));

    let response = send_json(
        &app,
        "/auth/login/start",
        &[],
        json!({
            "tenant": "acme",
            "credential_identifier": "alice",
            "login_request": URL_SAFE_NO_PAD.encode(started.request.serialize()),
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = response_json(response).await;
    assert_json_error_starts_with(&body, "internal", "database error:");
    assert!(body["error"]["message"]
        .as_str()
        .expect("message")
        .contains("tenant lookup failed"));
}

#[tokio::test]
async fn login_start_password_file_store_db_error_returns_500() {
    let mut rng = rand::rng();
    let tenant_id = Uuid::new_v4();
    let password = random_password();
    let started = client::login_start(&mut rng, &password).expect("client login_start");
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![tenant_model(tenant_id, "acme")]])
        .append_query_errors(vec![DbErr::Custom("password file read failed".to_string())])
        .into_connection();
    let app = build_auth_router(AuthState::new(db, ServerSetup::generate(&mut rng)));

    let response = send_json(
        &app,
        "/auth/login/start",
        &[],
        json!({
            "tenant": "acme",
            "credential_identifier": "alice",
            "login_request": URL_SAFE_NO_PAD.encode(started.request.serialize()),
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = response_json(response).await;
    assert_json_error_starts_with(&body, "internal", "database error:");
    assert!(body["error"]["message"]
        .as_str()
        .expect("message")
        .contains("password file read failed"));
}

#[tokio::test]
async fn login_finish_expired_pending_returns_404() {
    // Enable manual time control so we can advance the clock to trigger expiry.
    pause();

    let mut rng = rand::rng();
    let tenant_id = Uuid::new_v4();
    let password = random_password();
    let setup = ServerSetup::generate(&mut rng);
    let password_file = make_password_file(&setup, &password);
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![tenant_model(tenant_id, "acme")]])
        .append_query_results(vec![vec![password_file_model(tenant_id, &password_file)]])
        .into_connection();
    let app = build_auth_router(AuthState::new(db, setup));

    let (client_state, started) = start_login(&app, "acme", &password).await;
    advance(PENDING_TTL + Duration::from_secs(1)).await;
    let finalization =
        make_login_finalization_b64(client_state, &password, &started.login_response);
    let response = send_json(
        &app,
        "/auth/login/finish",
        &[],
        json!({
            "handshake_id": started.handshake_id,
            "login_finalization": finalization,
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = response_json(response).await;
    assert_json_error(&body, "not_found", "unknown or expired handshake_id");
}

#[tokio::test]
async fn login_finish_bad_base64_returns_400() {
    let mut rng = rand::rng();
    let tenant_id = Uuid::new_v4();
    let password = random_password();
    let setup = ServerSetup::generate(&mut rng);
    let password_file = make_password_file(&setup, &password);
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![tenant_model(tenant_id, "acme")]])
        .append_query_results(vec![vec![password_file_model(tenant_id, &password_file)]])
        .into_connection();
    let app = build_auth_router(AuthState::new(db, setup));
    let (_client_state, started) = start_login(&app, "acme", &password).await;

    let response = send_json(
        &app,
        "/auth/login/finish",
        &[],
        json!({
            "handshake_id": started.handshake_id,
            "login_finalization": "%%%not-base64%%%",
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_json_error(
        &body,
        "bad_request",
        "`login_finalization` is not valid url-safe-base64",
    );
}

#[tokio::test]
async fn login_finish_malformed_finalization_returns_400() {
    let mut rng = rand::rng();
    let tenant_id = Uuid::new_v4();
    let password = random_password();
    let setup = ServerSetup::generate(&mut rng);
    let password_file = make_password_file(&setup, &password);
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![tenant_model(tenant_id, "acme")]])
        .append_query_results(vec![vec![password_file_model(tenant_id, &password_file)]])
        .into_connection();
    let app = build_auth_router(AuthState::new(db, setup));
    let (_client_state, started) = start_login(&app, "acme", &password).await;

    let response = send_json(
        &app,
        "/auth/login/finish",
        &[],
        json!({
            "handshake_id": started.handshake_id,
            "login_finalization": "",
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_json_error_starts_with(&body, "bad_request", "malformed `login_finalization`:");
}

#[tokio::test]
async fn login_finish_dummy_flow_returns_invalid_bearer() {
    let mut rng = rand::rng();
    let password = random_password();
    let setup = ServerSetup::generate(&mut rng);
    let password_file = make_password_file(&setup, &password);
    let client_started = client::login_start(&mut rng, &password).expect("client login_start");
    let server_started = server::login_start(
        &mut rng,
        &setup,
        Some(&password_file),
        client_started.request,
        b"alice",
    )
    .expect("server login_start");
    let finalization = URL_SAFE_NO_PAD.encode(
        client::login_finish(client_started.state, &password, server_started.response)
            .expect("client login_finish")
            .finalization
            .serialize(),
    );
    let auth = AuthState::new(
        MockDatabase::new(DatabaseBackend::Postgres).into_connection(),
        setup,
    );
    let handshake_id = Uuid::new_v4();
    auth.pending
        .insert(
            handshake_id,
            Pending {
                tenant_id: None,
                state: server_started.state,
                lease_seconds_requested: 60,
                created_at: tokio::time::Instant::now(),
            },
        )
        .await
        .expect("insert pending");
    let app = build_auth_router(auth);

    let response = send_json(
        &app,
        "/auth/login/finish",
        &[],
        json!({
            "handshake_id": handshake_id,
            "login_finalization": finalization,
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let header = response
        .headers()
        .get("www-authenticate")
        .expect("www-authenticate")
        .to_str()
        .expect("ascii");
    assert!(header.contains("error=\"invalid_bearer\""));
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "invalid_bearer");
    assert_eq!(
        body["error"]["message"],
        "invalid Authorization bearer; run `bw`."
    );
}

#[tokio::test]
async fn login_finish_insert_lease_error_returns_500() {
    let mut rng = rand::rng();
    let tenant_id = Uuid::new_v4();
    let password = random_password();
    let setup = ServerSetup::generate(&mut rng);
    let password_file = make_password_file(&setup, &password);
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![tenant_model(tenant_id, "acme")]])
        .append_query_results(vec![vec![password_file_model(tenant_id, &password_file)]])
        .append_query_errors(vec![DbErr::Custom("lease insert failed".to_string())])
        .into_connection();
    let app = build_auth_router(AuthState::new(db, setup));
    let (client_state, started) = start_login(&app, "acme", &password).await;
    let finalization =
        make_login_finalization_b64(client_state, &password, &started.login_response);

    let response = send_json(
        &app,
        "/auth/login/finish",
        &[],
        json!({
            "handshake_id": started.handshake_id,
            "login_finalization": finalization,
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = response_json(response).await;
    assert_json_error_starts_with(&body, "internal", "database error:");
    assert!(body["error"]["message"]
        .as_str()
        .expect("message")
        .contains("lease insert failed"));
}

// ===========================================================================
// register_start success path (lines 425, 435–452)
// ===========================================================================

/// Valid `registration_request` with a known tenant: exercises the
/// `Ok(r) => r` arm (line 425), `server::registration_start` success
/// (lines 435–440), and the `info! + Json(...)` success response
/// (lines 447–452).
#[tokio::test]
async fn register_start_success_returns_200() {
    let mut rng = rand::rng();
    let tenant_id = Uuid::new_v4();
    let setup = ServerSetup::generate(&mut rng);
    let app = build_auth_router(mock_auth_with_known_tenant(setup, tenant_id));

    let response = send_json(
        &app,
        "/auth/register/start",
        &[],
        json!({
            "tenant": "acme",
            "credential_identifier": "alice",
            "registration_request": make_registration_request_b64(),
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert!(
        body["registration_response"].is_string(),
        "expected registration_response, got {body}"
    );
}

// ===========================================================================
// register_finish malformed bytes path (lines 514–515)
// ===========================================================================

/// Valid base64 string that decodes to bytes which are NOT a valid
/// `RegistrationUpload`: `RegistrationUpload::deserialize` returns
/// `OpaqueError::Serialization` → bad_request (lines 514–515).
#[tokio::test]
async fn register_finish_malformed_upload_bytes_returns_400() {
    let mut rng = rand::rng();
    let tenant_id = Uuid::new_v4();
    let setup = ServerSetup::generate(&mut rng);
    let app = build_auth_router(mock_auth_with_known_tenant(setup, tenant_id));

    // Empty string is valid url-safe-base64 (decodes to []) but is NOT
    // a valid serialized RegistrationUpload → Serialization error.
    let response = send_json(
        &app,
        "/auth/register/finish",
        &[],
        json!({
            "tenant": "acme",
            "registration_upload": "",
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_json_error_starts_with(&body, "bad_request", "malformed `registration_upload`:");
}

// ===========================================================================
// register_finish success path (lines 531–542)
// ===========================================================================

/// Full register_finish: `MockPasswordFileStore` is empty so
/// `upsert_password_file` returns `Ok(())`, reaching the `info!` +
/// `StatusCode::CREATED` + `Json(RegisterFinishResponse {...})` arm
/// (lines 531–542).
#[tokio::test]
async fn register_finish_success_returns_201() {
    let mut rng = rand::rng();
    let tenant_id = Uuid::new_v4();
    let setup = ServerSetup::generate(&mut rng);
    let password = random_password();
    let upload_b64 = make_registration_upload_b64(&setup, &password);
    let app = build_auth_router(mock_auth_with_known_tenant(setup, tenant_id));

    let response = send_json(
        &app,
        "/auth/register/finish",
        &[],
        json!({
            "tenant": "acme",
            "registration_upload": upload_b64,
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = response_json(response).await;
    assert_eq!(body["tenant"], "acme");
    assert!(body["suite_version"].is_number(), "got {body}");
}

// ===========================================================================
// login_finish InvalidLogin path (lines 791–799)
// ===========================================================================

/// Cross-session finalization mismatch: start session A (real server
/// state stored in the pending map), then finish with session B's
/// finalization bytes.  `server::login_finish(state_A, finalization_B)`
/// returns `OpaqueError::InvalidLogin` → 401 (lines 791–799).
#[tokio::test]
async fn login_finish_invalid_login_returns_401() {
    let mut rng = rand::rng();
    let tenant_id = Uuid::new_v4();
    let password = random_password();
    let setup = ServerSetup::generate(&mut rng);
    let password_file = make_password_file(&setup, &password);

    // DB for session A's login_start (reads tenant + password_file).
    let db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![tenant_model(tenant_id, "acme")]])
        .append_query_results(vec![vec![password_file_model(tenant_id, &password_file)]])
        .into_connection();
    let app = build_auth_router(AuthState::new(db, setup.clone()));

    // Start session A — its ServerLoginState is stored in the pending map.
    let (_client_state_a, started_a) = start_login(&app, "acme", &password).await;

    // Build session B entirely offline (no pending map entry for it).
    let client_b = client::login_start(&mut rng, &password).expect("client B start");
    let server_b = server::login_start(
        &mut rng,
        &setup,
        Some(&password_file),
        client_b.request,
        b"alice",
    )
    .expect("server B start");
    let finished_b = client::login_finish(client_b.state, &password, server_b.response)
        .expect("client B finish");
    let finalization_b = URL_SAFE_NO_PAD.encode(finished_b.finalization.serialize());

    // Submit session B's finalization using session A's handshake_id.
    // server::login_finish(state_A, finalization_B) → OpaqueError::InvalidLogin.
    let response = send_json(
        &app,
        "/auth/login/finish",
        &[],
        json!({
            "handshake_id": started_a.handshake_id,
            "login_finalization": finalization_b,
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let www_auth = response
        .headers()
        .get("www-authenticate")
        .expect("www-authenticate header")
        .to_str()
        .expect("ascii");
    assert!(
        www_auth.contains("error=\"invalid_bearer\""),
        "www-authenticate: {www_auth}"
    );
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "invalid_bearer");
}

// ===========================================================================
// login_finish rate-limit (endpoints.rs line 742)
// ===========================================================================

/// Exhaust the login_finish rate limiter: the first two requests consume the
/// burst allowance; the third request hits the rate-limit arm → 429.
/// Covers `too_many_requests` return at endpoints.rs line 742.
#[tokio::test]
async fn login_finish_rate_limit_returns_429() {
    let mut rng = rand::rng();
    let auth = AuthState::from_stores(
        Arc::new(MockLeaseStore::new()),
        Arc::new(MockTenantStore::new()),
        Arc::new(MockPasswordFileStore::new()),
        ServerSetup::generate(&mut rng),
    )
    .with_rate_limiter(RateLimitConfig {
        rate_per_second: 1,
        burst: 2,
        disabled: false,
    });
    let app = build_auth_router(auth);
    let body = json!({
        "handshake_id": Uuid::new_v4(),
        "login_finalization": "",
    });

    // Consume the burst.
    for _ in 0..2 {
        let _ = send_json(&app, "/auth/login/finish", &[], body.clone()).await;
    }

    // Third request → rate limited.
    let response = send_json(&app, "/auth/login/finish", &[], body).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after = response
        .headers()
        .get("retry-after")
        .expect("retry-after")
        .to_str()
        .expect("ascii")
        .parse::<u64>()
        .expect("integer");
    assert!(retry_after >= 1);
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "rate_limited");
}

// ===========================================================================
// login_finish success path (endpoints.rs line 745)
// ===========================================================================

/// A complete offline OPAQUE login via `MockLeaseStore` + `MockTenantStore` +
/// `MockPasswordFileStore`: `login_finish_inner` returns `Ok` and the
/// wrapper emits `Json(response).into_response()` → endpoints.rs line 745.
#[tokio::test]
async fn login_finish_success_returns_200_with_bearer() {
    let mut rng = rand::rng();
    let tenant_id = Uuid::new_v4();
    let password = random_password();
    let setup = ServerSetup::generate(&mut rng);
    let password_file = make_password_file(&setup, &password);

    let auth = AuthState::from_stores(
        Arc::new(MockLeaseStore::new()),
        Arc::new(MockTenantStore::with_tenant("acme", tenant_id)),
        Arc::new(MockPasswordFileStore::with_file(tenant_id, &password_file)),
        setup,
    );
    let app = build_auth_router(auth);

    let (client_state, started) = start_login(&app, "acme", &password).await;
    let finalization =
        make_login_finalization_b64(client_state, &password, &started.login_response);

    let response = send_json(
        &app,
        "/auth/login/finish",
        &[],
        json!({
            "handshake_id": started.handshake_id,
            "login_finalization": finalization,
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert!(
        body["bearer"].is_string(),
        "expected bearer in response, got: {body}"
    );
    assert!(
        body["expires_at"].is_string(),
        "expected expires_at in response, got: {body}"
    );
}
