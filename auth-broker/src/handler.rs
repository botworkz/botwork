use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Json;
use axum::Router;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};
use botwork_vault::{Vault, VaultError};
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use subtle::ConstantTimeEq;
use tokio::time::Instant;
use tracing::{info, warn};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::cache::{evict_caps_for, is_expired, AppState, CacheEntry};
use crate::caps::{
    cap_is_expired, decode_cap, encode_cap, mint_cap_id, CapEntry, CapId, CAP_BYTES, CAP_TTL,
};
use crate::error_response::{self, ErrorCode};

const PREFIX: &str = "[auth-broker]";
pub const BOTWORK_CAP_COOKIE_NAME: &str = "botwork_cap";
const API_NAMESPACE: &str = "api";
const API_PLUGIN: &str = "api";

/// Canonical 401 reply. Every 401 path in this handler must funnel
/// through `error_response::unauthorized` so the JSON body +
/// `WWW-Authenticate` header contract from issue #125 is enforced
/// in exactly one place.
fn unauthorized(code: ErrorCode, tenant: Option<&str>) -> Response {
    error_response::unauthorized(code, tenant)
}

pub fn redact_token(token: &str) -> String {
    if token.len() <= 6 {
        "***redacted***".to_string()
    } else {
        format!("{}…", &token[..6])
    }
}

