//! Offline handler coverage for `handler.rs` lease/error branches.
//!
//! Gate: `required-features = ["test-support"]`

mod common;

use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::body::{to_bytes, Body};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use botwork_auth_broker::auth::{AuthState, Bearer, LeaseRow};
use botwork_auth_broker::handler::BOTWORK_CAP_COOKIE_NAME;
use botwork_auth_broker::store::mock::{
    LeaseOutcome, MockLeaseStore, MockPasswordFileStore, MockTenantStore,
};
use botwork_auth_broker::store::sea_orm_impl::{SeaOrmLeaseStore, SeaOrmTenantStore};
use botwork_auth_broker::store::{LeaseStore, PasswordFileStore, TenantStore};
use botwork_auth_broker::{
    build_app_state, build_router, build_user_api_router, cache_key, wrap_session_key, AppState,
    CapEntry, CAP_TTL,
};
use botwork_entity::{lease, tenant};
use botwork_opaque_handshake::{client, server, LoginResponse, PasswordFile, ServerSetup};
use botwork_vault::Vault;
use chrono::{Duration, Utc};
use sea_orm::{DatabaseBackend, DatabaseConnection, DbErr, MockDatabase, MockExecResult};
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::time::{sleep, Instant};
use tower::ServiceExt;
use uuid::Uuid;

use common::{bearer as bearer_header, offline_auth_state};

const TEST_SESSION_KEY_BYTES: [u8; 64] = [0xAB; 64];

struct Captured {
    status: StatusCode,
    headers: HeaderMap,
    raw_body: String,
    json: Option<Value>,
}

fn build_state(auth: AuthState) -> AppState {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();
    // These handler/error-path tests only need a stable path value on AppState;
    // none of the covered branches rely on the tempdir remaining on disk after
    // setup completes.
    AppState::with_auth(path, auth)
}

fn build_auth_state(
    lease_store: Arc<dyn LeaseStore + Send + Sync>,
    tenant_store: Arc<dyn TenantStore + Send + Sync>,
) -> AuthState {
    let setup = botwork_opaque_handshake::ServerSetup::generate(&mut rand::thread_rng());
    let password_file_store: Arc<dyn PasswordFileStore + Send + Sync> =
        Arc::new(MockPasswordFileStore::new());
    AuthState::from_stores(lease_store, tenant_store, password_file_store, setup)
}

fn build_app_from_dbs(
    lease_db: DatabaseConnection,
    tenant_db: DatabaseConnection,
) -> (AppState, axum::Router) {
    let lease_store: Arc<dyn LeaseStore + Send + Sync> = Arc::new(SeaOrmLeaseStore::new(lease_db));
    let tenant_store: Arc<dyn TenantStore + Send + Sync> =
        Arc::new(SeaOrmTenantStore::new(tenant_db));
    let state = build_state(build_auth_state(lease_store, tenant_store));
    let app = build_router(state.clone());
    (state, app)
}

async fn build_empty_app() -> axum::Router {
    build_router(build_state(offline_auth_state().await))
}

async fn capture(response: axum::http::Response<Body>) -> Captured {
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let raw_body = String::from_utf8(bytes.to_vec()).expect("utf8 body");
    let is_json = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("application/json"))
        .unwrap_or(false);
    let json = if raw_body.is_empty() || !is_json {
        None
    } else {
        Some(serde_json::from_str(&raw_body).expect("json body"))
    };
    Captured {
        status,
        headers,
        raw_body,
        json,
    }
}

fn header<'a>(captured: &'a Captured, name: &str) -> Option<&'a str> {
    captured
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
}

fn assert_structured_error(captured: &Captured, expected_status: StatusCode, expected_code: &str) {
    assert_eq!(captured.status, expected_status);
    assert_eq!(header(captured, "content-type"), Some("application/json"));
    let www_authenticate = header(captured, "www-authenticate").expect("www-authenticate header");
    assert!(
        www_authenticate.contains(&format!("error=\"{expected_code}\"")),
        "expected error code {expected_code}, header={www_authenticate}"
    );
    assert!(
        www_authenticate.contains("error_description=\""),
        "missing error_description, header={www_authenticate}"
    );
    let body = captured.json.as_ref().expect("json body");
    assert_eq!(body["error"]["code"], expected_code);
    assert!(
        body["error"]["message"].is_string(),
        "body={}",
        captured.raw_body
    );
}

fn assert_api_error(captured: &Captured, expected_status: StatusCode, expected_code: &str) {
    assert_eq!(captured.status, expected_status);
    assert_eq!(header(captured, "content-type"), Some("application/json"));
    assert!(
        header(captured, "www-authenticate").is_none(),
        "api error should not emit www-authenticate, body={}",
        captured.raw_body
    );
    let body = captured.json.as_ref().expect("json body");
    assert_eq!(body["error"]["code"], expected_code);
    assert!(
        body["error"]["message"].is_string(),
        "body={}",
        captured.raw_body
    );
}

async fn send_auth_check(
    app: &axum::Router,
    authorization: Option<&str>,
    original_path: &str,
) -> Captured {
    let mut builder = Request::builder().method("POST").uri("/auth/check");
    if let Some(value) = authorization {
        builder = builder.header("authorization", value);
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .header("x-envoy-original-path", original_path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    capture(response).await
}

async fn send_wrapped_export_key(app: &axum::Router, authorization: Option<&str>) -> Captured {
    let mut builder = Request::builder()
        .method("GET")
        .uri("/auth/lease/wrapped-export-key");
    if let Some(value) = authorization {
        builder = builder.header("authorization", value);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    capture(response).await
}

async fn send_whoami(app: &axum::Router, authorization: Option<&str>) -> Captured {
    let mut builder = Request::builder().method("GET").uri("/api/auth/whoami");
    if let Some(value) = authorization {
        builder = builder.header("authorization", value);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    capture(response).await
}

async fn send_logout(
    app: &axum::Router,
    authorization: Option<&str>,
    forwarded_proto: Option<&str>,
) -> Captured {
    let mut builder = Request::builder().method("POST").uri("/api/auth/logout");
    if let Some(value) = authorization {
        builder = builder.header("authorization", value);
    }
    if let Some(value) = forwarded_proto {
        builder = builder.header("x-forwarded-proto", value);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    capture(response).await
}

async fn send_login(app: &axum::Router, body: Value) -> Captured {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    capture(response).await
}

fn encoded_bearer(bearer: &Bearer) -> String {
    URL_SAFE_NO_PAD.encode(bearer.as_bytes())
}

fn live_lease_model(bearer: &Bearer, tenant_id: Uuid) -> lease::Model {
    let now = Utc::now();
    let expires_at = now + Duration::hours(2);
    lease::Model {
        id: Uuid::new_v4(),
        tenant_id,
        bearer_hash: bearer.hash().to_vec(),
        wrapped_export_key: wrap_session_key(bearer.as_bytes(), &TEST_SESSION_KEY_BYTES),
        issued_at: now,
        expires_at,
        idle_extends_to: expires_at,
        revoked_at: None,
    }
}

fn successful_validation_db(bearer: &Bearer, tenant_id: Uuid) -> DatabaseConnection {
    let model = live_lease_model(bearer, tenant_id);
    let updated = lease::Model {
        idle_extends_to: model.idle_extends_to + Duration::hours(1),
        ..model.clone()
    };
    MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![model], vec![updated]])
        .into_connection()
}

fn live_lease_row(tenant_id: Uuid) -> LeaseRow {
    let now = Utc::now();
    let expires_at = now + Duration::hours(2);
    LeaseRow {
        id: Uuid::new_v4(),
        tenant_id,
        issued_at: now,
        expires_at,
        idle_extends_to: expires_at,
        revoked_at: None,
    }
}

fn build_mock_check_app(
    vault_root: std::path::PathBuf,
    tenant_name: &str,
    tenant_id: Uuid,
    lease_store: Arc<MockLeaseStore>,
) -> (AppState, axum::Router) {
    let lease_store_dyn: Arc<dyn LeaseStore + Send + Sync> = lease_store;
    let tenant_store_dyn: Arc<dyn TenantStore + Send + Sync> =
        Arc::new(MockTenantStore::with_tenant(tenant_name, tenant_id));
    let state = AppState::with_auth(
        vault_root,
        build_auth_state(lease_store_dyn, tenant_store_dyn),
    );
    let app = build_router(state.clone());
    (state, app)
}

fn expired_validation_db(bearer: &Bearer, tenant_id: Uuid) -> DatabaseConnection {
    let mut model = live_lease_model(bearer, tenant_id);
    model.expires_at = Utc::now() - Duration::hours(1);
    MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![model]])
        .into_connection()
}

fn revoked_validation_db(bearer: &Bearer, tenant_id: Uuid) -> DatabaseConnection {
    let mut model = live_lease_model(bearer, tenant_id);
    model.revoked_at = Some(Utc::now());
    MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![model]])
        .into_connection()
}

