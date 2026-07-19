//! `auth::endpoints` — HTTP handlers for OPAQUE registration + login.
//!
//! Mounted by [`crate::handler::build_router`] under `/auth/`:
//!
//! | Endpoint                  | Verb | Purpose                                                           |
//! |---------------------------|------|-------------------------------------------------------------------|
//! | `/auth/register/start`    | POST | Begin OPAQUE registration; returns `RegistrationResponse` bytes.  |
//! | `/auth/register/finish`   | POST | Finalise registration; persists the `PasswordFile`.               |
//! | `/auth/login/start`       | POST | Begin OPAQUE login; stashes `ServerLoginState` keyed by UUID.     |
//! | `/auth/login/finish`      | POST | Finalise login; INSERTs a `lease` row and returns the bearer.     |
//!
//! All four endpoints exchange OPAQUE wire messages as
//! url-safe-base64 (no-pad) JSON fields, so the on-wire payload is
//! pure JSON and a `bw` CLI doesn't have to negotiate any
//! special content type. Constants in [`crate::error_response`] cover
//! the 401 wire shape from #125; the 400 / 404 / 409 / 500 paths use
//! a lightweight envelope local to this module so the JSON-only
//! callers don't see two contradictory shapes for non-`/auth/check`
//! errors.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use botwork_opaque_handshake::{
    server, LoginFinalization, LoginRequest, OpaqueError, RegistrationRequest, RegistrationUpload,
    SUITE_VERSION,
};
use chrono::Utc;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tracing::{info, warn};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::auth::{
    lease::{Bearer, WrappedExportKey, LEASE_DEFAULT_SECONDS},
    lease_kek::wrap_session_key,
    opaque::UpsertError,
    pending::{Pending, PendingError, PendingMap},
    rate_limit::{RateLimitConfig, RateLimiter},
};
use crate::store::{LeaseStore, PasswordFileStore, TenantStore};

const PREFIX: &str = "[auth-broker]";

/// Sentinel tenant key used for the `/auth/login/finish` rate-limit bucket.
///
/// `login_finish` carries no tenant in the request body (only a
/// `handshake_id`), so the per-`(tenant, IP)` key cannot be derived from
/// the request. This sentinel separates `login_finish` buckets from the
/// per-`(tenant, IP)` buckets of the other three endpoints. It is never a
/// valid tenant name: tenant names must match `^[A-Za-z0-9_-]{1,63}$`,
/// so an empty string is structurally distinct from any real tenant.
const RATE_LIMIT_NO_TENANT_SENTINEL: &str = "";

/// Maximum age of a cached OPAQUE export key. Once an entry has not
/// been refreshed for this long it is eligible for eviction by
/// [`AuthState::sweep_lease_export_keys`] and removed lazily by
/// [`AuthState::lease_export_key`]. Five minutes mirrors the
/// `IDLE_TTL` used for the main cache layer — an active lease
/// continuously refreshes its entry on every bearer validation,
/// so the window only opens once no `/auth/check` has arrived.
pub const LEASE_EXPORT_KEY_TTL: Duration = Duration::from_secs(5 * 60);

/// Inner type of [`AuthState::lease_export_keys`]: each entry pairs the
/// `Instant` the key was last written with the `Zeroizing` key bytes.
type LeaseExportKeyMap = Arc<Mutex<HashMap<Uuid, (Instant, Zeroizing<Vec<u8>>)>>>;

/// State carried alongside the existing `AppState` for the four new
/// `/auth/*` endpoints. Kept as an `Arc<…>` so the axum
/// `State(...)` extractor stays `Clone` and `Send + Sync` without
/// the caller having to wrap each field individually.
///
/// The three store fields abstract every DB call so tests can inject
/// mock implementations (see [`crate::store::mock`]) without Docker.
#[derive(Clone)]
pub struct AuthState {
    pub lease_store: Arc<dyn LeaseStore + Send + Sync>,
    pub tenant_store: Arc<dyn TenantStore + Send + Sync>,
    pub password_file_store: Arc<dyn PasswordFileStore + Send + Sync>,
    pub setup: Arc<botwork_opaque_handshake::ServerSetup>,
    pub pending: PendingMap,
    /// Per-lease OPAQUE export-key cache. Each entry carries the
    /// `Instant` it was last written so [`AuthState::sweep_lease_export_keys`]
    /// can evict stale entries. Values are wrapped in [`Zeroizing`]
    /// so memory is scrubbed on eviction.
    pub lease_export_keys: LeaseExportKeyMap,
    /// Per-`(tenant, source-IP)` token-bucket rate limiter applied to
    /// the four OPAQUE auth endpoints. Constructed with
    /// [`RateLimitConfig::disabled`] by default so the in-process test
    /// harness is unaffected; the production binary sets the config from
    /// environment variables via [`AuthState::with_rate_limiter`].
    pub rate_limiter: RateLimiter,
}