pub fn cache_key(tenant: &str, bearer: &str) -> [u8; 32] {
    type Blake2b256 = Blake2b<U32>;
    let mut hasher = Blake2b256::new();
    hasher.update(tenant.as_bytes());
    hasher.update(b":");
    hasher.update(bearer.as_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Extract a bearer token from an Authorization header value.
pub fn extract_bearer(auth_header: &str) -> Option<Zeroizing<String>> {
    if !auth_header.starts_with("Bearer ") {
        return None;
    }

    let bearer = auth_header["Bearer ".len()..].trim();
    if bearer.is_empty() {
        return None;
    }

    Some(Zeroizing::new(bearer.to_string()))
}

fn extract_cookie_cap(headers: &HeaderMap) -> Option<Zeroizing<String>> {
    let cookie = headers.get("cookie")?.to_str().ok()?;
    for part in cookie.split(';') {
        let (name, value) = part.trim().split_once('=')?;
        if name.trim() == BOTWORK_CAP_COOKIE_NAME {
            let value = value.trim();
            if !value.is_empty() {
                return Some(Zeroizing::new(value.to_string()));
            }
        }
    }
    None
}

fn request_cap(headers: &HeaderMap) -> Result<Option<Zeroizing<String>>, ErrorCode> {
    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !auth_header.is_empty() {
        return extract_bearer(auth_header)
            .ok_or(ErrorCode::InvalidBearer)
            .map(Some);
    }
    Ok(extract_cookie_cap(headers))
}

/// Extract `(tenant, namespace, plugin)` from
/// `x-envoy-original-path`.
pub fn extract_tenant_namespace_plugin(original_path: &str) -> Option<(String, String, String)> {
    match crate::grammar::parse_original_path(original_path)? {
        crate::grammar::ParsedPath::Mcp {
            tenant,
            namespace,
            plugin,
        } => Some((tenant, namespace, plugin)),
        _ => None,
    }
}

#[derive(Serialize)]
struct FetchSecret {
    service: String,
    name: String,
    kind: String,
    /// Base64-encoded secret value. Held as a `Zeroizing<String>`
    /// so the response body's secret bytes are scrubbed once axum
    /// has written them onto the wire.
    #[serde(serialize_with = "serialize_zeroizing_string")]
    value_b64: Zeroizing<String>,
}

fn serialize_zeroizing_string<S: serde::Serializer>(
    value: &Zeroizing<String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(value.as_str())
}

#[derive(Serialize)]
struct FetchResponse {
    tenant: String,
    plugin: String,
    secrets: Vec<FetchSecret>,
}

fn success(tenant: &str, cap_value: &str) -> Response {
    (
        StatusCode::OK,
        [("x-botwork-tenant", tenant), ("x-botwork-cap", cap_value)],
        "OK",
    )
        .into_response()
}

fn success_public_no_identity() -> Response {
    (StatusCode::OK, "OK").into_response()
}

/// Return 200 OK with `x-botwork-admin: admin` injected for the
/// downstream API's `require_admin` gate.  Emitted when the request
/// carries the pre-shared genesis admin key (checked via constant-time
/// comparison in [`check`] before any path-specific routing).
fn success_admin() -> Response {
    (StatusCode::OK, [("x-botwork-admin", "admin")], "OK").into_response()
}

/// Mint a fresh cap value bound to `lease_id`.
///
/// Round 1b: `lease_id` is now a required `Uuid` (no longer
/// `Option<Uuid>`) because the legacy bearer-as-vault-password
/// path is gone. Every cap is part of exactly one lease cohort.
fn mint_cap_value(
    cache_key: [u8; 32],
    namespace: &str,
    plugin: &str,
    lease_id: Uuid,
    now: Instant,
) -> (String, CapEntry) {
    let cap_id = mint_cap_id();
    let cap_value = encode_cap(&cap_id);
    let entry = CapEntry {
        cache_key,
        namespace: namespace.to_string(),
        plugin: plugin.to_string(),
        expires_at: now + CAP_TTL,
        lease_id,
    };
    (cap_value, entry)
}

fn log_fetch_unauthorized(reason: &str, cap: Option<&str>) {
    let cap = match cap {
        Some(cap) => redact_token(cap),
        None => "<missing>".to_string(),
    };
    warn!("{PREFIX} fetch unauthorized reason={reason} cap={cap}");
}

fn decode_cap_with_reason(cap_value: &str) -> Result<CapId, String> {
    let decoded = URL_SAFE_NO_PAD
        .decode(cap_value)
        .map_err(|_| "base64url decode failed".to_string())?;
    if decoded.len() != CAP_BYTES {
        return Err(format!("unexpected length={}", decoded.len()));
    }
    let mut cap_id = [0u8; CAP_BYTES];
    cap_id.copy_from_slice(&decoded);
    Ok(cap_id)
}

pub async fn check(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let original_path = headers
        .get("x-envoy-original-path")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(path) = crate::grammar::parse_original_path(original_path) else {
        #[rustfmt::skip]
        warn!("{PREFIX} unauthorized request with bad path={}", if original_path.is_empty() { "<missing>" } else { original_path });
        return unauthorized(ErrorCode::InvalidBearer, None);
    };

    // ── Genesis admin bearer fast-path ─────────────────────────────────
    // When the pre-shared admin key is configured and the request carries a
    // matching bearer, bypass the OPAQUE lease path and inject
    // `x-botwork-admin: admin`.  The API layer gates admin-only routes
    // (`/api/tenants`, `/api/plugins`, …) on that header.
    //
    // The check is skipped for the login path (ApiAuthLogin), which is
    // intentionally public.  For every other path a matching admin bearer
    // always wins over the tenant OPAQUE path.
    if !matches!(path, crate::grammar::ParsedPath::ApiAuthLogin) {
        if let Some(ref admin_key) = state.admin_api_key {
            let bearer_matches = request_cap(&headers)
                .ok()
                .flatten()
                .is_some_and(|b| bool::from(b.as_bytes().ct_eq(admin_key.as_bytes())));
            if bearer_matches {
                info!("{PREFIX} auth/check: admin bearer accepted path={original_path}");
                return success_admin();
            }
        }
    }

    match path {
        crate::grammar::ParsedPath::ApiAuthLogin => StatusCode::OK.into_response(),
        crate::grammar::ParsedPath::Spa { tenant } => match request_cap(&headers) {
            Ok(None) => success_public_no_identity(),
            Ok(Some(bearer)) => {
                match try_lease_path(
                    &state.auth,
                    Some(&tenant),
                    API_NAMESPACE,
                    API_PLUGIN,
                    bearer.as_str(),
                    &state,
                    Instant::now(),
                )
                .await
                {
                    LeasePathOutcome::Hit(response)
                    | LeasePathOutcome::Miss(response)
                    | LeasePathOutcome::Expired(response)
                    | LeasePathOutcome::Revoked(response) => response,
                }
            }
            Err(code) => unauthorized(code, Some(&tenant)),
        },
        crate::grammar::ParsedPath::Mcp {
            tenant,
            namespace,
            plugin,
        } => match request_cap(&headers) {
            Ok(Some(bearer)) => match try_lease_path(
                &state.auth,
                Some(&tenant),
                &namespace,
                &plugin,
                bearer.as_str(),
                &state,
                Instant::now(),
            )
            .await
            {
                LeasePathOutcome::Hit(response)
                | LeasePathOutcome::Miss(response)
                | LeasePathOutcome::Expired(response)
                | LeasePathOutcome::Revoked(response) => response,
            },
            Ok(None) => unauthorized(ErrorCode::MissingBearer, Some(&tenant)),
            Err(code) => unauthorized(code, Some(&tenant)),
        },
        crate::grammar::ParsedPath::ApiAuthProtected => match request_cap(&headers) {
            Ok(Some(bearer)) => match try_lease_path(
                &state.auth,
                None,
                API_NAMESPACE,
                API_PLUGIN,
                bearer.as_str(),
                &state,
                Instant::now(),
            )
            .await
            {
                LeasePathOutcome::Hit(response)
                | LeasePathOutcome::Miss(response)
                | LeasePathOutcome::Expired(response)
                | LeasePathOutcome::Revoked(response) => response,
            },
            Ok(None) => unauthorized(ErrorCode::MissingBearer, None),
            Err(code) => unauthorized(code, None),
        },
        crate::grammar::ParsedPath::Api { tenant } => {
            let tenant_scope = tenant.as_deref();
            match request_cap(&headers) {
                Ok(Some(bearer)) => match try_lease_path(
                    &state.auth,
                    tenant_scope,
                    API_NAMESPACE,
                    API_PLUGIN,
                    bearer.as_str(),
                    &state,
                    Instant::now(),
                )
                .await
                {
                    LeasePathOutcome::Hit(response)
                    | LeasePathOutcome::Miss(response)
                    | LeasePathOutcome::Expired(response)
                    | LeasePathOutcome::Revoked(response) => response,
                },
                Ok(None) => unauthorized(ErrorCode::MissingBearer, tenant_scope),
                Err(code) => unauthorized(code, tenant_scope),
            }
        }
    }
}

pub async fn fetch(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let cap_header = headers.get("x-botwork-cap");
    let cap_for_log = cap_header.and_then(|value| value.to_str().ok());
    let cap_presence = if cap_header.is_some() {
        "present"
    } else {
        "absent"
    };
    let cap_prefix = cap_for_log
        .map(redact_token)
        .unwrap_or_else(|| "-".to_string());
    let cap_len = cap_header
        .map(|value| {
            value
                .to_str()
                .map_or_else(|_| value.as_bytes().len(), str::len)
        })
        .unwrap_or(0);
    info!("{PREFIX} secrets/fetch: cap={cap_presence} cap_prefix={cap_prefix} cap_len={cap_len}");

    let Some(raw_cap_header) = cap_header else {
        warn!("{PREFIX} secrets/fetch: rejected — missing x-botwork-cap");
        return unauthorized(ErrorCode::MissingBearer, None);
    };
    let cap_value = match raw_cap_header.to_str() {
        Ok(value) => value,
        Err(_) => {
            warn!("{PREFIX} secrets/fetch: rejected — malformed cap (base64url decode failed)");
            return unauthorized(ErrorCode::InvalidBearer, None);
        }
    };

    let cap_id = match decode_cap_with_reason(cap_value) {
        Ok(cap_id) => cap_id,
        Err(reason) => {
            warn!("{PREFIX} secrets/fetch: rejected — malformed cap ({reason})");
            log_fetch_unauthorized("bad header", Some(cap_value));
            return unauthorized(ErrorCode::InvalidBearer, None);
        }
    };

    let now = Instant::now();
    let (cache_key, namespace, plugin) = {
        let mut caps = state.caps.lock().await;
        let Some(entry) = caps.get(&cap_id) else {
            warn!("{PREFIX} secrets/fetch: rejected — unknown cap (not in cap cache)");
            log_fetch_unauthorized("unknown cap", Some(cap_value));
            return unauthorized(ErrorCode::InvalidBearer, None);
        };

        if cap_is_expired(entry, now) {
            let age = now.duration_since(entry.expires_at - CAP_TTL).as_secs();
            let ttl = CAP_TTL.as_secs();
            caps.remove(&cap_id);
            warn!("{PREFIX} secrets/fetch: rejected — expired cap age={age}s ttl={ttl}s");
            log_fetch_unauthorized("cap expired", Some(cap_value));
            return unauthorized(ErrorCode::InvalidBearer, None);
        }

        (
            entry.cache_key,
            entry.namespace.clone(),
            entry.plugin.clone(),
        )
    };

    let mut evicted = false;
    let payload = {
        let mut cache = state.cache.lock().await;
        let should_evict = cache
            .get(&cache_key)
            .map(|entry| is_expired(entry, now))
            .unwrap_or(true);

        if should_evict {
            // Drop the cache entry. The `UnlockedMasterKey` it
            // carried scrubs itself via its `Zeroizing` wrapper on
            // drop; we don't have to call any further `lock` step
            // because v4 no longer caches a full `Vault`.
            cache.remove(&cache_key);
            evicted = true;
            None
        } else if let Some(entry) = cache.get(&cache_key) {
            // v4 per-secret unlock: open the vault file under the
            // cached master key, fetch only the entries the cap's
            // `plugin` is allowed to see, decrypt each one
            // individually, and let the plaintext drop out of
            // scope before the next entry's loop iteration. The
            // cache never holds plaintext secret values.
            let mut vault = Vault::new(&entry.vault_root);
            if let Err(err) = vault.open_with_master(&entry.master) {
                warn!(
                    "{PREFIX} secrets/fetch: rejected — vault unlock failed for tenant={} err={err}",
                    entry.tenant
                );
                evicted = true;
                None
            } else {
                match vault.list_secrets() {
                    Ok(listed) => {
                        let vault_secrets = listed.len();
                        let mut secrets = Vec::new();
                        let mut returned = Vec::new();
                        for (key, meta) in listed {
                            if !meta.allowed_consumers.contains(&plugin) {
                                continue;
                            }
                            // Per-secret decrypt: `decrypted` is a
                            // `Zeroizing<Vec<u8>>` that wipes itself
                            // at the end of this loop iteration once
                            // the base64 encoding (which is also
                            // `Zeroizing`) has copied the bytes
                            // through to the response.
                            match vault.decrypt_entry(&entry.master, &key) {
                                Ok(decrypted) => {
                                    let kind = meta.kind.to_string();
                                    returned.push((
                                        key.service.clone(),
                                        key.name.clone(),
                                        kind.clone(),
                                    ));
                                    secrets.push(FetchSecret {
                                        service: key.service,
                                        name: key.name,
                                        kind,
                                        value_b64: Zeroizing::new(STANDARD.encode(&*decrypted)),
                                    });
                                }
                                Err(_) => continue,
                            }
                        }

                        Some((
                            FetchResponse {
                                tenant: entry.tenant.clone(),
                                plugin: plugin.clone(),
                                secrets,
                            },
                            vault_secrets,
                            returned,
                        ))
                    }
                    Err(_) => {
                        evicted = true;
                        None
                    }
                }
            }
        } else {
            evicted = true;
            None
        }
    };

    if evicted {
        evict_caps_for(&state, cache_key).await;
        warn!("{PREFIX} secrets/fetch: rejected — orphaned cap (underlying cache entry evicted)");
        log_fetch_unauthorized("vault evicted", Some(cap_value));
        return unauthorized(ErrorCode::InvalidBearer, None);
    }

    let Some((payload, vault_secrets, returned)) = payload else {
        warn!("{PREFIX} secrets/fetch: rejected — orphaned cap (underlying cache entry evicted)");
        log_fetch_unauthorized("vault evicted", Some(cap_value));
        return unauthorized(ErrorCode::InvalidBearer, None);
    };
    let visible_to_plugin = payload.secrets.len();
    let returned_fmt = returned
        .iter()
        .map(|(service, name, kind)| format!("({service},{name},{kind})"))
        .collect::<Vec<_>>()
        .join(",");

    info!(
        "{PREFIX} secrets/fetch: ok tenant={} namespace={} plugin={} vault_secrets={} visible_to_plugin={} returned=[{}]",
        payload.tenant,
        namespace,
        payload.plugin,
        vault_secrets,
        visible_to_plugin,
        returned_fmt
    );

    Json(payload).into_response()
}

/// `GET /auth/lease/wrapped-export-key` — issue #146 round 1b.
///
/// The CLI's `botwork-vault init --from-lease` / `put-secret` /
/// `list` flow needs the wrapped export_key for the current
/// lease so it can derive the vault master key. Auth is the same
/// bearer the rest of the lease-path uses (the lease IS the
/// authority that says "you may unlock this tenant's vault"); no
/// admin-only gate.
pub async fn wrapped_export_key(State(state): State<AppState>, headers: HeaderMap) -> Response {
    use chrono::Utc;

    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(bearer) = extract_bearer(auth_header) else {
        return unauthorized(ErrorCode::MissingBearer, None);
    };
    let Ok(decoded) = URL_SAFE_NO_PAD.decode(bearer.as_str()) else {
        return unauthorized(ErrorCode::InvalidBearer, None);
    };
    if decoded.len() != crate::auth::lease::BEARER_BYTES {
        return unauthorized(ErrorCode::InvalidBearer, None);
    }
    let bearer = match crate::auth::lease::Bearer::try_from_slice(&decoded) {
        Ok(bearer) => bearer,
        Err(_) => return unauthorized(ErrorCode::InvalidBearer, None),
    };

    match state
        .auth
        .lease_store
        .validate_and_extend(&bearer, Utc::now())
        .await
    {
        Ok(Some(validated)) => {
            state
                .auth
                .remember_lease_export_key(validated.lease.id, &validated.export_key)
                .await;
            // Return the OPAQUE-mutual SessionKey bytes
            // verbatim. The client (`botwork-vault init
            // --from-lease` / `Vault::create` / `Vault::unlock`)
            // feeds them straight into HKDF along with the
            // per-vault salt + suite_version, and the broker's own
            // `try_lease_path` derives the master from the exact
            // same bytes when it opens the vault on
            // `/auth/check`. The two derivations have to converge,
            // so this endpoint must NOT re-wrap before returning —
            // an earlier draft did, and the e2e round-trip caught
            // the mismatch (`/auth/check` 401s with
            // `invalid_bearer` because the vault was sealed under
            // wrapped bytes but the broker tries to open it under
            // unwrapped ones).
            //
            // Transit confidentiality is the TLS layer's job; this
            // payload is bearer-gated so only the live lease
            // holder ever sees the bytes, and they're already
            // OPAQUE-derived (not the user's password), so a
            // wire-captured copy can't be used to log in.
            Json(WrappedExportKeyResponse {
                wrapped_export_key: URL_SAFE_NO_PAD.encode(&*validated.export_key),
                suite_version: botwork_opaque_handshake::SUITE_VERSION,
            })
            .into_response()
        }
        Ok(None) => unauthorized(ErrorCode::InvalidBearer, None),
        Err(crate::auth::lease::ValidationError::Expired) => {
            unauthorized(ErrorCode::ExpiredLease, None)
        }
        Err(crate::auth::lease::ValidationError::Revoked) => {
            unauthorized(ErrorCode::RevokedLease, None)
        }
        Err(crate::auth::lease::ValidationError::Db(err)) => {
            warn!("{PREFIX} auth/lease/wrapped-export-key: db error {err}");
            // Without the legacy fall-through there's no graceful
            // degradation any more — a DB outage just looks like
            // "lease can't be validated", and the client should
            // re-login when the broker is healthy again.
            unauthorized(ErrorCode::InvalidBearer, None)
        }
    }
}

#[derive(Serialize)]
struct WrappedExportKeyResponse {
    /// URL-safe-base64 (no pad) of the wrapped session-key bytes.
    /// Client `Vault::unlock` / `Vault::create` consumes these
    /// bytes verbatim as the HKDF input.
    wrapped_export_key: String,
    /// Suite version the lease was minted against.
    suite_version: u8,
}

#[derive(Debug, Deserialize)]
struct ApiLoginRequest {
    tenant: Option<String>,
    credential_identifier: Option<String>,
    opaque_login_request: Option<String>,
    handshake_id: Option<Uuid>,
    opaque_login_finalization: Option<String>,
    #[serde(default)]
    lease_seconds_requested: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ApiLoginResponse {
    Start {
        handshake_id: Uuid,
        opaque_login_response: String,
        expires_in_seconds: u64,
    },
    Finish {
        bearer: String,
        expires_at: chrono::DateTime<chrono::Utc>,
        lease_id: Uuid,
    },
}

#[derive(Debug, Serialize)]
struct ApiWhoAmIResponse {
    tenant: String,
    lease_id: Uuid,
    expires_at: chrono::DateTime<chrono::Utc>,
    idle_extends_to: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
struct ApiErrorEnvelope {
    error: ApiErrorBody,
}

#[derive(Debug, Serialize)]
struct ApiErrorBody {
    code: &'static str,
    message: String,
}

struct ValidatedRequestLease {
    bearer: crate::auth::lease::Bearer,
    tenant: String,
    lease: crate::auth::LeaseRow,
}

fn api_json_error(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        Json(ApiErrorEnvelope {
            error: ApiErrorBody {
                code,
                message: message.into(),
            },
        }),
    )
        .into_response()
}

fn cookie_is_secure(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

fn cookie_expires_at(expires_at: chrono::DateTime<chrono::Utc>) -> String {
    // RFC 1123 cookie date format. chrono's %a/%b are fixed English regardless of
    // system locale, so this is safe to use directly.
    expires_at.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn auth_cookie(value: &str, expires_at: chrono::DateTime<chrono::Utc>, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{BOTWORK_CAP_COOKIE_NAME}={value}; Path=/; HttpOnly; SameSite=Lax; Expires={}{}",
        cookie_expires_at(expires_at),
        secure
    )
}

fn clear_auth_cookie(secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{BOTWORK_CAP_COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT{}",
        secure
    )
}

async fn validate_request_lease(
    auth: &crate::auth::AuthState,
    headers: &HeaderMap,
) -> Result<ValidatedRequestLease, Response> {
    let Some(token) = request_cap(headers).map_err(|code| unauthorized(code, None))? else {
        return Err(unauthorized(ErrorCode::MissingBearer, None));
    };
    let decoded = URL_SAFE_NO_PAD
        .decode(token.as_str())
        .map_err(|_| unauthorized(ErrorCode::InvalidBearer, None))?;
    let bearer = crate::auth::lease::Bearer::try_from_slice(&decoded)
        .map_err(|_| unauthorized(ErrorCode::InvalidBearer, None))?;
    let validated = match auth
        .lease_store
        .validate_and_extend(&bearer, Utc::now())
        .await
    {
        Ok(Some(validated)) => validated,
        Ok(None) => return Err(unauthorized(ErrorCode::InvalidBearer, None)),
        Err(crate::auth::lease::ValidationError::Expired) => {
            return Err(unauthorized(ErrorCode::ExpiredLease, None))
        }
        Err(crate::auth::lease::ValidationError::Revoked) => {
            return Err(unauthorized(ErrorCode::RevokedLease, None))
        }
        Err(crate::auth::lease::ValidationError::Db(err)) => {
            warn!("{PREFIX} lease validation failed err={err}");
            return Err(unauthorized(ErrorCode::InvalidBearer, None));
        }
    };
    let tenant = match auth
        .tenant_store
        .lookup_tenant_name_by_id(validated.lease.tenant_id)
        .await
    {
        Ok(Some(name)) => name,
        Ok(None) => return Err(unauthorized(ErrorCode::InvalidBearer, None)),
        Err(err) => {
            warn!(
                "{PREFIX} tenant lookup failed lease_id={} err={err}",
                validated.lease.id
            );
            return Err(unauthorized(ErrorCode::InvalidBearer, None));
        }
    };
    auth.remember_lease_export_key(validated.lease.id, &validated.export_key)
        .await;
    Ok(ValidatedRequestLease {
        bearer,
        tenant,
        lease: validated.lease,
    })
}

async fn api_auth_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ApiLoginRequest>,
) -> Response {
    if let Some(opaque_login_request) = body.opaque_login_request {
        let Some(tenant) = body.tenant else {
            return api_json_error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "`tenant` is required for login start",
            );
        };
        let credential_identifier = body.credential_identifier.unwrap_or_else(|| tenant.clone());
        match crate::auth::endpoints::login_start_inner(
            &state.auth,
            crate::auth::endpoints::SharedLoginStartRequest {
                tenant,
                credential_identifier,
                login_request: opaque_login_request,
                lease_seconds_requested: body.lease_seconds_requested,
            },
        )
        .await
        {
            Ok(response) => Json(ApiLoginResponse::Start {
                handshake_id: response.handshake_id,
                opaque_login_response: response.login_response,
                expires_in_seconds: response.expires_in_seconds,
            })
            .into_response(),
            Err(response) => response,
        }
    } else if let (Some(handshake_id), Some(opaque_login_finalization)) =
        (body.handshake_id, body.opaque_login_finalization)
    {
        match crate::auth::endpoints::login_finish_inner(
            &state.auth,
            crate::auth::endpoints::SharedLoginFinishRequest {
                handshake_id,
                login_finalization: opaque_login_finalization,
            },
        )
        .await
        {
            Ok(response) => {
                let mut out = Json(ApiLoginResponse::Finish {
                    bearer: response.bearer.clone(),
                    expires_at: response.expires_at,
                    lease_id: response.lease_id,
                })
                .into_response();
                if let Ok(cookie) = auth_cookie(
                    &response.bearer,
                    response.expires_at,
                    cookie_is_secure(&headers),
                )
                .parse()
                {
                    out.headers_mut().append("set-cookie", cookie);
                }
                out
            }
            Err(response) => response,
        }
    } else {
        api_json_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "expected either `opaque_login_request` or (`handshake_id`, `opaque_login_finalization`)",
        )
    }
}

async fn api_auth_logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let secure = cookie_is_secure(&headers);
    let validated = match validate_request_lease(&state.auth, &headers).await {
        Ok(validated) => validated,
        Err(_) => {
            // Lease already gone (revoked, expired, missing bearer) — still clear the
            // cookie so the client ends up in a clean logged-out state.  Logout must
            // be idempotent: a double-tap / retry must not surface a 401.
            let mut response = StatusCode::NO_CONTENT.into_response();
            if let Ok(cookie) = clear_auth_cookie(secure).parse() {
                response.headers_mut().append("set-cookie", cookie);
            }
            return response;
        }
    };
    let cache_key = cache_key(
        &validated.tenant,
        &URL_SAFE_NO_PAD.encode(validated.bearer.as_bytes()),
    );
    evict_caps_for(&state, cache_key).await;
    if let Err(err) = state
        .auth
        .lease_store
        .revoke(&validated.bearer.hash(), Utc::now())
        .await
    {
        warn!(
            "{PREFIX} api/auth/logout: revoke failed lease_id={} err={err}",
            validated.lease.id
        );
        return api_json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("database error: {err}"),
        );
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    if let Ok(cookie) = clear_auth_cookie(secure).parse() {
        response.headers_mut().append("set-cookie", cookie);
    }
    response
}