fn missing_validation_db() -> DatabaseConnection {
    MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![Vec::<lease::Model>::new()])
        .into_connection()
}

fn validation_db_error(message: &str) -> DatabaseConnection {
    MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_errors(vec![DbErr::Custom(message.to_string())])
        .into_connection()
}

fn successful_tenant_db(tenant_id: Uuid, name: &str) -> DatabaseConnection {
    let now = Utc::now();
    let model = tenant::Model {
        id: tenant_id,
        name: name.to_string(),
        created_at: now,
        updated_at: now,
    };
    MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![model]])
        .into_connection()
}

fn missing_tenant_db() -> DatabaseConnection {
    MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![Vec::<tenant::Model>::new()])
        .into_connection()
}

fn tenant_db_error(message: &str) -> DatabaseConnection {
    MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_errors(vec![DbErr::Custom(message.to_string())])
        .into_connection()
}

#[tokio::test]
async fn wrapped_export_key_missing_bearer_returns_missing_bearer() {
    let app = build_empty_app().await;
    let captured = send_wrapped_export_key(&app, None).await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "missing_bearer");
}

#[tokio::test]
async fn wrapped_export_key_non_base64_bearer_returns_invalid_bearer() {
    let app = build_empty_app().await;
    let captured = send_wrapped_export_key(&app, Some(&bearer_header("not-base64%%%"))).await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

#[tokio::test]
async fn wrapped_export_key_wrong_length_bearer_returns_invalid_bearer() {
    let app = build_empty_app().await;
    let short = URL_SAFE_NO_PAD.encode(b"foo");
    let captured = send_wrapped_export_key(&app, Some(&bearer_header(&short))).await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

#[tokio::test]
async fn wrapped_export_key_lease_miss_returns_invalid_bearer() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let (_, app) = build_app_from_dbs(
        missing_validation_db(),
        successful_tenant_db(tenant_id, "acme"),
    );

    let captured =
        send_wrapped_export_key(&app, Some(&bearer_header(&encoded_bearer(&bearer)))).await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

#[tokio::test]
async fn wrapped_export_key_expired_returns_expired_lease() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let (_, app) = build_app_from_dbs(
        expired_validation_db(&bearer, tenant_id),
        successful_tenant_db(tenant_id, "acme"),
    );

    let captured =
        send_wrapped_export_key(&app, Some(&bearer_header(&encoded_bearer(&bearer)))).await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "expired_lease");
}

#[tokio::test]
async fn wrapped_export_key_revoked_returns_revoked_lease() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let (_, app) = build_app_from_dbs(
        revoked_validation_db(&bearer, tenant_id),
        successful_tenant_db(tenant_id, "acme"),
    );

    let captured =
        send_wrapped_export_key(&app, Some(&bearer_header(&encoded_bearer(&bearer)))).await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "revoked_lease");
}

#[tokio::test]
async fn wrapped_export_key_db_error_returns_invalid_bearer() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let (_, app) = build_app_from_dbs(
        validation_db_error("db boom"),
        successful_tenant_db(tenant_id, "acme"),
    );

    let captured =
        send_wrapped_export_key(&app, Some(&bearer_header(&encoded_bearer(&bearer)))).await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

#[tokio::test]
async fn api_auth_whoami_validate_request_lease_miss_returns_invalid_bearer() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let (_, app) = build_app_from_dbs(
        missing_validation_db(),
        successful_tenant_db(tenant_id, "acme"),
    );

    let captured = send_whoami(&app, Some(&bearer_header(&encoded_bearer(&bearer)))).await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

#[tokio::test]
async fn api_auth_whoami_validate_request_lease_expired_returns_expired_lease() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let (_, app) = build_app_from_dbs(
        expired_validation_db(&bearer, tenant_id),
        successful_tenant_db(tenant_id, "acme"),
    );

    let captured = send_whoami(&app, Some(&bearer_header(&encoded_bearer(&bearer)))).await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "expired_lease");
}

#[tokio::test]
async fn api_auth_whoami_validate_request_lease_revoked_returns_revoked_lease() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let (_, app) = build_app_from_dbs(
        revoked_validation_db(&bearer, tenant_id),
        successful_tenant_db(tenant_id, "acme"),
    );

    let captured = send_whoami(&app, Some(&bearer_header(&encoded_bearer(&bearer)))).await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "revoked_lease");
}

#[tokio::test]
async fn api_auth_whoami_validate_request_lease_db_error_returns_invalid_bearer() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let (_, app) = build_app_from_dbs(
        validation_db_error("db boom"),
        successful_tenant_db(tenant_id, "acme"),
    );

    let captured = send_whoami(&app, Some(&bearer_header(&encoded_bearer(&bearer)))).await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

#[tokio::test]
async fn api_auth_whoami_valid_bearer_returns_identity_json() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let lease_db = successful_validation_db(&bearer, tenant_id);
    let tenant_db = successful_tenant_db(tenant_id, "acme");
    let (_, app) = build_app_from_dbs(lease_db, tenant_db);

    let captured = send_whoami(&app, Some(&bearer_header(&encoded_bearer(&bearer)))).await;
    assert_eq!(captured.status, StatusCode::OK);
    let body = captured.json.as_ref().expect("json body");
    assert_eq!(body["tenant"], "acme");
    assert!(body["expires_at"].is_string(), "body={}", captured.raw_body);
    assert!(
        body["idle_extends_to"].is_string(),
        "body={}",
        captured.raw_body
    );
    assert!(Uuid::parse_str(body["lease_id"].as_str().expect("lease id string")).is_ok());
}

#[tokio::test]
async fn api_auth_logout_valid_bearer_clears_cookie_without_secure_by_default() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let model = live_lease_model(&bearer, tenant_id);
    let updated = lease::Model {
        idle_extends_to: model.idle_extends_to + Duration::hours(1),
        ..model.clone()
    };
    let lease_db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![model], vec![updated]])
        .append_exec_results(vec![MockExecResult {
            last_insert_id: 0,
            rows_affected: 1,
        }])
        .into_connection();
    let tenant_db = successful_tenant_db(tenant_id, "acme");
    let (_, app) = build_app_from_dbs(lease_db, tenant_db);

    let captured = send_logout(&app, Some(&bearer_header(&encoded_bearer(&bearer))), None).await;
    assert_eq!(captured.status, StatusCode::NO_CONTENT);
    assert_eq!(captured.raw_body, "");
    let set_cookie = header(&captured, "set-cookie").expect("set-cookie");
    assert!(
        set_cookie.contains(&format!("{BOTWORK_CAP_COOKIE_NAME}=")),
        "missing cookie name, got {set_cookie}"
    );
    for expected in [
        "Path=/",
        "HttpOnly",
        "SameSite=Lax",
        "Max-Age=0",
        "Expires=Thu, 01 Jan 1970 00:00:00 GMT",
    ] {
        assert!(
            set_cookie.contains(expected),
            "missing cookie attribute {expected}, got {set_cookie}"
        );
    }
    assert!(
        !set_cookie.contains("; Secure"),
        "cookie must omit Secure without x-forwarded-proto=https, got {set_cookie}"
    );
}

#[tokio::test]
async fn api_auth_logout_valid_bearer_sets_secure_cookie_for_https() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let model = live_lease_model(&bearer, tenant_id);
    let updated = lease::Model {
        idle_extends_to: model.idle_extends_to + Duration::hours(1),
        ..model.clone()
    };
    let lease_db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![model], vec![updated]])
        .append_exec_results(vec![MockExecResult {
            last_insert_id: 0,
            rows_affected: 1,
        }])
        .into_connection();
    let tenant_db = successful_tenant_db(tenant_id, "acme");
    let (_, app) = build_app_from_dbs(lease_db, tenant_db);

    let captured = send_logout(
        &app,
        Some(&bearer_header(&encoded_bearer(&bearer))),
        Some("https"),
    )
    .await;
    assert_eq!(captured.status, StatusCode::NO_CONTENT);
    let set_cookie = header(&captured, "set-cookie").expect("set-cookie");
    assert!(
        set_cookie.contains("; Secure"),
        "cookie must include Secure when x-forwarded-proto=https, got {set_cookie}"
    );
}