impl AuthState {
    /// Production constructor: wraps a [`DatabaseConnection`] in the
    /// three SeaORM-backed store implementations.
    pub fn new(db: DatabaseConnection, setup: botwork_opaque_handshake::ServerSetup) -> Self {
        use crate::store::sea_orm_impl::{
            SeaOrmLeaseStore, SeaOrmPasswordFileStore, SeaOrmTenantStore,
        };
        // Wrap in Arc once so all three stores share the same underlying
        // connection without requiring DatabaseConnection: Clone.  (sea-orm's
        // `mock` feature removes the Clone impl from DatabaseConnection, so
        // using Arc-clone is the compatible path for both prod and test builds.)
        let db = Arc::new(db);
        Self::from_stores(
            Arc::new(SeaOrmLeaseStore::new_shared(Arc::clone(&db))),
            Arc::new(SeaOrmTenantStore::new_shared(Arc::clone(&db))),
            Arc::new(SeaOrmPasswordFileStore::new_shared(db)),
            setup,
        )
    }

    /// Like [`AuthState::new`] but accepts a pre-shared
    /// `Arc<DatabaseConnection>`.
    ///
    /// This is useful in integration-test harnesses that need to keep an
    /// independent reference to the same connection (e.g. for seeding data)
    /// while also handing it to `AuthState`.  Using `Arc` avoids the need
    /// for `DatabaseConnection: Clone`, which sea-orm removes when its
    /// `mock` feature is enabled.
    pub fn new_arc(
        db: Arc<DatabaseConnection>,
        setup: botwork_opaque_handshake::ServerSetup,
    ) -> Self {
        use crate::store::sea_orm_impl::{
            SeaOrmLeaseStore, SeaOrmPasswordFileStore, SeaOrmTenantStore,
        };
        Self::from_stores(
            Arc::new(SeaOrmLeaseStore::new_shared(Arc::clone(&db))),
            Arc::new(SeaOrmTenantStore::new_shared(Arc::clone(&db))),
            Arc::new(SeaOrmPasswordFileStore::new_shared(db)),
            setup,
        )
    }

    /// Construct from explicit store implementations.
    ///
    /// Useful for injecting mock stores in tests (see
    /// [`crate::store::mock`]) or for integration harnesses that need
    /// direct store control.
    ///
    /// Rate limiting is **disabled** by default so the in-process test
    /// harness is unaffected. Call [`AuthState::with_rate_limiter`] to
    /// enable it (the production binary does this from env config).
    pub fn from_stores(
        lease_store: Arc<dyn LeaseStore + Send + Sync>,
        tenant_store: Arc<dyn TenantStore + Send + Sync>,
        password_file_store: Arc<dyn PasswordFileStore + Send + Sync>,
        setup: botwork_opaque_handshake::ServerSetup,
    ) -> Self {
        Self {
            lease_store,
            tenant_store,
            password_file_store,
            setup: Arc::new(setup),
            pending: PendingMap::new(),
            lease_export_keys: Arc::new(Mutex::new(HashMap::new())),
            rate_limiter: RateLimiter::new(RateLimitConfig::disabled()),
        }
    }

    /// Builder-style setter for the rate limiter config. Replaces the
    /// default disabled limiter with one driven by the supplied config.
    /// The production binary calls this after reading env vars.
    pub fn with_rate_limiter(mut self, config: RateLimitConfig) -> Self {
        self.rate_limiter = RateLimiter::new(config);
        self
    }

    pub async fn remember_lease_export_key(&self, lease_id: Uuid, export_key: &[u8]) {
        self.lease_export_keys.lock().await.insert(
            lease_id,
            (Instant::now(), Zeroizing::new(export_key.to_vec())),
        );
    }