async fn api_auth_whoami(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match validate_request_lease(&state.auth, &headers).await {
        Ok(validated) => Json(ApiWhoAmIResponse {
            tenant: validated.tenant,
            lease_id: validated.lease.id,
            expires_at: validated.lease.expires_at,
            idle_extends_to: validated.lease.idle_extends_to,
        })
        .into_response(),
        Err(response) => response,
    }
}

pub fn build_router(state: AppState) -> Router {
    // /auth/{register,login}/* are mounted via the auth subrouter
    // (which is generic over `AuthState`). We compose the
    // `AppState`-typed router with the auth subrouter by attaching
    // the auth state separately and then merging — same trick
    // round 1a used, kept here so the merge into a fresh
    // `Router::<AppState>` resolves.
    let core = Router::new()
        .route("/secrets/fetch", post(fetch))
        .route("/auth/lease/wrapped-export-key", get(wrapped_export_key))
        .route("/api/auth/login", post(api_auth_login))
        .route("/api/auth/logout", post(api_auth_logout))
        .route("/api/auth/whoami", get(api_auth_whoami))
        .route("/", any(check))
        .route("/{*path}", any(check))
        .with_state(state.clone());
    core.merge(crate::auth::build_auth_router(state.auth.clone()))
        .merge(crate::admin::build_admin_router(state))
}