#[tokio::test]
async fn api_auth_logout_revoke_db_error_returns_500() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let model = live_lease_model(&bearer, tenant_id);
    let updated = lease::Model {
        idle_extends_to: model.idle_extends_to + Duration::hours(1),
        ..model.clone()
    };
    let lease_db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![model], vec![updated]])
        .append_exec_errors(vec![DbErr::Custom("connection lost".to_string())])
        .into_connection();
    let tenant_db = successful_tenant_db(tenant_id, "acme");
    let (_, app) = build_app_from_dbs(lease_db, tenant_db);

    let captured = send_logout(&app, Some(&bearer_header(&encoded_bearer(&bearer))), None).await;
    assert_api_error(&captured, StatusCode::INTERNAL_SERVER_ERROR, "internal");
    let body = captured.json.as_ref().expect("json body");
    assert_eq!(
        body["error"]["message"],
        "database error: Custom Error: connection lost"
    );
    assert!(
        header(&captured, "set-cookie").is_none(),
        "revoke error path returns before clearing cookie"
    );
}

#[tokio::test]
async fn api_auth_login_missing_fields_returns_bad_request() {
    let app = build_empty_app().await;
    let captured = send_login(&app, json!({})).await;
    assert_api_error(&captured, StatusCode::BAD_REQUEST, "bad_request");
    let body = captured.json.as_ref().expect("json body");
    assert_eq!(
        body["error"]["message"],
        "expected either `opaque_login_request` or (`handshake_id`, `opaque_login_finalization`)"
    );
}

#[tokio::test]
async fn auth_check_expired_lease_returns_expired_lease() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let (_, app) = build_app_from_dbs(
        expired_validation_db(&bearer, tenant_id),
        successful_tenant_db(tenant_id, "acme"),
    );

    let captured = send_auth_check(
        &app,
        Some(&bearer_header(&encoded_bearer(&bearer))),
        "/acme/ns/plugin",
    )
    .await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "expired_lease");
}

#[tokio::test]
async fn auth_check_revoked_lease_returns_revoked_lease() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let (_, app) = build_app_from_dbs(
        revoked_validation_db(&bearer, tenant_id),
        successful_tenant_db(tenant_id, "acme"),
    );

    let captured = send_auth_check(
        &app,
        Some(&bearer_header(&encoded_bearer(&bearer))),
        "/acme/ns/plugin",
    )
    .await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "revoked_lease");
}

#[tokio::test]
async fn auth_check_lease_db_error_returns_invalid_bearer() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let (_, app) = build_app_from_dbs(
        validation_db_error("db boom"),
        successful_tenant_db(tenant_id, "acme"),
    );

    let captured = send_auth_check(
        &app,
        Some(&bearer_header(&encoded_bearer(&bearer))),
        "/acme/ns/plugin",
    )
    .await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

#[tokio::test]
async fn auth_check_lease_miss_evicts_matching_caps() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let bearer_value = encoded_bearer(&bearer);
    let (state, app) = build_app_from_dbs(
        missing_validation_db(),
        successful_tenant_db(tenant_id, "acme"),
    );
    let cache_key = cache_key("acme", &bearer_value);
    let cap_id = botwork_auth_broker::caps::mint_cap_id();
    state
        .insert_cap_for_test(
            cap_id,
            CapEntry {
                cache_key,
                namespace: "ns".to_string(),
                plugin: "plugin".to_string(),
                expires_at: Instant::now() + CAP_TTL,
                lease_id: Uuid::new_v4(),
            },
        )
        .await;
    assert_eq!(state.caps_len().await, 1);

    let captured =
        send_auth_check(&app, Some(&bearer_header(&bearer_value)), "/acme/ns/plugin").await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
    assert_eq!(
        state.caps_len().await,
        0,
        "miss should evict the stale cap cohort"
    );
}

#[tokio::test]
async fn auth_check_tenant_lookup_miss_returns_invalid_bearer() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let (_, app) = build_app_from_dbs(
        successful_validation_db(&bearer, tenant_id),
        missing_tenant_db(),
    );

    let captured = send_auth_check(
        &app,
        Some(&bearer_header(&encoded_bearer(&bearer))),
        "/acme/ns/plugin",
    )
    .await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

#[tokio::test]
async fn auth_check_tenant_lookup_db_error_returns_invalid_bearer() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let (_, app) = build_app_from_dbs(
        successful_validation_db(&bearer, tenant_id),
        tenant_db_error("tenant db down"),
    );

    let captured = send_auth_check(
        &app,
        Some(&bearer_header(&encoded_bearer(&bearer))),
        "/acme/ns/plugin",
    )
    .await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

#[tokio::test]
async fn auth_check_tenant_mismatch_returns_invalid_bearer() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let (_, app) = build_app_from_dbs(
        successful_validation_db(&bearer, tenant_id),
        successful_tenant_db(tenant_id, "other"),
    );

    let captured = send_auth_check(
        &app,
        Some(&bearer_header(&encoded_bearer(&bearer))),
        "/acme/ns/plugin",
    )
    .await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

// ---------------------------------------------------------------------------
// P9 — cookie/helper arms
// ---------------------------------------------------------------------------