    /// Retrieve the export key for `lease_id`. Returns `None` when no
    /// entry exists **or** when the entry is older than
    /// [`LEASE_EXPORT_KEY_TTL`] (the stale entry is evicted in-place
    /// so memory is reclaimed without waiting for the next sweep).
    pub async fn lease_export_key(&self, lease_id: Uuid) -> Option<Zeroizing<Vec<u8>>> {
        let mut map = self.lease_export_keys.lock().await;
        let now = Instant::now();
        // Resolve the result in one `get()` pass. The closure consumes the
        // borrow so the subsequent `map.remove()` compiles without a
        // second lookup.
        let outcome = map.get(&lease_id).map(|(written_at, key)| {
            if now.saturating_duration_since(*written_at) > LEASE_EXPORT_KEY_TTL {
                Err(())
            } else {
                Ok(key.clone())
            }
        });
        match outcome {
            None => None,
            Some(Ok(key)) => Some(key),
            Some(Err(())) => {
                map.remove(&lease_id);
                None
            }
        }
    }

    /// Evict all export-key entries older than [`LEASE_EXPORT_KEY_TTL`].
    /// Returns the number of entries removed. Called from the background
    /// prune task (via [`crate::cache::prune_once`]) so the map does not
    /// grow without bound across continuous logins.
    ///
    /// `Zeroizing` drop semantics ensure the key bytes are scrubbed from
    /// memory as each evicted entry is dropped.
    pub async fn sweep_lease_export_keys(&self, now: Instant) -> usize {
        let mut map = self.lease_export_keys.lock().await;
        let before = map.len();
        map.retain(|_, (written_at, _)| {
            now.saturating_duration_since(*written_at) <= LEASE_EXPORT_KEY_TTL
        });
        before.saturating_sub(map.len())
    }

    /// Length of the export-key map. Only compiled under `test` /
    /// `test-support` — production callers never need to ask.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn lease_export_key_count(&self) -> usize {
        self.lease_export_keys.lock().await.len()
    }
}

/// Build the `/auth/{register,login}/{start,finish}` router. The
/// caller composes this into the top-level router that also serves
/// `/auth/check` and `/secrets/fetch`.
pub fn build_auth_router(state: AuthState) -> Router {
    Router::new()
        .route("/auth/register/start", post(register_start))
        .route("/auth/register/finish", post(register_finish))
        .route("/auth/login/start", post(login_start))
        .route("/auth/login/finish", post(login_finish))
        .with_state(state)
}

pub(crate) use LoginFinishRequest as SharedLoginFinishRequest;
pub(crate) use LoginStartRequest as SharedLoginStartRequest;

// ---------------------------------------------------------------------------
// Shared error envelope
// ---------------------------------------------------------------------------

/// Non-401 error envelope. The 401 path goes through
/// [`crate::error_response::unauthorized`] to preserve the #125 wire
/// contract. The shape here is intentionally similar but minimal:
/// `error.code` + `error.message`, no `WWW-Authenticate` (that
/// header is HTTP-Bearer-specific).
#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

fn json_error(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorEnvelope {
            error: ErrorBody {
                code,
                message: message.into(),
            },
        }),
    )
        .into_response()
}

fn bad_request(message: impl Into<String>) -> Response {
    json_error(StatusCode::BAD_REQUEST, "bad_request", message)
}

fn not_found(message: impl Into<String>) -> Response {
    json_error(StatusCode::NOT_FOUND, "not_found", message)
}

fn conflict(message: impl Into<String>) -> Response {
    json_error(StatusCode::CONFLICT, "conflict", message)
}

fn internal(message: impl Into<String>) -> Response {
    json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
}

/// `429 Too Many Requests` response with `Retry-After` header.
///
/// The same structured error envelope is used for all four endpoints
/// so rate-limited responses are indistinguishable by shape from other
/// error responses.
fn too_many_requests(retry_after: Duration) -> Response {
    let secs = retry_after.as_secs().max(1);
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(axum::http::header::RETRY_AFTER, secs.to_string())],
        Json(ErrorEnvelope {
            error: ErrorBody {
                code: "rate_limited",
                message: format!("rate limit exceeded; retry after {secs}s"),
            },
        }),
    )
        .into_response()
}