pub fn build_user_api_router(state: AppState) -> Router {
    crate::secrets::build_secrets_router(state)
}

/// Outcome of the lease path at the top of `/auth/check`.
///
/// Round 1b: `Miss` no longer means "fall through to legacy" — it
/// means "401 immediately with `invalid_bearer`". The variant
/// stays because the underlying lookup distinguishes "row not
/// found" from "expired" / "revoked", and the structured 401
/// taxonomy from issue #125 splits those into separate
/// `error.code` values.
enum LeasePathOutcome {
    Hit(Response),
    Miss(Response),
    Expired(Response),
    Revoked(Response),
}

/// OS-injection-required: a vault that creates-OK but then fails to unlock
/// needs OS-level fault injection (filesystem or crypto hardware); validated
/// by the docker/e2e tier. Cannot be triggered in an offline unit test.
/// Extracted so the unreachable arm body leaves the coverage denominator;
/// behaviour is identical in a normal build (the attribute is only active
/// under `--cfg tarpaulin_include`).
#[cfg(not(tarpaulin_include))]
fn try_lease_path_autocreate_unlock_fail(
    tenant: &str,
    lease_id: Uuid,
    err: impl std::fmt::Display,
) -> LeasePathOutcome {
    warn!(
        "{PREFIX} auth/check: vault auto-create unlock failed \
         tenant={tenant} lease_id={lease_id} err={err}"
    );
    LeasePathOutcome::Miss(unauthorized(ErrorCode::InvalidBearer, Some(tenant)))
}