/// Helper to send an auth-check request with a cookie header (no Authorization).
async fn send_auth_check_with_cookie(
    app: &axum::Router,
    cookie: &str,
    original_path: &str,
) -> Captured {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/check")
                .header("cookie", cookie)
                .header("x-envoy-original-path", original_path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    capture(response).await
}

/// `ApiAuthLogin` arm: `x-envoy-original-path` matching the login path
/// returns 200 immediately — no bearer required.
#[tokio::test]
async fn auth_check_api_auth_login_path_returns_200() {
    let app = build_empty_app().await;
    // success_public_no_identity() / ApiAuthLogin returns plain-text "OK",
    // not JSON, so we bypass capture() here.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/check")
                .header("x-envoy-original-path", "/api/auth/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// `Spa` arm with no bearer: returns 200 OK body "OK" via
/// `success_public_no_identity()`.
#[tokio::test]
async fn auth_check_spa_path_with_no_bearer_returns_200() {
    let app = build_empty_app().await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/check")
                .header("x-envoy-original-path", "/acme")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(bytes.as_ref(), b"OK");
}

/// `Spa` arm with bearer via cookie: exercises `extract_cookie_cap`'s
/// inner loop, returns 401 because the MockDatabase has no matching lease.
#[tokio::test]
async fn auth_check_spa_path_with_bearer_via_cookie_is_401() {
    // Use a SeaOrm MockDatabase so validate_and_extend returns Ok(None).
    let (_, app) = build_app_from_dbs(missing_validation_db(), missing_tenant_db());
    // 32-byte URL-safe-base64 — a well-formed bearer that decodes but
    // finds no matching lease in the MockDatabase.
    let bearer_str = URL_SAFE_NO_PAD.encode([0x42u8; 32]);
    let cookie = format!("{BOTWORK_CAP_COOKIE_NAME}={bearer_str}");
    let captured = send_auth_check_with_cookie(&app, &cookie, "/acme").await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

/// `ApiAuthProtected` arm with no bearer returns 401 `missing_bearer`.
#[tokio::test]
async fn auth_check_api_auth_protected_path_missing_bearer_is_401() {
    let app = build_empty_app().await;
    let captured = send_auth_check(&app, None, "/api/auth/whoami").await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "missing_bearer");
}

/// `Api { tenant: None }` arm with no bearer returns 401 `missing_bearer`.
#[tokio::test]
async fn auth_check_api_path_no_tenant_missing_bearer_is_401() {
    let app = build_empty_app().await;
    let captured = send_auth_check(&app, None, "/api").await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "missing_bearer");
}

/// Logout with the bearer delivered via a cookie (no Authorization header).
/// Exercises `extract_cookie_cap`'s successful return path and the
/// `validate_request_lease` → `request_cap` → cookie branch.
#[tokio::test]
async fn api_auth_logout_bearer_via_cookie_clears_cookie() {
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();

    let model = live_lease_model(&bearer, tenant_id);
    let updated = lease::Model {
        idle_extends_to: model.idle_extends_to + Duration::hours(1),
        ..model.clone()
    };
    let lease_db = MockDatabase::new(DatabaseBackend::Postgres)
        .append_query_results(vec![vec![model], vec![updated]])
        .append_exec_results(vec![MockExecResult {
            last_insert_id: 0,
            rows_affected: 1,
        }])
        .into_connection();
    let tenant_db = successful_tenant_db(tenant_id, "acme");
    let (_, app) = build_app_from_dbs(lease_db, tenant_db);

    // Deliver the bearer through the cookie, NOT via the Authorization header.
    let cookie = format!("{BOTWORK_CAP_COOKIE_NAME}={}", encoded_bearer(&bearer));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/logout")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let captured = capture(response).await;

    assert_eq!(captured.status, StatusCode::NO_CONTENT);
    assert_eq!(captured.raw_body, "");
    let set_cookie = header(&captured, "set-cookie").expect("set-cookie header");
    assert!(
        set_cookie.contains(&format!("{BOTWORK_CAP_COOKIE_NAME}=")),
        "logout via cookie must clear auth cookie, got {set_cookie}"
    );
    assert!(
        set_cookie.contains("Max-Age=0"),
        "cookie must be zeroed, got {set_cookie}"
    );
}

#[tokio::test]
async fn auth_check_valid_lease_creates_vault_inserts_cache_and_mints_cap() {
    let dir = tempdir().expect("tempdir");
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let tenant = "acme";
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let bearer_value = encoded_bearer(&bearer);
    let lease_store = Arc::new(MockLeaseStore::new());
    lease_store.push_outcome(LeaseOutcome::Valid(live_lease_row(tenant_id)));
    let (state, app) = build_mock_check_app(vault_root.clone(), tenant, tenant_id, lease_store);

    let tenant_root = vault_root.join(tenant);
    assert!(
        !tenant_root.exists(),
        "test requires a fresh tenant vault root"
    );

    let captured = send_auth_check(
        &app,
        Some(&bearer_header(&bearer_value)),
        "/acme/ns/exec-bash",
    )
    .await;
    assert_eq!(captured.status, StatusCode::OK);
    assert_eq!(header(&captured, "x-botwork-tenant"), Some("acme"));
    assert!(header(&captured, "x-botwork-cap").is_some());
    assert!(
        tenant_root.exists(),
        "vault should be created on first lease hit"
    );
    assert_eq!(state.cache_len().await, 1);
    assert_eq!(state.caps_len().await, 1);
    assert_eq!(state.metrics_snapshot().await.counters.cache_inserts, 1);
}

#[tokio::test]
async fn auth_check_valid_lease_unlocks_existing_vault_and_mints_cap() {
    let dir = tempdir().expect("tempdir");
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let tenant = "acme";
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let bearer_value = encoded_bearer(&bearer);
    let suite_version = botwork_opaque_handshake::SUITE_VERSION;
    let export_key = [0x42u8; 32];
    Vault::create(vault_root.join(tenant), &export_key, suite_version)
        .expect("precreate tenant vault");

    let lease_store = Arc::new(MockLeaseStore::new());
    lease_store.push_outcome(LeaseOutcome::Valid(live_lease_row(tenant_id)));
    let (state, app) = build_mock_check_app(vault_root, tenant, tenant_id, lease_store);

    let captured = send_auth_check(
        &app,
        Some(&bearer_header(&bearer_value)),
        "/acme/ns/exec-bash",
    )
    .await;
    assert_eq!(captured.status, StatusCode::OK);
    assert_eq!(header(&captured, "x-botwork-tenant"), Some("acme"));
    assert!(header(&captured, "x-botwork-cap").is_some());
    assert_eq!(state.cache_len().await, 1);
    assert_eq!(state.caps_len().await, 1);
    assert_eq!(state.metrics_snapshot().await.counters.cache_inserts, 1);
}

#[tokio::test]
async fn auth_check_cache_hit_extends_last_used_without_second_insert() {
    let dir = tempdir().expect("tempdir");
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let tenant = "acme";
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let bearer_value = encoded_bearer(&bearer);
    let key = cache_key(tenant, &bearer_value);
    let lease_store = Arc::new(MockLeaseStore::new());
    lease_store.push_outcome(LeaseOutcome::Valid(live_lease_row(tenant_id)));
    lease_store.push_outcome(LeaseOutcome::Valid(live_lease_row(tenant_id)));
    let (state, app) = build_mock_check_app(vault_root, tenant, tenant_id, lease_store);

    let first = send_auth_check(
        &app,
        Some(&bearer_header(&bearer_value)),
        "/acme/ns/exec-bash",
    )
    .await;
    assert_eq!(first.status, StatusCode::OK);
    let first_cap = header(&first, "x-botwork-cap")
        .expect("first cap")
        .to_string();
    let (_, first_last_used) = state.entry_times(key).await.expect("cache entry");

    sleep(StdDuration::from_millis(5)).await;

    let second = send_auth_check(
        &app,
        Some(&bearer_header(&bearer_value)),
        "/acme/ns/exec-bash",
    )
    .await;
    assert_eq!(second.status, StatusCode::OK);
    let second_cap = header(&second, "x-botwork-cap").expect("second cap");
    assert_ne!(first_cap, second_cap, "each check should mint a fresh cap");
    let (_, second_last_used) = state.entry_times(key).await.expect("cache entry");
    assert!(
        second_last_used > first_last_used,
        "cache-hit path should slide last_used forward"
    );
    assert_eq!(state.cache_len().await, 1);
    assert_eq!(state.metrics_snapshot().await.counters.cache_inserts, 1);
}

// ---------------------------------------------------------------------------
// try_lease_path happy-path: vault create / unlock / cache-hit branches
//
// These tests drive `handler.rs` lines ~991–1083 through `/auth/check`
// with a real TempDir vault, using MockLeaseStore so `validated.export_key`
// is always the deterministic `[0x42u8; 32]` the mock returns.
// ---------------------------------------------------------------------------

/// Deterministic export-key bytes returned by `MockLeaseStore` for
/// `LeaseOutcome::Valid`. Pre-creating a vault with these bytes ensures
/// `vault.unlock_master` succeeds when the handler calls it.
const MOCK_EXPORT_KEY: [u8; 32] = [0x42u8; 32];

/// Lease validity window used in test `LeaseRow` fixtures.
const TEST_LEASE_DURATION_HOURS: i64 = 2;

/// Build an `(AppState, Router)` pair pointing at `vault_root`, backed
/// entirely by in-process mock stores (no SeaORM, no Docker).
fn build_mock_app(
    vault_root: &std::path::Path,
    lease_store: Arc<MockLeaseStore>,
    tenant_store: Arc<MockTenantStore>,
) -> (AppState, axum::Router) {
    let ls: Arc<dyn LeaseStore + Send + Sync> = lease_store;
    let ts: Arc<dyn TenantStore + Send + Sync> = tenant_store;
    let auth = build_auth_state(ls, ts);
    let state = AppState::with_auth(vault_root.to_path_buf(), auth);
    let app = build_router(state.clone());
    (state, app)
}

/// Minimal live `LeaseRow` for `MockLeaseStore::push_outcome(Valid(...))`.
fn fresh_lease_row(tenant_id: Uuid) -> LeaseRow {
    let now = Utc::now();
    LeaseRow {
        id: Uuid::new_v4(),
        tenant_id,
        issued_at: now,
        expires_at: now + Duration::hours(TEST_LEASE_DURATION_HOURS),
        idle_extends_to: now + Duration::hours(TEST_LEASE_DURATION_HOURS),
        revoked_at: None,
    }
}

/// **Fresh-vault auto-create branch** (`handler.rs` ~995–1033):
/// The tenant vault directory does not yet exist, so `Vault::new` +
/// `unlock_master` returns `VaultError::NotInitialized` and the handler
/// auto-creates a fresh v4 vault via `Vault::create`, then unlocks it.
/// Asserts 200 with both `x-botwork-tenant` and `x-botwork-cap` headers.
#[tokio::test]
async fn auth_check_valid_lease_fresh_vault_auto_creates_and_returns_200() {
    let tenant_id = Uuid::new_v4();
    // TempDir must outlive the request so the vault file is accessible.
    let dir = tempdir().expect("tempdir");

    let lease_store = Arc::new(MockLeaseStore::new());
    lease_store.push_outcome(LeaseOutcome::Valid(fresh_lease_row(tenant_id)));
    let tenant_store = Arc::new(MockTenantStore::with_tenant("acme", tenant_id));

    // The tenant sub-directory deliberately does NOT pre-exist → auto-create path.
    let (_, app) = build_mock_app(dir.path(), lease_store, tenant_store);

    let bearer = Bearer::generate();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/check")
                .header("authorization", bearer_header(&encoded_bearer(&bearer)))
                .header("x-envoy-original-path", "/acme/ns/plugin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-botwork-tenant")
            .and_then(|v| v.to_str().ok()),
        Some("acme"),
        "x-botwork-tenant header absent or wrong"
    );
    assert!(
        response.headers().contains_key("x-botwork-cap"),
        "x-botwork-cap header absent"
    );
}

/// **Existing-vault unlock branch** (`handler.rs` ~993–994):
/// Pre-create the tenant vault under `TempDir` with the same export-key
/// bytes (`[0x42; 32]`) that `MockLeaseStore` returns in `Valid`, so the
/// handler's `vault.unlock_master(...)` path succeeds instead of
/// falling through to auto-create.
/// Asserts 200 with both `x-botwork-tenant` and `x-botwork-cap` headers.
#[tokio::test]
async fn auth_check_valid_lease_existing_vault_unlocks_and_returns_200() {
    let tenant_id = Uuid::new_v4();
    let dir = tempdir().expect("tempdir");

    // Pre-create the vault under the tenant sub-directory using the same
    // 32-byte export key the MockLeaseStore will hand to the handler.
    let suite_version = botwork_opaque_handshake::SUITE_VERSION;
    Vault::create(dir.path().join("acme"), &MOCK_EXPORT_KEY, suite_version)
        .expect("pre-create tenant vault");

    let lease_store = Arc::new(MockLeaseStore::new());
    lease_store.push_outcome(LeaseOutcome::Valid(fresh_lease_row(tenant_id)));
    let tenant_store = Arc::new(MockTenantStore::with_tenant("acme", tenant_id));

    let (_, app) = build_mock_app(dir.path(), lease_store, tenant_store);

    let bearer = Bearer::generate();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/check")
                .header("authorization", bearer_header(&encoded_bearer(&bearer)))
                .header("x-envoy-original-path", "/acme/ns/plugin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-botwork-tenant")
            .and_then(|v| v.to_str().ok()),
        Some("acme"),
        "x-botwork-tenant header absent or wrong"
    );
    assert!(
        response.headers().contains_key("x-botwork-cap"),
        "x-botwork-cap header absent"
    );
}