/// Extract the best-effort client IP address from request headers.
///
/// Checks `x-forwarded-for` first (taking the left-most / original-client
/// entry), then `x-real-ip`. Falls back to `"unknown"` when neither
/// header is present. The result is used as part of the rate-limit key
/// and is never returned to the caller.
fn extract_client_ip(headers: &HeaderMap) -> String {
    // x-forwarded-for: may be "client, proxy1, proxy2"; we want the first.
    if let Some(xff) = headers.get("x-forwarded-for") {
        if let Ok(s) = xff.to_str() {
            if let Some(first) = s.split(',').next() {
                let ip = first.trim();
                if !ip.is_empty() {
                    return ip.to_string();
                }
            }
        }
    }
    // x-real-ip: set by Nginx-style proxies.
    if let Some(xri) = headers.get("x-real-ip") {
        if let Ok(s) = xri.to_str() {
            let ip = s.trim();
            if !ip.is_empty() {
                return ip.to_string();
            }
        }
    }
    "unknown".to_string()
}

// `Result<Vec<u8>, Box<Response>>` instead of `Result<Vec<u8>, Response>`:
// `axum::http::Response` is a 128-byte enum (clippy::result_large_err
// would flag a non-boxed `Err` and refuse the strict `-D warnings` CI
// gate). Boxing the error variant keeps the function's `Result` shape
// small on the hot path while still letting the caller `?` it.
fn b64_decode(field: &str, value: &str) -> Result<Vec<u8>, Box<Response>> {
    URL_SAFE_NO_PAD.decode(value).map_err(|_| {
        Box::new(bad_request(format!(
            "`{field}` is not valid url-safe-base64"
        )))
    })
}

// ---------------------------------------------------------------------------
// /auth/register/start
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RegisterStartRequest {
    tenant: String,
    credential_identifier: String,
    registration_request: String,
}

#[derive(Debug, Serialize)]
struct RegisterStartResponse {
    registration_response: String,
}