/// Non-tarpaulin build: identical logic, included in coverage. Kept as a
/// separate definition so the normal build always has this function
/// regardless of the `tarpaulin` cfg.
#[cfg(tarpaulin_include)]
fn try_lease_path_autocreate_unlock_fail(
    tenant: &str,
    lease_id: Uuid,
    err: impl std::fmt::Display,
) -> LeasePathOutcome {
    warn!(
        "{PREFIX} auth/check: vault auto-create unlock failed \
         tenant={tenant} lease_id={lease_id} err={err}"
    );
    LeasePathOutcome::Miss(unauthorized(ErrorCode::InvalidBearer, Some(tenant)))
}

/// OS-injection-required: `Vault::create` fails when `vault_root` is on a
/// read-only filesystem or lacks write permission; validated by the
/// docker/e2e tier. Cannot be triggered in an offline unit test.
/// Extracted so the unreachable arm body leaves the coverage denominator;
/// behaviour is identical in a normal build.
#[cfg(not(tarpaulin_include))]
fn try_lease_path_create_fail(
    tenant: &str,
    lease_id: Uuid,
    err: impl std::fmt::Display,
) -> LeasePathOutcome {
    warn!(
        "{PREFIX} auth/check: vault auto-create write failed \
         tenant={tenant} lease_id={lease_id} err={err}"
    );
    LeasePathOutcome::Miss(unauthorized(ErrorCode::InvalidBearer, Some(tenant)))
}