/// **Cache-hit sliding-extend branch** (`handler.rs` ~1051–1052):
/// Issue the same bearer+tenant twice against the same `AppState`.
/// The first call creates the vault and inserts the cache entry;
/// the second call hits `cache.get_mut(&cache_key)` and only updates
/// `last_used` rather than re-inserting a new entry.
/// Asserts both responses are 200 and the cache size remains 1.
#[tokio::test]
async fn auth_check_valid_lease_cache_hit_sliding_extend_both_return_200() {
    let tenant_id = Uuid::new_v4();
    let dir = tempdir().expect("tempdir");

    let lease_store = Arc::new(MockLeaseStore::new());
    // Queue two outcomes — one for each request.
    lease_store.push_outcome(LeaseOutcome::Valid(fresh_lease_row(tenant_id)));
    lease_store.push_outcome(LeaseOutcome::Valid(fresh_lease_row(tenant_id)));
    let tenant_store = Arc::new(MockTenantStore::with_tenant("acme", tenant_id));

    let (state, app) = build_mock_app(dir.path(), lease_store, tenant_store);

    let bearer = Bearer::generate();
    let auth_header = bearer_header(&encoded_bearer(&bearer));

    // First request: vault does not exist → auto-create → cache insert.
    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/check")
                .header("authorization", &auth_header)
                .header("x-envoy-original-path", "/acme/ns/plugin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK, "first request must succeed");
    assert_eq!(
        state.cache_len().await,
        1,
        "cache must have one entry after first request"
    );

    // Second request with the same bearer+tenant: cache hit → sliding extend,
    // no new insertion.
    let second = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/check")
                .header("authorization", &auth_header)
                .header("x-envoy-original-path", "/acme/ns/plugin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        second.status(),
        StatusCode::OK,
        "second request must succeed"
    );
    assert_eq!(
        state.cache_len().await,
        1,
        "cache must not grow on a cache-hit sliding extend"
    );
}

// ===========================================================================
// P10 — check() path-dispatch arms (bad path, Spa, ApiAuthProtected, Api)
// ===========================================================================

/// `check()` with no `x-envoy-original-path` header: path is an empty string
/// which `parse_original_path` can't match — covers the `warn!` argument
/// expression `if original_path.is_empty() { "<missing>" }` branch.
#[tokio::test]
async fn check_bad_path_empty_returns_401() {
    let app = build_empty_app().await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/check")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["code"], "invalid_bearer");
}

/// `check()` with a non-empty path that `parse_original_path` rejects
/// (e.g. `/api/auth` exactly, which the grammar explicitly returns `None` for).
/// Covers the `else { original_path }` arm of the `warn!` argument expression.
#[tokio::test]
async fn check_bad_path_nonempty_returns_401() {
    let app = build_empty_app().await;
    let captured = send_auth_check(&app, None, "/api/auth").await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

/// `Spa` arm with a malformed `Authorization` header (not `******
/// `request_cap` returns `Err(InvalidBearer)` → covers the `Err(code) =>` arm.
#[tokio::test]
async fn check_spa_with_invalid_authorization_returns_401() {
    let app = build_empty_app().await;
    let captured = send_auth_check(&app, Some("Basic abc"), "/acme").await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

/// `Spa` arm with a valid bearer: exercises the full `try_lease_path` →
/// vault-create → `LeasePathOutcome::Hit` path.  Covers lines 229–233.
#[tokio::test]
async fn check_spa_with_valid_bearer_returns_200() {
    let dir = tempdir().expect("tempdir");
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let lease_store = Arc::new(MockLeaseStore::new());
    lease_store.push_outcome(LeaseOutcome::Valid(live_lease_row(tenant_id)));
    let (_, app) = build_mock_check_app(vault_root, "acme", tenant_id, lease_store);

    let captured = send_auth_check(
        &app,
        Some(&bearer_header(&encoded_bearer(&bearer))),
        "/acme",
    )
    .await;
    assert_eq!(captured.status, StatusCode::OK);
    assert_eq!(header(&captured, "x-botwork-tenant"), Some("acme"));
    assert!(header(&captured, "x-botwork-cap").is_some());
}

/// `ApiAuthProtected` arm with a malformed `Authorization` header:
/// covers the `Err(code) =>` arm (line 279).
#[tokio::test]
async fn check_api_auth_protected_with_invalid_authorization_returns_401() {
    let app = build_empty_app().await;
    let captured = send_auth_check(&app, Some("Basic abc"), "/api/auth/whoami").await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

/// `ApiAuthProtected` arm with a valid bearer: exercises `try_lease_path` →
/// vault-create → `Hit`.  Covers lines 262–276.
#[tokio::test]
async fn check_api_auth_protected_with_valid_bearer_returns_200() {
    let dir = tempdir().expect("tempdir");
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let lease_store = Arc::new(MockLeaseStore::new());
    lease_store.push_outcome(LeaseOutcome::Valid(live_lease_row(tenant_id)));
    let (_, app) = build_mock_check_app(vault_root, "acme", tenant_id, lease_store);

    let captured = send_auth_check(
        &app,
        Some(&bearer_header(&encoded_bearer(&bearer))),
        "/api/auth/whoami",
    )
    .await;
    assert_eq!(captured.status, StatusCode::OK);
}

/// `Api` arm with a malformed `Authorization` header: covers the `Err(code) =>`
/// arm (line 301).
#[tokio::test]
async fn check_api_with_invalid_authorization_returns_401() {
    let app = build_empty_app().await;
    let captured = send_auth_check(&app, Some("Basic abc"), "/api").await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

/// `Api` arm with a valid bearer: exercises `try_lease_path` → vault-create
/// → `Hit`.  Covers lines 284–298.
#[tokio::test]
async fn check_api_with_valid_bearer_returns_200() {
    let dir = tempdir().expect("tempdir");
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let tenant_id = Uuid::new_v4();
    let bearer = Bearer::generate();
    let lease_store = Arc::new(MockLeaseStore::new());
    lease_store.push_outcome(LeaseOutcome::Valid(live_lease_row(tenant_id)));
    let (_, app) = build_mock_check_app(vault_root, "acme", tenant_id, lease_store);

    let captured =
        send_auth_check(&app, Some(&bearer_header(&encoded_bearer(&bearer))), "/api").await;
    assert_eq!(captured.status, StatusCode::OK);
}

// ===========================================================================
// P11 — try_lease_path guard arms (bad base64, wrong length)
// ===========================================================================

/// ****** contains characters not in the URL-safe-base64 alphabet:
/// `URL_SAFE_NO_PAD.decode` returns `Err` → covers the base64-failure
/// `warn!` + early return (lines 921–924).
#[tokio::test]
async fn check_mcp_bad_base64_bearer_returns_401() {
    let app = build_empty_app().await;
    let captured = send_auth_check(&app, Some("******"), "/acme/mcp/exec-bash").await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

/// ****** base64 decodes to fewer than 32 bytes: covers the length
/// guard `warn!` + early return (lines 929–932).
#[tokio::test]
async fn check_mcp_wrong_length_bearer_returns_401() {
    // "foo" base64-encodes to "Zm9v" (3 bytes decoded), not BEARER_BYTES(32).
    let short = URL_SAFE_NO_PAD.encode(b"foo");
    let app = build_empty_app().await;
    let captured = send_auth_check(&app, Some(&bearer_header(&short)), "/acme/mcp/exec-bash").await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

// ===========================================================================
// P12 — fetch() non-UTF8 cap header (lines 334–335)
// ===========================================================================

/// `/secrets/fetch` with a `x-botwork-cap` header that contains non-UTF-8
/// bytes: `raw_cap_header.to_str()` fails → covers the `Err(_) => warn! +
/// 401` arm (lines 334–335).
#[tokio::test]
async fn fetch_non_utf8_cap_header_returns_401() {
    let (state, _vault_root) = common::build_offline_app_state().await;
    let app = build_router(state);
    // 0xE9 is valid as a Latin-1 HTTP header byte but is NOT valid UTF-8
    // by itself (it is the lead byte of a 3-byte UTF-8 sequence, so a
    // single 0xE9 is malformed).
    let non_utf8_cap = HeaderValue::from_bytes(&[0xE9u8, b'x', b'y']).expect("valid header bytes");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/secrets/fetch")
                .header("x-botwork-cap", non_utf8_cap)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["code"], "invalid_bearer");
}

// ===========================================================================
// P13 — wrapped_export_key success path (lines 528–555)
// ===========================================================================

/// `/auth/lease/wrapped-export-key` with a valid bearer: `validate_and_extend`
/// returns `Ok(Some(validated))` → covers the success arm (lines 528–555).
#[tokio::test]
async fn wrapped_export_key_valid_bearer_returns_json() {
    let tenant_id = Uuid::new_v4();
    let lease_store = Arc::new(MockLeaseStore::new());
    lease_store.push_outcome(LeaseOutcome::Valid(live_lease_row(tenant_id)));
    let lease_store_dyn: Arc<dyn LeaseStore + Send + Sync> = lease_store;
    let tenant_store_dyn: Arc<dyn TenantStore + Send + Sync> = Arc::new(MockTenantStore::new());
    let state = build_state(build_auth_state(lease_store_dyn, tenant_store_dyn));
    let app = build_router(state);
    let bearer = Bearer::generate();

    let captured =
        send_wrapped_export_key(&app, Some(&bearer_header(&encoded_bearer(&bearer)))).await;

    assert_eq!(captured.status, StatusCode::OK);
    let body = captured.json.as_ref().expect("json body");
    assert!(
        body["wrapped_export_key"].is_string(),
        "body={}",
        captured.raw_body
    );
    assert!(
        body["suite_version"].is_number(),
        "body={}",
        captured.raw_body
    );
}

// ===========================================================================
// P14 — validate_request_lease tenant lookup arms (lines 718–724)
// ===========================================================================

/// `validate_request_lease` in `api_auth_whoami`: lease validates but
/// `lookup_tenant_name_by_id` returns `Ok(None)` — covers line 718.
#[tokio::test]
async fn whoami_tenant_not_found_returns_401() {
    let tenant_id = Uuid::new_v4();
    let lease_store = Arc::new(MockLeaseStore::new());
    lease_store.push_outcome(LeaseOutcome::Valid(live_lease_row(tenant_id)));
    let lease_store_dyn: Arc<dyn LeaseStore + Send + Sync> = lease_store;
    // Empty TenantStore → lookup_tenant_name_by_id returns Ok(None).
    let tenant_store_dyn: Arc<dyn TenantStore + Send + Sync> = Arc::new(MockTenantStore::new());
    let state = build_state(build_auth_state(lease_store_dyn, tenant_store_dyn));
    let app = build_router(state);
    let bearer = Bearer::generate();

    let captured = send_whoami(&app, Some(&bearer_header(&encoded_bearer(&bearer)))).await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

/// `validate_request_lease` in `api_auth_whoami`: lease validates but
/// `lookup_tenant_name_by_id` returns `Err` — covers lines 719–724.
#[tokio::test]
async fn whoami_tenant_db_error_returns_401() {
    let tenant_id = Uuid::new_v4();
    let lease_store = Arc::new(MockLeaseStore::new());
    lease_store.push_outcome(LeaseOutcome::Valid(live_lease_row(tenant_id)));
    let lease_store_dyn: Arc<dyn LeaseStore + Send + Sync> = lease_store;
    let tenant_store_dyn: Arc<dyn TenantStore + Send + Sync> =
        Arc::new(MockTenantStore::always_error("tenant db down"));
    let state = build_state(build_auth_state(lease_store_dyn, tenant_store_dyn));
    let app = build_router(state);
    let bearer = Bearer::generate();

    let captured = send_whoami(&app, Some(&bearer_header(&encoded_bearer(&bearer)))).await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

// ===========================================================================
// P15 — api_auth_login routing arms (lines 742–808)
// ===========================================================================

/// `opaque_login_request` is present but `tenant` is absent:
/// covers the `let Some(tenant) = body.tenant else { return 400 }` arm
/// (lines 742–748).
#[tokio::test]
async fn login_start_opaque_request_missing_tenant_returns_400() {
    let app = build_empty_app().await;
    let captured = send_login(
        &app,
        json!({
            "opaque_login_request": "abc"
        }),
    )
    .await;
    assert_api_error(&captured, StatusCode::BAD_REQUEST, "bad_request");
    let body = captured.json.as_ref().expect("json body");
    assert_eq!(
        body["error"]["message"],
        "`tenant` is required for login start"
    );
}

/// `opaque_login_request` + `tenant` present but the base64 is invalid:
/// `login_start_inner` returns `Err` → covers the `Err(response) => response`
/// passthrough (lines 749–767).
#[tokio::test]
async fn login_start_with_opaque_request_routes_through_login_start_inner() {
    let app = build_empty_app().await;
    let captured = send_login(
        &app,
        json!({
            "opaque_login_request": "%%notbase64%%",
            "tenant": "acme"
        }),
    )
    .await;
    // b64_decode fails → login_start_inner returns Err(bad_request) → 400.
    assert_api_error(&captured, StatusCode::BAD_REQUEST, "bad_request");
    let body = captured.json.as_ref().expect("json body");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("login_request"),
        "body={}",
        captured.raw_body
    );
}

/// `opaque_login_request` + `tenant` with a valid base64-encoded OPAQUE
/// `LoginRequest`: `login_start_inner` succeeds → covers the `Ok(response) =>
/// Json(ApiLoginResponse::Start { … })` arm (lines 761–766).
#[tokio::test]
async fn login_start_valid_opaque_request_returns_200_start_response() {
    let mut rng = rand::thread_rng();
    let password = vec![0x01u8; 32];
    let setup = ServerSetup::generate(&mut rng);
    let tenant_id = Uuid::new_v4();

    let started = client::login_start(&mut rng, &password).expect("client login_start");
    let login_request_b64 = URL_SAFE_NO_PAD.encode(started.request.serialize());

    let auth = AuthState::from_stores(
        Arc::new(MockLeaseStore::new()),
        Arc::new(MockTenantStore::with_tenant("acme", tenant_id)),
        Arc::new(MockPasswordFileStore::new()),
        setup,
    );
    let state = build_state(auth);
    let app = build_router(state);

    let captured = send_login(
        &app,
        json!({
            "opaque_login_request": login_request_b64,
            "tenant": "acme"
        }),
    )
    .await;
    assert_eq!(captured.status, StatusCode::OK);
    let body = captured.json.as_ref().expect("json body");
    assert!(
        body["handshake_id"].is_string(),
        "body={}",
        captured.raw_body
    );
    assert!(
        body["opaque_login_response"].is_string(),
        "body={}",
        captured.raw_body
    );
}

/// `handshake_id` + `opaque_login_finalization` present, but the handshake_id
/// is unknown: `login_finish_inner` returns `Err(not_found)` → covers the
/// `Err(response) => response` arm (line 799).
#[tokio::test]
async fn login_finish_unknown_handshake_id_returns_error() {
    let app = build_empty_app().await;
    let captured = send_login(
        &app,
        json!({
            "handshake_id": Uuid::new_v4(),
            "opaque_login_finalization": "abc"
        }),
    )
    .await;
    // Pending map is empty → not_found or bad_request.
    assert!(
        captured.status == StatusCode::NOT_FOUND || captured.status == StatusCode::BAD_REQUEST,
        "expected 404 or 400, got {}",
        captured.status
    );
}

/// Neither `opaque_login_request` nor the finish pair is present:
/// covers the final `else` arm → `api_json_error(BAD_REQUEST, …)` (lines 802–806).
///
/// (Already tested by `api_auth_login_missing_fields_returns_bad_request`,
/// kept here for cross-reference clarity.)
#[tokio::test]
async fn login_neither_start_nor_finish_fields_returns_400() {
    let app = build_empty_app().await;
    let captured = send_login(&app, json!({ "tenant": "acme" })).await;
    assert_api_error(&captured, StatusCode::BAD_REQUEST, "bad_request");
}

// ===========================================================================
// P16 — api_auth_logout no-bearer path (lines 814–823)
// ===========================================================================

/// Logout with no bearer: `validate_request_lease` returns `Err(MissingBearer)`
/// → idempotent 204 + clear-cookie (lines 814–823).
#[tokio::test]
async fn logout_no_bearer_returns_204_and_clears_cookie() {
    let app = build_empty_app().await;
    let captured = send_logout(&app, None, None).await;
    assert_eq!(captured.status, StatusCode::NO_CONTENT);
    assert_eq!(captured.raw_body, "");
    let set_cookie = header(&captured, "set-cookie").expect("set-cookie header");
    assert!(
        set_cookie.contains(&format!("{BOTWORK_CAP_COOKIE_NAME}=")),
        "must clear the auth cookie, got {set_cookie}"
    );
    assert!(set_cookie.contains("Max-Age=0"), "got {set_cookie}");
    assert!(
        !set_cookie.contains("; Secure"),
        "no Secure flag without https header, got {set_cookie}"
    );
}

/// Logout with no bearer and `x-forwarded-proto: https`: the clear-cookie
/// must include the `Secure` flag.
#[tokio::test]
async fn logout_no_bearer_https_clears_cookie_with_secure_flag() {
    let app = build_empty_app().await;
    let captured = send_logout(&app, None, Some("https")).await;
    assert_eq!(captured.status, StatusCode::NO_CONTENT);
    let set_cookie = header(&captured, "set-cookie").expect("set-cookie header");
    assert!(
        set_cookie.contains("; Secure"),
        "must include Secure flag for https, got {set_cookie}"
    );
}

// ===========================================================================
// P17 — build_app_state + build_user_api_router (lib.rs 43–44, handler.rs 886–887)
// ===========================================================================

/// Calls `build_app_state` (the lib.rs convenience wrapper) to cover
/// lines 43–44 of lib.rs.
#[tokio::test]
async fn build_app_state_wrapper_creates_valid_app_state() {
    let dir = tempdir().expect("tempdir");
    let auth = offline_auth_state().await;
    let state = build_app_state(dir.path().to_path_buf(), auth);
    // Sanity: the returned state should have an empty cache.
    assert_eq!(state.cache_len().await, 0);
}

/// Calls `build_user_api_router` to cover lines 886–887 of handler.rs.
#[tokio::test]
async fn build_user_api_router_returns_a_router() {
    let dir = tempdir().expect("tempdir");
    let auth = offline_auth_state().await;
    let state = AppState::with_auth(dir.path().to_path_buf(), auth);
    // Just constructing the router is enough to cover the function body.
    let _router = build_user_api_router(state);
}

// ===========================================================================
// P18 — try_lease_path bearer-shape guards (handler.rs 921–932)
// ===========================================================================

/// A bearer whose raw value contains non-URL-safe-base64 characters is
/// rejected before any lease lookup: covers the `!Ok(decoded)` branch
/// (handler.rs lines 921–924).
#[tokio::test]
async fn auth_check_bearer_not_base64_is_401() {
    let (_, app) = build_app_from_dbs(missing_validation_db(), missing_tenant_db());
    // `!!!` is not valid URL-safe-no-pad base64.
    let captured = send_auth_check(
        &app,
        Some(&bearer_header("!!not-base64!!")),
        "/acme/ns/plugin",
    )
    .await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

/// A bearer whose raw value is valid base64 but decodes to fewer than
/// `BEARER_BYTES` (32) bytes is rejected: covers handler.rs lines 929–932.
#[tokio::test]
async fn auth_check_bearer_wrong_decoded_length_is_401() {
    let (_, app) = build_app_from_dbs(missing_validation_db(), missing_tenant_db());
    // Encode 8 bytes → decodes to 8, not 32.
    let short = URL_SAFE_NO_PAD.encode([0u8; 8]);
    let captured = send_auth_check(&app, Some(&bearer_header(&short)), "/acme/ns/plugin").await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

// ===========================================================================
// P19 — wrapped_export_key bearer decoded-length guard (handler.rs 519)
// ===========================================================================

/// A bearer that is valid base64 but decodes to a length other than
/// `BEARER_BYTES` (32) bytes is rejected at the length check in
/// `wrapped_export_key`: covers handler.rs line 519.
#[tokio::test]
async fn wrapped_export_key_wrong_decoded_length_bearer_is_401() {
    let app = build_empty_app().await;
    // Encode 16 bytes → decodes to 16, not 32.
    let short = URL_SAFE_NO_PAD.encode([0u8; 16]);
    let captured = send_wrapped_export_key(&app, Some(&bearer_header(&short))).await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

// ===========================================================================
// P20 — ApiAuthProtected path with bearer calls try_lease_path (handler.rs 274–277)
// ===========================================================================

/// POST `/auth/check` with `x-envoy-original-path: /api/auth/whoami` and a
/// valid-format bearer: exercises the `ParsedPath::ApiAuthProtected` arm that
/// calls `try_lease_path` → `LeasePathOutcome::Miss` → covers the multi-line
/// OR-pattern arm (handler.rs lines 274–277).
#[tokio::test]
async fn auth_check_api_auth_protected_path_with_bearer_hits_try_lease_path() {
    let (_, app) = build_app_from_dbs(missing_validation_db(), missing_tenant_db());
    let bearer = Bearer::generate();
    let captured = send_auth_check(
        &app,
        Some(&bearer_header(&URL_SAFE_NO_PAD.encode(bearer.as_bytes()))),
        "/api/auth/whoami",
    )
    .await;
    // Miss → 401 invalid_bearer.
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

// ===========================================================================
// P21 — Api path with bearer calls try_lease_path (handler.rs 296–299)
// ===========================================================================

/// POST `/auth/check` with `x-envoy-original-path: /api/tenant/acme/resource`
/// and a valid-format bearer: exercises the `ParsedPath::Api` arm that calls
/// `try_lease_path` → `LeasePathOutcome::Miss` → covers the multi-line
/// OR-pattern arm (handler.rs lines 296–299).
#[tokio::test]
async fn auth_check_api_path_with_bearer_hits_try_lease_path() {
    let (_, app) = build_app_from_dbs(missing_validation_db(), missing_tenant_db());
    let bearer = Bearer::generate();
    let captured = send_auth_check(
        &app,
        Some(&bearer_header(&URL_SAFE_NO_PAD.encode(bearer.as_bytes()))),
        "/api/tenant/acme/resource",
    )
    .await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

// ===========================================================================
// P22 — Spa path with bearer and expired lease (handler.rs 231)
// ===========================================================================

/// POST `/auth/check` with `x-envoy-original-path: /acme` and a bearer whose
/// lease is expired: exercises the `ParsedPath::Spa` arm's `try_lease_path`
/// call with `LeasePathOutcome::Expired` → covers handler.rs line 231.
#[tokio::test]
async fn auth_check_spa_path_with_expired_bearer_is_401() {
    let bearer = Bearer::generate();
    let tenant_id = Uuid::new_v4();
    let (_, app) = build_app_from_dbs(
        expired_validation_db(&bearer, tenant_id),
        missing_tenant_db(),
    );
    let captured = send_auth_check(
        &app,
        Some(&bearer_header(&URL_SAFE_NO_PAD.encode(bearer.as_bytes()))),
        "/acme",
    )
    .await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "expired_lease");
}

// ===========================================================================
// P23 — try_lease_path tenant mismatch (handler.rs 971)
// ===========================================================================

/// A lease belonging to tenant "acme" presented on path `/other/ns/plugin`
/// triggers the tenant-mismatch 401 branch (handler.rs line 971).
#[tokio::test]
async fn auth_check_tenant_mismatch_is_401() {
    let bearer = Bearer::generate();
    let acme_id = Uuid::new_v4();

    let lease_store: Arc<dyn LeaseStore + Send + Sync> = {
        let s = Arc::new(MockLeaseStore::new());
        s.push_outcome(LeaseOutcome::Valid(live_lease_row(acme_id)));
        s
    };
    // Tenant store resolves acme_id → "acme".
    let tenant_store: Arc<dyn TenantStore + Send + Sync> =
        Arc::new(MockTenantStore::with_tenant("acme", acme_id));
    let state = build_state(build_auth_state(lease_store, tenant_store));
    let app = build_router(state);

    // Path tenant is "other", but the lease resolves to "acme".
    let captured = send_auth_check(
        &app,
        Some(&bearer_header(&URL_SAFE_NO_PAD.encode(bearer.as_bytes()))),
        "/other/ns/plugin",
    )
    .await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "invalid_bearer");
}

// ===========================================================================
// Helpers for OPAQUE login-finish success tests
// ===========================================================================

fn make_password_file(setup: &ServerSetup, password: &[u8]) -> PasswordFile {
    let mut rng = rand::thread_rng();
    let started =
        client::registration_start(&mut rng, password).expect("client registration_start");
    let response = server::registration_start(setup, started.request, b"alice")
        .expect("server registration_start");
    let finished =
        client::registration_finish(&mut rng, started.state, password, response.response)
            .expect("client registration_finish");
    server::registration_finish(finished.upload)
}

async fn do_login_start_via_http(
    app: &axum::Router,
    tenant: &str,
    password: &[u8],
) -> (botwork_opaque_handshake::ClientLoginState, Uuid, String) {
    let mut rng = rand::thread_rng();
    let started = client::login_start(&mut rng, password).expect("client login_start");
    let request_b64 =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(started.request.serialize());

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login/start")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "tenant": tenant,
                        "credential_identifier": "alice",
                        "login_request": request_b64,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "login_start must succeed"
    );
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    let handshake_id: Uuid = body["handshake_id"]
        .as_str()
        .unwrap()
        .parse()
        .expect("handshake_id uuid");
    let login_response_b64 = body["login_response"].as_str().unwrap().to_string();
    (started.state, handshake_id, login_response_b64)
}

fn make_login_finalization_b64(
    state: botwork_opaque_handshake::ClientLoginState,
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

// ===========================================================================
// P24 — api_auth_login finish + cookie success (handler.rs 781–797)
// ===========================================================================

/// A successful OPAQUE login finish via `/api/auth/login` with the finish
/// body: `login_finish_inner` returns `Ok` → the bearer + set-cookie are
/// written into the response (handler.rs lines 781–797).
#[tokio::test]
async fn api_auth_login_finish_success_with_set_cookie() {
    let mut rng = rand::thread_rng();
    let tenant_id = Uuid::new_v4();
    let password: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
    let setup = ServerSetup::generate(&mut rng);
    let password_file = make_password_file(&setup, &password);

    let lease_store: Arc<dyn LeaseStore + Send + Sync> = Arc::new(MockLeaseStore::new());
    let tenant_store: Arc<dyn TenantStore + Send + Sync> =
        Arc::new(MockTenantStore::with_tenant("acme", tenant_id));
    let pf_store: Arc<dyn PasswordFileStore + Send + Sync> =
        Arc::new(MockPasswordFileStore::with_file(tenant_id, &password_file));
    let auth = AuthState::from_stores(lease_store, tenant_store, pf_store, setup);
    let dir = tempdir().expect("tempdir");
    let state = AppState::with_auth(dir.path().to_path_buf(), auth);
    let app = build_router(state);

    // Step 1: login/start via /auth/login/start (not /api/auth/login — the
    // start half is only on the auth subrouter).
    let (client_state, handshake_id, login_response_b64) =
        do_login_start_via_http(&app, "acme", &password).await;

    // Step 2: client finish.
    let finalization = make_login_finalization_b64(client_state, &password, &login_response_b64);

    // Step 3: POST to /api/auth/login with finish body.
    let captured = send_login(
        &app,
        json!({
            "handshake_id": handshake_id,
            "opaque_login_finalization": finalization,
        }),
    )
    .await;

    assert_eq!(captured.status, StatusCode::OK);
    // The response must carry a set-cookie header with the auth cookie.
    let set_cookie = header(&captured, "set-cookie").expect("set-cookie header");
    assert!(
        set_cookie.to_lowercase().contains("botwork_cap="),
        "set-cookie must contain the botwork_cap value, got: {set_cookie}"
    );
    // The JSON body must carry the bearer.
    let body = captured.json.as_ref().expect("json body");
    assert!(
        body["bearer"].is_string(),
        "expected bearer in response body, got: {body}"
    );
}

// ===========================================================================
// P25 — ApiAuthProtected path with expired lease (handler.rs line 269)
// ===========================================================================

/// POST `/auth/check` with `x-envoy-original-path: /api/auth/whoami` and a
/// bearer whose lease is expired: exercises the `ParsedPath::ApiAuthProtected`
/// arm's `try_lease_path` call with `LeasePathOutcome::Expired` → covers
/// handler.rs line 269 (`| LeasePathOutcome::Expired(response)`).
#[tokio::test]
async fn auth_check_api_auth_protected_expired_returns_expired_lease() {
    let bearer = Bearer::generate();
    let tenant_id = Uuid::new_v4();
    let (_, app) = build_app_from_dbs(
        expired_validation_db(&bearer, tenant_id),
        missing_tenant_db(),
    );
    let captured = send_auth_check(
        &app,
        Some(&bearer_header(&URL_SAFE_NO_PAD.encode(bearer.as_bytes()))),
        "/api/auth/whoami",
    )
    .await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "expired_lease");
}

// ===========================================================================
// P26 — Api path with expired lease (handler.rs line 291)
// ===========================================================================

/// POST `/auth/check` with `x-envoy-original-path: /api/tenant/acme/secrets`
/// and a bearer whose lease is expired: exercises the `ParsedPath::Api` arm's
/// `try_lease_path` call with `LeasePathOutcome::Expired` → covers handler.rs
/// line 291 (`| LeasePathOutcome::Expired(response)`).
#[tokio::test]
async fn auth_check_api_tenant_expired_returns_expired_lease() {
    let bearer = Bearer::generate();
    let tenant_id = Uuid::new_v4();
    let (_, app) = build_app_from_dbs(
        expired_validation_db(&bearer, tenant_id),
        missing_tenant_db(),
    );
    let captured = send_auth_check(
        &app,
        Some(&bearer_header(&URL_SAFE_NO_PAD.encode(bearer.as_bytes()))),
        "/api/tenant/acme/secrets",
    )
    .await;
    assert_structured_error(&captured, StatusCode::UNAUTHORIZED, "expired_lease");
}

// ===========================================================================
// P27 — login_start with unknown tenant (endpoints.rs line 648)
// ===========================================================================

/// `login_start_inner`: `lookup_tenant_id_by_name` returns `Ok(None)` (tenant
/// unknown) → covers the `Ok(None) => (None, None)` arm (endpoints.rs line
/// 648). The server proceeds with the dummy OPAQUE flow (enumeration
/// resistance) and returns 200 with `handshake_id` + `login_response`.
#[tokio::test]
async fn login_start_unknown_tenant_returns_200_via_dummy_opaque_flow() {
    let mut rng = rand::thread_rng();
    let password = vec![0x01u8; 32];
    let setup = ServerSetup::generate(&mut rng);

    let started = client::login_start(&mut rng, &password).expect("client login_start");
    let login_request_b64 = URL_SAFE_NO_PAD.encode(started.request.serialize());

    // Empty tenant store: "unknown-tenant" is not registered.
    let auth = AuthState::from_stores(
        Arc::new(MockLeaseStore::new()),
        Arc::new(MockTenantStore::new()),
        Arc::new(MockPasswordFileStore::new()),
        setup,
    );
    let state = build_state(auth);
    let app = build_router(state);

    let captured = send_login(
        &app,
        json!({
            "opaque_login_request": login_request_b64,
            "tenant": "unknown-tenant"
        }),
    )
    .await;
    // OPAQUE enumeration resistance: unknown tenant returns 200 with a
    // dummy handshake indistinguishable from a real one on the wire.
    assert_eq!(captured.status, StatusCode::OK);
    let body = captured.json.as_ref().expect("json body");
    assert!(
        body["handshake_id"].is_string(),
        "body={}",
        captured.raw_body
    );
    assert!(
        body["opaque_login_response"].is_string(),
        "body={}",
        captured.raw_body
    );
}

// ===========================================================================
// P28 — login_start with password-file DB error (endpoints.rs lines 643-645)
// ===========================================================================

/// `login_start_inner`: tenant is found but `load_password_file` returns
/// `Err` → covers the inner-match `Err(err)` arm (endpoints.rs lines
/// 643-645). The handler returns 500 `internal`.
#[tokio::test]
async fn login_start_password_file_db_error_returns_500() {
    let mut rng = rand::thread_rng();
    let password = vec![0x01u8; 32];
    let setup = ServerSetup::generate(&mut rng);
    let tenant_id = Uuid::new_v4();

    let started = client::login_start(&mut rng, &password).expect("client login_start");
    let login_request_b64 = URL_SAFE_NO_PAD.encode(started.request.serialize());

    let auth = AuthState::from_stores(
        Arc::new(MockLeaseStore::new()),
        Arc::new(MockTenantStore::with_tenant("acme", tenant_id)),
        // Always-error store: load_password_file returns Err for any tenant.
        Arc::new(MockPasswordFileStore::always_error("db connection lost")),
        setup,
    );
    let state = build_state(auth);
    let app = build_router(state);

    let captured = send_login(
        &app,
        json!({
            "opaque_login_request": login_request_b64,
            "tenant": "acme"
        }),
    )
    .await;
    assert_api_error(&captured, StatusCode::INTERNAL_SERVER_ERROR, "internal");
}