async fn register_start(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(body): Json<RegisterStartRequest>,
) -> Response {
    let ip = extract_client_ip(&headers);
    if let Err(retry_after) = state
        .rate_limiter
        .check(&body.tenant, &ip, Instant::now())
        .await
    {
        return too_many_requests(retry_after);
    }

    let tenant_id = match state
        .tenant_store
        .lookup_tenant_id_by_name(&body.tenant)
        .await
    {
        Ok(Some(id)) => id,
        Ok(None) => {
            // Registration explicitly DOES 404 on unknown tenants —
            // it's an operator-only flow today and the enumeration
            // concern that drives the login dummy-flow doesn't
            // apply.
            warn!(
                "{PREFIX} auth/register/start: rejected — unknown tenant={}",
                body.tenant
            );
            return not_found(format!("unknown tenant '{}'", body.tenant));
        }
        Err(err) => {
            warn!("{PREFIX} auth/register/start: db error: {err}");
            return internal(format!("database error: {err}"));
        }
    };

    let req_bytes = match b64_decode("registration_request", &body.registration_request) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let request = match RegistrationRequest::deserialize(&req_bytes) {
        Ok(r) => r,
        Err(OpaqueError::Serialization(detail)) => {
            return bad_request(format!("malformed `registration_request`: {detail}"));
        }
        Err(err) => {
            warn!("{PREFIX} auth/register/start: opaque error: {err}");
            return internal(format!("opaque error: {err}"));
        }
    };

    let started = match server::registration_start(
        &state.setup,
        request,
        body.credential_identifier.as_bytes(),
    ) {
        Ok(s) => s,
        Err(err) => {
            warn!("{PREFIX} auth/register/start: opaque server error: {err}");
            return internal(format!("opaque server error: {err}"));
        }
    };

    info!(
        "{PREFIX} auth/register/start: ok tenant={} tenant_id={tenant_id}",
        body.tenant
    );
    Json(RegisterStartResponse {
        registration_response: URL_SAFE_NO_PAD.encode(started.response.serialize()),
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// /auth/register/finish
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RegisterFinishRequest {
    tenant: String,
    #[serde(default, rename = "credential_identifier")]
    _credential_identifier: Option<String>,
    registration_upload: String,
}

#[derive(Debug, Serialize)]
struct RegisterFinishResponse {
    tenant: String,
    suite_version: u8,
}

async fn register_finish(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(body): Json<RegisterFinishRequest>,
) -> Response {
    let ip = extract_client_ip(&headers);
    if let Err(retry_after) = state
        .rate_limiter
        .check(&body.tenant, &ip, Instant::now())
        .await
    {
        return too_many_requests(retry_after);
    }

    let tenant_id = match state
        .tenant_store
        .lookup_tenant_id_by_name(&body.tenant)
        .await
    {
        Ok(Some(id)) => id,
        Ok(None) => {
            warn!(
                "{PREFIX} auth/register/finish: rejected — unknown tenant={}",
                body.tenant
            );
            return not_found(format!("unknown tenant '{}'", body.tenant));
        }
        Err(err) => {
            warn!("{PREFIX} auth/register/finish: db error: {err}");
            return internal(format!("database error: {err}"));
        }
    };

    let upload_bytes = match b64_decode("registration_upload", &body.registration_upload) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let upload = match RegistrationUpload::deserialize(&upload_bytes) {
        Ok(u) => u,
        Err(OpaqueError::Serialization(detail)) => {
            return bad_request(format!("malformed `registration_upload`: {detail}"));
        }
        Err(err) => {
            warn!("{PREFIX} auth/register/finish: opaque error: {err}");
            return internal(format!("opaque error: {err}"));
        }
    };

    let password_file = server::registration_finish(upload);

    match state
        .password_file_store
        .upsert_password_file(tenant_id, &password_file, SUITE_VERSION as i32)
        .await
    {
        Ok(()) => {
            info!(
                "{PREFIX} auth/register/finish: ok tenant={} tenant_id={tenant_id}",
                body.tenant
            );
            (
                StatusCode::CREATED,
                Json(RegisterFinishResponse {
                    tenant: body.tenant,
                    suite_version: SUITE_VERSION,
                }),
            )
                .into_response()
        }
        Err(UpsertError::Conflict) => {
            warn!(
                "{PREFIX} auth/register/finish: rejected — password file already exists for tenant={}",
                body.tenant
            );
            conflict(format!(
                "a password file already exists for tenant '{}'",
                body.tenant
            ))
        }
        Err(UpsertError::Db(err)) => {
            warn!("{PREFIX} auth/register/finish: db error: {err}");
            internal(format!("database error: {err}"))
        }
    }
}

// ---------------------------------------------------------------------------
// /auth/login/start
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct LoginStartRequest {
    pub(crate) tenant: String,
    pub(crate) credential_identifier: String,
    pub(crate) login_request: String,
    #[serde(default)]
    pub(crate) lease_seconds_requested: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct LoginStartResponse {
    pub(crate) handshake_id: Uuid,
    pub(crate) login_response: String,
    pub(crate) expires_in_seconds: u64,
}

async fn login_start(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(body): Json<LoginStartRequest>,
) -> Response {
    let ip = extract_client_ip(&headers);
    if let Err(retry_after) = state
        .rate_limiter
        .check(&body.tenant, &ip, Instant::now())
        .await
    {
        return too_many_requests(retry_after);
    }
    match login_start_inner(&state, body).await {
        Ok(response) => Json(response).into_response(),
        Err(response) => response,
    }
}

pub(crate) async fn login_start_inner(
    state: &AuthState,
    body: LoginStartRequest,
) -> Result<LoginStartResponse, Response> {
    login_start_inner_with_handshake_id(state, body, Uuid::new_v4()).await
}

async fn login_start_inner_with_handshake_id(
    state: &AuthState,
    body: LoginStartRequest,
    handshake_id: Uuid,
) -> Result<LoginStartResponse, Response> {
    let req_bytes = match b64_decode("login_request", &body.login_request) {
        Ok(b) => b,
        Err(resp) => return Err(*resp),
    };
    let request = match LoginRequest::deserialize(&req_bytes) {
        Ok(r) => r,
        Err(OpaqueError::Serialization(detail)) => {
            return Err(bad_request(format!("malformed `login_request`: {detail}")));
        }
        Err(err) => {
            warn!("{PREFIX} auth/login/start: opaque error: {err}");
            return Err(internal(format!("opaque error: {err}")));
        }
    };

    // OPAQUE enumeration resistance: unknown tenants AND known
    // tenants without a password_file go through the dummy flow so
    // the wire-observable response shape is identical to a real
    // tenant's. We carry a *placeholder* tenant_id of nil into the
    // pending map for the dummy case — `/auth/login/finish` will
    // refuse to mint a lease against tenant_id=nil because the FK
    // on lease.tenant_id rejects it, surfacing as
    // `OpaqueError::InvalidLogin` on the wire. That matches what a
    // wrong-password attempt against a real tenant produces.
    let (tenant_id, password_file) = match state
        .tenant_store
        .lookup_tenant_id_by_name(&body.tenant)
        .await
    {
        Ok(Some(id)) => match state.password_file_store.load_password_file(id).await {
            Ok(pf) => (Some(id), pf),
            Err(err) => {
                warn!("{PREFIX} auth/login/start: db error reading password_file: {err}");
                return Err(internal(format!("database error: {err}")));
            }
        },
        Ok(None) => (None, None),
        Err(err) => {
            warn!("{PREFIX} auth/login/start: db error looking up tenant: {err}");
            return Err(internal(format!("database error: {err}")));
        }
    };

    let started = match server::login_start(
        &mut rand::thread_rng(),
        &state.setup,
        password_file.as_ref(),
        request,
        body.credential_identifier.as_bytes(),
    ) {
        Ok(s) => s,
        Err(err) => {
            warn!("{PREFIX} auth/login/start: opaque server error: {err}");
            return Err(internal(format!("opaque server error: {err}")));
        }
    };

    let lease_seconds = body
        .lease_seconds_requested
        .unwrap_or(LEASE_DEFAULT_SECONDS);

    // Stash the state keyed by handshake_id. The pending map owns
    // the serialised form so the typed ServerLoginState can be
    // dropped here (its bytes live on inside the Zeroizing pending-map
    // pending map slot).
    let pending = Pending {
        // `tenant_id` is `None` when the request drove the OPAQUE
        // dummy flow (unknown tenant or tenant without a
        // password_file row). `/auth/login/finish` will refuse to
        // mint a lease for `None` and return `InvalidBearer` — same
        // wire shape as wrong-password against a real tenant.
        tenant_id,
        state: started.state,
        lease_seconds_requested: lease_seconds,
        created_at: Instant::now(),
    };
    match state.pending.insert(handshake_id, pending).await {
        Ok(()) => {}
        Err(PendingError::Collision) => {
            // UUIDv4 collision means our RNG is broken — 500.
            return Err(internal("handshake_id collision (RNG failure?)"));
        }
        // The other arms (NotFound / Expired) can't fire on insert;
        // exhaustive match for completeness.
        Err(err) => return Err(internal(format!("pending map error: {err}"))),
    }

    let known_tenant = tenant_id.is_some();
    info!(
        "{PREFIX} auth/login/start: ok tenant={} known={known_tenant} handshake_id={handshake_id}",
        body.tenant,
    );
    Ok(LoginStartResponse {
        handshake_id,
        login_response: URL_SAFE_NO_PAD.encode(started.response.serialize()),
        expires_in_seconds: crate::auth::pending::PENDING_TTL.as_secs(),
    })
}

// ---------------------------------------------------------------------------
// /auth/login/finish
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct LoginFinishRequest {
    pub(crate) handshake_id: Uuid,
    pub(crate) login_finalization: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct LoginFinishResponse {
    pub(crate) bearer: String,
    pub(crate) expires_at: chrono::DateTime<chrono::Utc>,
    pub(crate) lease_id: Uuid,
}

async fn login_finish(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Json(body): Json<LoginFinishRequest>,
) -> Response {
    let ip = extract_client_ip(&headers);
    // `login_finish` has no tenant in the body; use an empty-string tenant
    // sentinel so the bucket is keyed per-IP without conflating it with
    // per-(tenant,IP) buckets from `login_start` / `register_*`.
    if let Err(retry_after) = state
        .rate_limiter
        .check(RATE_LIMIT_NO_TENANT_SENTINEL, &ip, Instant::now())
        .await
    {
        return too_many_requests(retry_after);
    }
    match login_finish_inner(&state, body).await {
        Ok(response) => Json(response).into_response(),
        Err(response) => response,
    }
}

pub(crate) async fn login_finish_inner(
    state: &AuthState,
    body: LoginFinishRequest,
) -> Result<LoginFinishResponse, Response> {
    let taken = match state.pending.take(body.handshake_id, Instant::now()).await {
        Ok(t) => t,
        Err(PendingError::NotFound) | Err(PendingError::Expired) => {
            // Same HTTP shape (404) regardless of which arm tripped:
            // surfaces uniformly as "go run login/start again".
            return Err(not_found("unknown or expired handshake_id"));
        }
        Err(PendingError::Collision) => {
            // `take` cannot produce Collision; placate the
            // exhaustive match without leaking the variant name.
            return Err(internal("pending map invariant violated"));
        }
    };

    let final_bytes = match b64_decode("login_finalization", &body.login_finalization) {
        Ok(b) => b,
        Err(resp) => return Err(*resp),
    };
    let finalization = match LoginFinalization::deserialize(&final_bytes) {
        Ok(f) => f,
        Err(OpaqueError::Serialization(detail)) => {
            return Err(bad_request(format!(
                "malformed `login_finalization`: {detail}"
            )));
        }
        Err(err) => {
            warn!("{PREFIX} auth/login/finish: opaque error: {err}");
            return Err(internal(format!("opaque error: {err}")));
        }
    };

    // Compute the OPAQUE mutual session key. This is the value we
    // wrap into `lease.wrapped_export_key` (see below for why the
    // OPAQUE *export* key isn't usable server-side).
    let session_key = match server::login_finish(taken.state, finalization) {
        Ok(sk) => sk,
        Err(OpaqueError::InvalidLogin) => {
            warn!(
                "{PREFIX} auth/login/finish: invalid login handshake_id={}",
                body.handshake_id
            );
            return Err(crate::error_response::unauthorized(
                crate::error_response::ErrorCode::InvalidBearer,
                None,
            ));
        }
        Err(err) => {
            warn!("{PREFIX} auth/login/finish: opaque server error: {err}");
            return Err(internal(format!("opaque server error: {err}")));
        }
    };

    // The pending map carries the tenant_id captured at start time.
    // `None` means start was driven through the OPAQUE dummy flow
    // (unknown tenant or known tenant without a `password_file`
    // row). Treat that as InvalidBearer so the wire shape mirrors
    // wrong-password against a real tenant — the dummy detection
    // lives in the `Option` type, not in a `Uuid::nil()` sentinel.
    let Some(tenant_id) = taken.tenant_id else {
        warn!(
            "{PREFIX} auth/login/finish: dummy-flow attempt completed handshake_id={}",
            body.handshake_id
        );
        return Err(crate::error_response::unauthorized(
            crate::error_response::ErrorCode::InvalidBearer,
            None,
        ));
    };

    // Mint the bearer + wrap the per-lease unlock secret.
    //
    // ## Why we wrap the mutual SessionKey, not the client's ExportKey
    //
    // OPAQUE's `ExportKey` is *client-only* by construction — the
    // server never observes it, and asking the client to send it
    // back would defeat the whole point of OPAQUE (the server is
    // supposed to be vault-blind without the user's password). The
    // value we *do* hold here is the mutual `SessionKey`, which:
    //
    //   * is mutually authenticated — both ends derive identical
    //     bytes from the OPAQUE handshake;
    //   * is fresh per login — re-login mints a new bearer, a new
    //     lease row, and a new SessionKey;
    //   * is never carried on the wire — the client doesn't have
    //     to surface it back to us after this point;
    //   * is exactly the right value to seal into the per-lease
    //     `wrapped_export_key` slot, because the *function* the
    //     column serves is "per-lease unlock secret bound to
    //     postgres only via the wrapping key".
    //
    // The schema column name `wrapped_export_key` is kept verbatim
    // because the contract (a wrapped per-lease secret) is
    // unchanged. The OPAQUE-specific naming is acceptable
    // semantic drift; renaming would force a cross-repo schema
    // migration for cosmetics only.
    //
    // ## KEK derivation: per-bearer, server-stateless
    //
    // As of this PR the KEK that seals `wrapped_export_key` is
    // HKDF'd from the bearer itself (see `auth::lease_kek`). The
    // bearer we just minted via `Bearer::generate()` is the only
    // input. After wrap, the bearer is base64-encoded onto the
    // response and the raw bytes drop out of scope here; nothing in
    // the broker holds onto them after this handler returns.
    //
    // This is the property that makes the broker restart-safe and
    // the operator unable to mass-decrypt postgres dumps: the
    // server has no persistent or process-local key material that
    // unwraps lease rows. Only a request carrying the live bearer
    // can unwrap that lease's session key.
    //
    // SECURITY.md captures the SessionKey-vs-ExportKey distinction
    // in detail.
    let bearer = Bearer::generate();
    let bearer_hash = bearer.hash();

    let wrapped = WrappedExportKey(wrap_session_key(bearer.as_bytes(), session_key.as_bytes()));

    let now = Utc::now();
    let row = match state
        .lease_store
        .insert_lease(
            tenant_id,
            &bearer_hash,
            &wrapped,
            taken.lease_seconds_requested,
            now,
        )
        .await
    {
        Ok(row) => row,
        Err(err) => {
            warn!("{PREFIX} auth/login/finish: db error inserting lease: {err}");
            return Err(internal(format!("database error: {err}")));
        }
    };

    info!(
        "{PREFIX} auth/login/finish: ok tenant_id={} lease_id={} expires_at={}",
        tenant_id, row.id, row.expires_at,
    );
    state
        .remember_lease_export_key(row.id, session_key.as_bytes())
        .await;
    Ok(LoginFinishResponse {
        bearer: URL_SAFE_NO_PAD.encode(bearer.as_bytes()),
        expires_at: row.expires_at,
        lease_id: row.id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::store::mock::{MockLeaseStore, MockPasswordFileStore, MockTenantStore};
    use axum::body::to_bytes;
    use axum::http::HeaderValue;
    use botwork_opaque_handshake::{client, server, PasswordFile};

    #[test]
    fn json_error_shape_is_stable() {
        // Pin the JSON-only error envelope so a future refactor
        // can't silently collapse it back to the bare string body
        // it replaced. The 401 path has its own structured contract
        // (see #125 and `error_response.rs`); this is the 400/404/
        // 409/500 surface.
        let resp = bad_request("bad");
        let (parts, _body) = resp.into_parts();
        assert_eq!(parts.status, StatusCode::BAD_REQUEST);
    }

    fn make_password_file(
        setup: &botwork_opaque_handshake::ServerSetup,
        password: &[u8],
    ) -> PasswordFile {
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

    async fn response_json(response: Response) -> serde_json::Value {
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        serde_json::from_slice(&body).expect("json body")
    }

    #[tokio::test]
    async fn login_start_collision_maps_to_500() {
        let tenant_id = Uuid::new_v4();
        let mut rng_pw = rand::thread_rng();
        let mut pw_bytes = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rng_pw, &mut pw_bytes);
        let password = &pw_bytes[..];
        let mut rng = rand::thread_rng();
        let setup = botwork_opaque_handshake::ServerSetup::generate(&mut rng);
        let password_file = make_password_file(&setup, password);
        let login = client::login_start(&mut rng, password).expect("client login_start");
        let preseeded = client::login_start(&mut rng, password).expect("preseeded login_start");
        let preseeded_server = server::login_start(
            &mut rng,
            &setup,
            Some(&password_file),
            preseeded.request,
            b"alice",
        )
        .expect("preseeded server login_start");

        let state = AuthState::from_stores(
            Arc::new(MockLeaseStore::new()),
            Arc::new(MockTenantStore::with_tenant("acme", tenant_id)),
            Arc::new(MockPasswordFileStore::with_file(tenant_id, &password_file)),
            setup,
        );
        let handshake_id = Uuid::new_v4();
        state
            .pending
            .insert(
                handshake_id,
                Pending {
                    tenant_id: Some(tenant_id),
                    state: preseeded_server.state,
                    lease_seconds_requested: LEASE_DEFAULT_SECONDS,
                    created_at: Instant::now(),
                },
            )
            .await
            .expect("preseed pending");

        let response = login_start_inner_with_handshake_id(
            &state,
            LoginStartRequest {
                tenant: "acme".to_string(),
                credential_identifier: "alice".to_string(),
                login_request: URL_SAFE_NO_PAD.encode(login.request.serialize()),
                lease_seconds_requested: None,
            },
            handshake_id,
        )
        .await
        .expect_err("collision must return a response");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = response_json(response).await;
        assert_eq!(body["error"]["code"], "internal");
        assert_eq!(
            body["error"]["message"],
            "handshake_id collision (RNG failure?)"
        );
    }

    #[test]
    fn extract_client_ip_prefers_single_x_forwarded_for() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.10"));
        assert_eq!(extract_client_ip(&headers), "203.0.113.10");
    }

    #[test]
    fn extract_client_ip_uses_first_proxy_hop() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("198.51.100.1, 198.51.100.2, 198.51.100.3"),
        );
        assert_eq!(extract_client_ip(&headers), "198.51.100.1");
    }

    #[test]
    fn extract_client_ip_falls_back_to_x_real_ip() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", HeaderValue::from_static("192.0.2.5"));
        assert_eq!(extract_client_ip(&headers), "192.0.2.5");
    }

    #[test]
    fn extract_client_ip_defaults_to_unknown() {
        assert_eq!(extract_client_ip(&HeaderMap::new()), "unknown");
    }
}