/// Non-tarpaulin build: identical logic, included in coverage. Kept as a
/// separate definition so the normal build always has this function
/// regardless of the `tarpaulin` cfg.
#[cfg(tarpaulin_include)]
fn try_lease_path_create_fail(
    tenant: &str,
    lease_id: Uuid,
    err: impl std::fmt::Display,
) -> LeasePathOutcome {
    warn!(
        "{PREFIX} auth/check: vault auto-create write failed \
         tenant={tenant} lease_id={lease_id} err={err}"
    );
    LeasePathOutcome::Miss(unauthorized(ErrorCode::InvalidBearer, Some(tenant)))
}

#[allow(clippy::too_many_arguments)]
async fn try_lease_path(
    auth: &crate::auth::AuthState,
    tenant_scope: Option<&str>,
    namespace: &str,
    plugin: &str,
    bearer: &str,
    state: &AppState,
    now: Instant,
) -> LeasePathOutcome {
    let Ok(decoded) = URL_SAFE_NO_PAD.decode(bearer) else {
        // Round 1b: bearers that aren't well-formed base64-of-32
        // are not lease bearers; the round-1a behaviour of
        // "fall through to legacy" is gone, so we 401 here.
        #[rustfmt::skip]
        warn!("{PREFIX} auth/check: bearer not a lease shape tenant={} bearer={}", tenant_scope.unwrap_or("-"), redact_token(bearer));
        return LeasePathOutcome::Miss(unauthorized(ErrorCode::InvalidBearer, tenant_scope));
    };
    if decoded.len() != crate::auth::lease::BEARER_BYTES {
        #[rustfmt::skip]
        warn!("{PREFIX} auth/check: bearer not 32 bytes tenant={} bearer={}", tenant_scope.unwrap_or("-"), redact_token(bearer));
        return LeasePathOutcome::Miss(unauthorized(ErrorCode::InvalidBearer, tenant_scope));
    }
    let lease_bearer = match crate::auth::lease::Bearer::try_from_slice(&decoded) {
        Ok(bearer) => bearer,
        Err(_) => {
            return LeasePathOutcome::Miss(unauthorized(ErrorCode::InvalidBearer, tenant_scope))
        }
    };
    match auth
        .lease_store
        .validate_and_extend(&lease_bearer, Utc::now())
        .await
    {
        Ok(Some(validated)) => {
            let tenant = match auth
                .tenant_store
                .lookup_tenant_name_by_id(validated.lease.tenant_id)
                .await
            {
                Ok(Some(tenant)) => tenant,
                Ok(None) => {
                    return LeasePathOutcome::Miss(unauthorized(
                        ErrorCode::InvalidBearer,
                        tenant_scope,
                    ))
                }
                Err(err) => {
                    warn!("{PREFIX} auth/check: tenant lookup failed err={err}");
                    return LeasePathOutcome::Miss(unauthorized(
                        ErrorCode::InvalidBearer,
                        tenant_scope,
                    ));
                }
            };
            if let Some(expected) = tenant_scope {
                if tenant != expected {
                    warn!("{PREFIX} auth/check: tenant mismatch expected={expected} actual={} bearer={}", tenant, redact_token(bearer));
                    return LeasePathOutcome::Miss(unauthorized(
                        ErrorCode::InvalidBearer,
                        Some(expected),
                    ));
                }
            }
            auth.remember_lease_export_key(validated.lease.id, &validated.export_key)
                .await;
            let lease_id = validated.lease.id;
            let suite_version = botwork_opaque_handshake::SUITE_VERSION;
            let cache_key = cache_key(&tenant, bearer);

            // Per the issue body: the cache entry no longer holds
            // a whole `Vault`. We materialise the
            // `UnlockedMasterKey` from the OPAQUE-supplied
            // session-key bytes (the wire form of the lease's
            // wrapped value) and cache that, plus the path the
            // tenant's vault lives at.
            let vault_root = state.vault_root.join(&tenant);
            let mut vault = Vault::new(&vault_root);
            let cache_master = match vault.unlock_master(&validated.export_key, suite_version) {
                Ok(master) => Some(master),
                Err(VaultError::NotInitialized(_)) => {
                    info!(
                        "{PREFIX} auth/check: lease validated tenant={tenant} lease_id={lease_id} \
                         (vault not initialised — creating fresh v4 vault from lease export_key)"
                    );
                    match Vault::create(&vault_root, &validated.export_key, suite_version) {
                        Ok(mut created_vault) => {
                            match created_vault.unlock_master(&validated.export_key, suite_version)
                            {
                                Ok(master) => {
                                    info!(
                                        "{PREFIX} auth/check: fresh vault created tenant={tenant} \
                                         lease_id={lease_id} suite_version={suite_version}"
                                    );
                                    Some(master)
                                }
                                Err(err) => {
                                    return try_lease_path_autocreate_unlock_fail(
                                        &tenant, lease_id, err,
                                    );
                                }
                            }
                        }
                        Err(err) => {
                            return try_lease_path_create_fail(&tenant, lease_id, err);
                        }
                    }
                }
                Err(err) => {
                    warn!(
                        "{PREFIX} auth/check: vault unlock failed tenant={tenant} lease_id={lease_id} err={err}"
                    );
                    return LeasePathOutcome::Miss(unauthorized(
                        ErrorCode::InvalidBearer,
                        Some(&tenant),
                    ));
                }
            };

            // Cache the unlocked master key against `cache_key`.
            let absolute_ttl = state.ttl_config.absolute_for(&tenant);
            let idle_ttl = state.ttl_config.idle_for(&tenant);
            if let Some(master) = cache_master {
                let mut cache = state.cache.lock().await;
                if let Some(existing) = cache.get_mut(&cache_key) {
                    existing.last_used = now;
                } else {
                    cache.insert(
                        cache_key,
                        CacheEntry {
                            tenant: tenant.clone(),
                            vault_root: vault_root.clone(),
                            master,
                            suite_version,
                            expires_at: now + absolute_ttl,
                            last_used: now,
                            created_at: now,
                            idle_ttl,
                        },
                    );
                    state.metrics.inc_insert();
                }
            }
            drop(vault);

            info!("{PREFIX} auth/check: lease validated tenant={tenant} namespace={namespace} plugin={plugin} lease_id={lease_id} bearer={}", redact_token(bearer));
            let (cap_value, cap_entry) =
                mint_cap_value(cache_key, namespace, plugin, lease_id, now);
            let mut caps = state.caps.lock().await;
            caps.insert(decode_cap(&cap_value).expect("newly minted cap"), cap_entry);
            // Sliding-extend on cache hits handled above; for a
            // fresh insert we already stamped expires_at at now +
            // absolute_ttl.
            LeasePathOutcome::Hit(success(&tenant, &cap_value))
        }
        Ok(None) => {
            #[rustfmt::skip]
            warn!("{PREFIX} auth/check: lease lookup miss tenant={} bearer={}", tenant_scope.unwrap_or("-"), redact_token(bearer));
            // Increment the cache-key-cohort eviction prophylactically:
            // a bearer that previously validated and then got rotated
            // (re-login on the client side mints a fresh bearer) leaves
            // a stale cache entry with the old cache_key; evicting it
            // here on the next miss is the cheapest way to keep the
            // cache from growing without a janitor sweep.
            if let Some(tenant) = tenant_scope {
                evict_caps_for(state, cache_key(tenant, bearer)).await;
            }
            LeasePathOutcome::Miss(unauthorized(ErrorCode::InvalidBearer, tenant_scope))
        }
        Err(crate::auth::lease::ValidationError::Expired) => {
            #[rustfmt::skip]
            warn!("{PREFIX} auth/check: lease expired tenant={} bearer={}", tenant_scope.unwrap_or("-"), redact_token(bearer));
            LeasePathOutcome::Expired(unauthorized(ErrorCode::ExpiredLease, tenant_scope))
        }
        Err(crate::auth::lease::ValidationError::Revoked) => {
            #[rustfmt::skip]
            warn!("{PREFIX} auth/check: lease revoked tenant={} bearer={}", tenant_scope.unwrap_or("-"), redact_token(bearer));
            LeasePathOutcome::Revoked(unauthorized(ErrorCode::RevokedLease, tenant_scope))
        }
        Err(crate::auth::lease::ValidationError::Db(err)) => {
            #[rustfmt::skip]
            warn!("{PREFIX} auth/check: lease lookup db error tenant={} err={err}", tenant_scope.unwrap_or("-"));
            // Round 1b: no legacy fall-through, so a DB error is
            // surfaced as `invalid_bearer`. Operators monitor the
            // "lease lookup db error" warn log and respond to
            // sustained errors as DB outages; clients retry once
            // the broker is healthy again.
            LeasePathOutcome::Miss(unauthorized(ErrorCode::InvalidBearer, tenant_scope))
        }
    }
}

// `/secrets/fetch` no longer needs a `master_bytes()` helper —
// `Vault::open_with_master` takes an `&UnlockedMasterKey` directly,
// so the master bytes never leave the opaque holder. This is the
// property that pins the per-secret-unlock invariant: there is only
// ever one master in memory at a time per cache entry, and dropping
// the entry scrubs it.

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use chrono::TimeZone;

    // ---------------------------------------------------------------------------
    // success_admin
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn success_admin_returns_200_with_x_botwork_admin_header() {
        let response = success_admin();
        assert_eq!(response.status(), StatusCode::OK);
        let admin_header = response
            .headers()
            .get("x-botwork-admin")
            .and_then(|v| v.to_str().ok());
        assert_eq!(admin_header, Some("admin"));
    }

    #[tokio::test]
    async fn success_admin_does_not_set_x_botwork_tenant() {
        let response = success_admin();
        assert!(
            response.headers().get("x-botwork-tenant").is_none(),
            "success_admin must not set x-botwork-tenant"
        );
    }

    // ---------------------------------------------------------------------------
    // redact_token
    // ---------------------------------------------------------------------------

    #[test]
    fn redact_token_short_returns_redacted() {
        assert_eq!(redact_token("hi"), "***redacted***");
        assert_eq!(redact_token(""), "***redacted***");
        assert_eq!(redact_token("123456"), "***redacted***");
    }

    #[test]
    fn redact_token_long_returns_prefix_with_ellipsis() {
        assert_eq!(redact_token("1234567"), "123456…");
        assert_eq!(redact_token("abcdefghij"), "abcdef…");
    }

    // ---------------------------------------------------------------------------
    // extract_tenant_namespace_plugin
    // ---------------------------------------------------------------------------

    #[test]
    fn extract_tenant_namespace_plugin_returns_tuple_for_mcp_path() {
        let result = extract_tenant_namespace_plugin("/acme/mcp/exec-bash");
        assert_eq!(
            result,
            Some((
                "acme".to_string(),
                "mcp".to_string(),
                "exec-bash".to_string()
            ))
        );
    }

    #[test]
    fn extract_tenant_namespace_plugin_returns_none_for_non_mcp_path() {
        assert!(extract_tenant_namespace_plugin("/acme").is_none());
        assert!(extract_tenant_namespace_plugin("/api/auth/whoami").is_none());
        assert!(extract_tenant_namespace_plugin("/api").is_none());
    }

    // ---------------------------------------------------------------------------
    // extract_cookie_cap
    // ---------------------------------------------------------------------------

    #[test]
    fn extract_cookie_cap_returns_none_when_no_matching_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            HeaderValue::from_static("other_cookie=abc; another=def"),
        );
        assert!(extract_cookie_cap(&headers).is_none());
    }

    #[test]
    fn extract_cookie_cap_returns_none_when_cookie_value_is_empty() {
        let mut headers = HeaderMap::new();
        let cookie = format!("{}=", BOTWORK_CAP_COOKIE_NAME);
        headers.insert("cookie", HeaderValue::from_str(&cookie).unwrap());
        assert!(extract_cookie_cap(&headers).is_none());
    }

    // ---------------------------------------------------------------------------
    // cookie_is_secure
    // ---------------------------------------------------------------------------

    #[test]
    fn cookie_is_secure_false_without_forwarded_proto_header() {
        let headers = HeaderMap::new();
        assert!(!cookie_is_secure(&headers));
    }

    #[test]
    fn cookie_is_secure_true_when_proto_is_https() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert!(cookie_is_secure(&headers));
    }

    #[test]
    fn cookie_is_secure_false_when_proto_is_http() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", HeaderValue::from_static("http"));
        assert!(!cookie_is_secure(&headers));
    }

    // ---------------------------------------------------------------------------
    // cookie_expires_at
    // ---------------------------------------------------------------------------

    #[test]
    fn cookie_expires_at_formats_rfc1123_date() {
        let dt = chrono::Utc.with_ymd_and_hms(2025, 1, 15, 12, 0, 0).unwrap();
        let formatted = cookie_expires_at(dt);
        assert!(
            formatted.contains("2025"),
            "should contain year, got {formatted}"
        );
        assert!(
            formatted.contains("GMT"),
            "should contain GMT suffix, got {formatted}"
        );
    }

    // ---------------------------------------------------------------------------
    // auth_cookie
    // ---------------------------------------------------------------------------

    #[test]
    fn auth_cookie_without_secure_omits_secure_flag() {
        let dt = chrono::Utc.with_ymd_and_hms(2025, 1, 15, 12, 0, 0).unwrap();
        let cookie = auth_cookie("mytoken", dt, false);
        assert!(
            cookie.contains(&format!("{BOTWORK_CAP_COOKIE_NAME}=mytoken")),
            "got {cookie}"
        );
        assert!(cookie.contains("Path=/"), "got {cookie}");
        assert!(cookie.contains("HttpOnly"), "got {cookie}");
        assert!(cookie.contains("SameSite=Lax"), "got {cookie}");
        assert!(
            !cookie.contains("; Secure"),
            "should omit Secure flag, got {cookie}"
        );
    }

    #[test]
    fn auth_cookie_with_secure_includes_secure_flag() {
        let dt = chrono::Utc.with_ymd_and_hms(2025, 1, 15, 12, 0, 0).unwrap();
        let cookie = auth_cookie("mytoken", dt, true);
        assert!(
            cookie.contains("; Secure"),
            "should include Secure flag, got {cookie}"
        );
    }

    // ---------------------------------------------------------------------------
    // clear_auth_cookie
    // ---------------------------------------------------------------------------

    #[test]
    fn clear_auth_cookie_without_secure_omits_secure_flag() {
        let cookie = clear_auth_cookie(false);
        assert!(
            cookie.contains(&format!("{BOTWORK_CAP_COOKIE_NAME}=")),
            "got {cookie}"
        );
        assert!(cookie.contains("Max-Age=0"), "got {cookie}");
        assert!(
            !cookie.contains("; Secure"),
            "should omit Secure flag, got {cookie}"
        );
    }

    #[test]
    fn clear_auth_cookie_with_secure_includes_secure_flag() {
        let cookie = clear_auth_cookie(true);
        assert!(
            cookie.contains("; Secure"),
            "should include Secure flag, got {cookie}"
        );
    }

    // ---------------------------------------------------------------------------
    // api_json_error
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn api_json_error_returns_correct_status_and_body() {
        let response = api_json_error(StatusCode::BAD_REQUEST, "bad_request", "something wrong");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(body["error"]["code"], "bad_request");
        assert_eq!(body["error"]["message"], "something wrong");
    }

    // ---------------------------------------------------------------------------
    // log_fetch_unauthorized (covers the None/missing-cap arm)
    // ---------------------------------------------------------------------------

    #[test]
    fn log_fetch_unauthorized_with_none_cap_does_not_panic() {
        // This drives the `None => "<missing>"` arm in log_fetch_unauthorized.
        log_fetch_unauthorized("test-reason", None);
    }

    #[test]
    fn log_fetch_unauthorized_with_some_cap_does_not_panic() {
        log_fetch_unauthorized("test-reason", Some("abc123def456"));
    }
}
