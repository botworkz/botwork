//! Wire client: drives `botwork_opaque_handshake` against the
//! broker's `/auth/{register,login}/{start,finish}` endpoints.
//!
//! Layered as plain `pub async fn`s rather than a `Client` struct so
//! consumers (the CLI subcommands today, a future web/admin UI
//! tomorrow) compose them at will. The `reqwest::Client` is created
//! once per call — the round-1a hot path is one
//! `botwork-login` invocation per ~7-day lease, so connection-pool
//! sharing isn't worth the lifetime plumbing.
//!
//! ## Wire shape
//!
//! Every OPAQUE message rides as a url-safe-base64 (no pad) JSON
//! field. The broker's `auth/endpoints.rs` and our
//! `botwork_opaque_handshake` crate use the same encoding for
//! message bytes, so the encode/decode round-trips are symmetric.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use botwork_opaque_handshake::{
    client as opaque_client, LoginResponse, OpaqueError, RegistrationResponse, SessionKey,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::error::LoginError;

/// Maximum number of body bytes captured into a
/// [`LoginError::UnexpectedStatus`] envelope. Keeps the error
/// payload bounded when the broker returns a long traceback.
const MAX_BODY_PREVIEW_BYTES: usize = 512;

/// Outcome of a successful `login` round-trip.
#[derive(Debug, Clone)]
pub struct LoginOutcome {
    /// URL-safe-base64 (no pad) bearer token returned by
    /// `/auth/login/finish`. Wrapped in `Zeroizing` so the buffer is
    /// wiped after the caller persists / displays it.
    pub bearer: Zeroizing<String>,
    /// UUID of the freshly-minted lease row.
    pub lease_id: Uuid,
    /// Broker-supplied absolute expiry of the lease.
    pub expires_at: DateTime<Utc>,
    /// Mutual OPAQUE session key. Currently unused by the CLI but
    /// returned here so a future caller (e.g. an admin tool that
    /// needs to authenticate to a follow-up endpoint without going
    /// back through OPAQUE) has it without rerunning the handshake.
    pub session_key: SessionKey,
}

/// Outcome of a successful `register` round-trip.
#[derive(Debug, Clone)]
pub struct RegisterOutcome {
    /// Tenant name (echoed verbatim).
    pub tenant: String,
    /// OPAQUE ciphersuite version observed in the broker's reply.
    pub suite_version: u8,
}

/// Drive an OPAQUE login against `base_url` for `tenant`.
///
/// `password` is borrowed read-only — the function never holds a
/// reference past the OPAQUE handshake state's lifetime, so the
/// caller can drop it immediately after this returns.
pub async fn run_login(
    base_url: &Url,
    tenant: &str,
    credential_identifier: &str,
    password: &[u8],
    lease_seconds: u64,
    ca_path: Option<&Path>,
) -> Result<LoginOutcome, LoginError> {
    let http = build_http_client_with_ca(ca_path)?;
    let mut rng = rand::thread_rng();

    // ── login/start ────────────────────────────────────────────────
    let cl = opaque_client::login_start(&mut rng, password)?;
    let start_url = base_url
        .join("auth/login/start")
        .expect("endpoint path is always valid");
    let start_body = json!({
        "tenant": tenant,
        "credential_identifier": credential_identifier,
        "login_request": URL_SAFE_NO_PAD.encode(cl.request.serialize()),
        "lease_seconds_requested": lease_seconds,
    });
    let body: LoginStartResponseBody =
        post_json_and_parse(&http, start_url.as_str(), &start_body).await?;
    let response_bytes = decode_b64(start_url.as_str(), "login_response", &body.login_response)?;
    let opaque_response = LoginResponse::deserialize(&response_bytes)?;

    // ── client/login_finish ────────────────────────────────────────
    //
    // The OPAQUE client surfaces wrong passwords against a real
    // tenant HERE, before any second HTTP round-trip. That's the
    // canonical `InvalidLogin` arm — we map it onto the user-facing
    // `incorrect password for tenant` message and skip the
    // `/auth/login/finish` request entirely (no point burning a
    // pending-map entry to confirm what the client already knows).
    let finish = match opaque_client::login_finish(cl.state, password, opaque_response) {
        Ok(f) => f,
        Err(OpaqueError::InvalidLogin) => return Err(LoginError::InvalidLogin(tenant.to_string())),
        Err(err) => return Err(LoginError::Opaque(err)),
    };

    // ── login/finish ───────────────────────────────────────────────
    let finish_url = base_url
        .join("auth/login/finish")
        .expect("endpoint path is always valid");
    let finish_body = json!({
        "handshake_id": body.handshake_id,
        "login_finalization": URL_SAFE_NO_PAD.encode(finish.finalization.serialize()),
    });
    let body: LoginFinishResponseBody =
        post_json_and_parse(&http, finish_url.as_str(), &finish_body).await?;
    Ok(LoginOutcome {
        bearer: Zeroizing::new(body.bearer),
        lease_id: body.lease_id,
        expires_at: body.expires_at,
        session_key: finish.session_key,
    })
}

/// Drive an OPAQUE registration. v0 is an operator-only flow; the
/// CLI exposes it under `botwork-login register`. The broker's
/// 404-on-unknown-tenant arm is mapped to [`LoginError::UnknownTenant`];
/// 409-on-already-registered to [`LoginError::AlreadyRegistered`].
pub async fn run_register(
    base_url: &Url,
    tenant: &str,
    credential_identifier: &str,
    password: &[u8],
    ca_path: Option<&Path>,
) -> Result<RegisterOutcome, LoginError> {
    let http = build_http_client_with_ca(ca_path)?;
    let mut rng = rand::thread_rng();

    // ── register/start ─────────────────────────────────────────────
    let cr = opaque_client::registration_start(&mut rng, password)?;
    let start_url = base_url
        .join("auth/register/start")
        .expect("endpoint path is always valid");
    let start_body = json!({
        "tenant": tenant,
        "credential_identifier": credential_identifier,
        "registration_request": URL_SAFE_NO_PAD.encode(cr.request.serialize()),
    });
    let body: RegisterStartResponseBody =
        match post_json_and_parse_with_tenant_arms(&http, start_url.as_str(), &start_body, tenant)
            .await
        {
            Ok(body) => body,
            Err(err) => return Err(err),
        };
    let response_bytes = decode_b64(
        start_url.as_str(),
        "registration_response",
        &body.registration_response,
    )?;
    let opaque_response = RegistrationResponse::deserialize(&response_bytes)?;
    let cf = opaque_client::registration_finish(&mut rng, cr.state, password, opaque_response)?;

    // ── register/finish ────────────────────────────────────────────
    let finish_url = base_url
        .join("auth/register/finish")
        .expect("endpoint path is always valid");
    let finish_body = json!({
        "tenant": tenant,
        "credential_identifier": credential_identifier,
        "registration_upload": URL_SAFE_NO_PAD.encode(cf.upload.serialize()),
    });
    let body: RegisterFinishResponseBody =
        post_json_and_parse_with_tenant_arms(&http, finish_url.as_str(), &finish_body, tenant)
            .await?;
    Ok(RegisterOutcome {
        tenant: body.tenant,
        suite_version: body.suite_version,
    })
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

fn build_http_client_with_ca(ca_path: Option<&Path>) -> Result<reqwest::Client, LoginError> {
    let mut builder = reqwest::Client::builder();
    if let Some(path) = ca_path {
        let pem = std::fs::read(path).map_err(|err| {
            LoginError::CaCert(format!(
                "failed to read --cacert file {}: {err}",
                path.display()
            ))
        })?;
        let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|err| {
            LoginError::CaCert(format!(
                "failed to parse PEM certificate(s) from {}: {err}",
                path.display()
            ))
        })?;
        if certs.is_empty() {
            return Err(LoginError::CaCert(format!(
                "no valid PEM certificate found in {}",
                path.display()
            )));
        }
        for cert in certs {
            builder = builder.add_root_certificate(cert);
        }
    }
    builder.build().map_err(|source| LoginError::Network {
        url: "<client-init>".to_string(),
        source,
    })
}

/// One-shot `POST` + JSON-parse helper. The wire-error mapping is
/// shared by every endpoint:
///
/// - 2xx → parse the body into `T`.
/// - 401 on `/auth/login/finish` → [`LoginError::InvalidLogin`] (the
///   server's OPAQUE-side rejection of a tenant the client thinks
///   it knows). Wrong-password against a *real* tenant trips on the
///   client side before this point.
/// - other non-2xx → [`LoginError::UnexpectedStatus`] with the first
///   ~512 bytes of the response body.
async fn post_json_and_parse<T: for<'de> Deserialize<'de>>(
    http: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
) -> Result<T, LoginError> {
    let response =
        http.post(url)
            .json(body)
            .send()
            .await
            .map_err(|source| LoginError::Network {
                url: url.to_string(),
                source,
            })?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|source| LoginError::Network {
            url: url.to_string(),
            source,
        })?;
    if status.is_success() {
        return serde_json::from_slice(&bytes).map_err(|source| LoginError::MalformedResponse {
            url: url.to_string(),
            source,
        });
    }
    // 401 on `/auth/login/finish` is the OPAQUE-server-side
    // `InvalidLogin` arm — see the comment in `run_login` for the
    // double-detection rationale.
    if status == reqwest::StatusCode::UNAUTHORIZED && url.ends_with("/auth/login/finish") {
        return Err(LoginError::InvalidLogin("<unknown-tenant>".to_string()));
    }
    Err(LoginError::UnexpectedStatus {
        status: status.as_u16(),
        url: url.to_string(),
        body: truncate_body_bytes(&bytes),
    })
}

/// Variant of [`post_json_and_parse`] that additionally maps the
/// `register` endpoints' tenant-specific status codes (404 →
/// [`LoginError::UnknownTenant`], 409 →
/// [`LoginError::AlreadyRegistered`]) onto typed arms before
/// falling back to [`LoginError::UnexpectedStatus`].
async fn post_json_and_parse_with_tenant_arms<T: for<'de> Deserialize<'de>>(
    http: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    tenant: &str,
) -> Result<T, LoginError> {
    let response =
        http.post(url)
            .json(body)
            .send()
            .await
            .map_err(|source| LoginError::Network {
                url: url.to_string(),
                source,
            })?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|source| LoginError::Network {
            url: url.to_string(),
            source,
        })?;
    if status.is_success() {
        return serde_json::from_slice(&bytes).map_err(|source| LoginError::MalformedResponse {
            url: url.to_string(),
            source,
        });
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(LoginError::UnknownTenant(tenant.to_string()));
    }
    if status == reqwest::StatusCode::CONFLICT {
        return Err(LoginError::AlreadyRegistered(tenant.to_string()));
    }
    Err(LoginError::UnexpectedStatus {
        status: status.as_u16(),
        url: url.to_string(),
        body: truncate_body_bytes(&bytes),
    })
}

fn truncate_body_bytes(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    truncate_body(text.into_owned())
}

fn truncate_body(mut body: String) -> String {
    if body.len() > MAX_BODY_PREVIEW_BYTES {
        // Walk back to the nearest char boundary so the truncate
        // call doesn't panic on multibyte UTF-8 mid-character.
        let mut cut = MAX_BODY_PREVIEW_BYTES;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
        body.push('…');
    }
    body
}

fn decode_b64(url: &str, field: &str, value: &str) -> Result<Vec<u8>, LoginError> {
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|err| LoginError::MalformedResponse {
            url: url.to_string(),
            source: serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("`{field}` is not valid url-safe-base64: {err}"),
            )),
        })
}

// ---------------------------------------------------------------------------
// Wire envelopes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LoginStartResponseBody {
    handshake_id: Uuid,
    login_response: String,
    #[allow(dead_code)]
    expires_in_seconds: u64,
}

#[derive(Debug, Deserialize)]
struct LoginFinishResponseBody {
    bearer: String,
    expires_at: DateTime<Utc>,
    lease_id: Uuid,
}

#[derive(Debug, Deserialize)]
struct RegisterStartResponseBody {
    registration_response: String,
}

#[derive(Debug, Deserialize)]
struct RegisterFinishResponseBody {
    tenant: String,
    suite_version: u8,
}

// ---------------------------------------------------------------------------
// Tests (pure-function only — wire-level tests live under tests/ and
// are gated on `docker_available()`).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const SINGLE_CERT_PEM: &[u8] = b"
        -----BEGIN CERTIFICATE-----
        MIIBtjCCAVugAwIBAgITBmyf1XSXNmY/Owua2eiedgPySjAKBggqhkjOPQQDAjA5
        MQswCQYDVQQGEwJVUzEPMA0GA1UEChMGQW1hem9uMRkwFwYDVQQDExBBbWF6b24g
        Um9vdCBDQSAzMB4XDTE1MDUyNjAwMDAwMFoXDTQwMDUyNjAwMDAwMFowOTELMAkG
        A1UEBhMCVVMxDzANBgNVBAoTBkFtYXpvbjEZMBcGA1UEAxMQQW1hem9uIFJvb3Qg
        Q0EgMzBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABCmXp8ZBf8ANm+gBG1bG8lKl
        ui2yEujSLtf6ycXYqm0fc4E7O5hrOXwzpcVOho6AF2hiRVd9RFgdszflZwjrZt6j
        QjBAMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgGGMB0GA1UdDgQWBBSr
        ttvXBp43rDCGB5Fwx5zEGbF4wDAKBggqhkjOPQQDAgNJADBGAiEA4IWSoxe3jfkr
        BqWTrBqYaGFy+uGh0PsceGCmQ5nFuMQCIQCcAu/xlJyzlvnrxir4tiz+OpAUFteM
        YyRIHN8wfdVoOw==
        -----END CERTIFICATE-----
    ";

    const PEM_BUNDLE: &[u8] = b"
        -----BEGIN CERTIFICATE-----
        MIIBtjCCAVugAwIBAgITBmyf1XSXNmY/Owua2eiedgPySjAKBggqhkjOPQQDAjA5
        MQswCQYDVQQGEwJVUzEPMA0GA1UEChMGQW1hem9uMRkwFwYDVQQDExBBbWF6b24g
        Um9vdCBDQSAzMB4XDTE1MDUyNjAwMDAwMFoXDTQwMDUyNjAwMDAwMFowOTELMAkG
        A1UEBhMCVVMxDzANBgNVBAoTBkFtYXpvbjEZMBcGA1UEAxMQQW1hem9uIFJvb3Qg
        Q0EgMzBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABCmXp8ZBf8ANm+gBG1bG8lKl
        ui2yEujSLtf6ycXYqm0fc4E7O5hrOXwzpcVOho6AF2hiRVd9RFgdszflZwjrZt6j
        QjBAMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgGGMB0GA1UdDgQWBBSr
        ttvXBp43rDCGB5Fwx5zEGbF4wDAKBggqhkjOPQQDAgNJADBGAiEA4IWSoxe3jfkr
        BqWTrBqYaGFy+uGh0PsceGCmQ5nFuMQCIQCcAu/xlJyzlvnrxir4tiz+OpAUFteM
        YyRIHN8wfdVoOw==
        -----END CERTIFICATE-----
        -----BEGIN CERTIFICATE-----
        MIIB8jCCAXigAwIBAgITBmyf18G7EEwpQ+Vxe3ssyBrBDjAKBggqhkjOPQQDAzA5
        MQswCQYDVQQGEwJVUzEPMA0GA1UEChMGQW1hem9uMRkwFwYDVQQDExBBbWF6b24g
        Um9vdCBDQSA0MB4XDTE1MDUyNjAwMDAwMFoXDTQwMDUyNjAwMDAwMFowOTELMAkG
        A1UEBhMCVVMxDzANBgNVBAoTBkFtYXpvbjEZMBcGA1UEAxMQQW1hem9uIFJvb3Qg
        Q0EgNDB2MBAGByqGSM49AgEGBSuBBAAiA2IABNKrijdPo1MN/sGKe0uoe0ZLY7Bi
        9i0b2whxIdIA6GO9mif78DluXeo9pcmBqqNbIJhFXRbb/egQbeOc4OO9X4Ri83Bk
        M6DLJC9wuoihKqB1+IGuYgbEgds5bimwHvouXKNCMEAwDwYDVR0TAQH/BAUwAwEB
        /zAOBgNVHQ8BAf8EBAMCAYYwHQYDVR0OBBYEFNPsxzplbszh2naaVvuc84ZtV+WB
        MAoGCCqGSM49BAMDA2gAMGUCMDqLIfG9fhGt0O9Yli/W651+kI0rz2ZVwyzjKKlw
        CkcO8DdZEv8tmZQoTipPNU0zWgIxAOp1AE47xDqUEpHJWEadIRNyp4iciuRMStuW
        1KyLa2tJElMzrdfkviT8tQp21KW8EA==
        -----END CERTIFICATE-----
    ";

    fn write_temp_pem(contents: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("tempfile");
        use std::io::Write as _;
        let normalized = String::from_utf8_lossy(contents)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        file.write_all(normalized.as_bytes()).expect("write pem");
        file.write_all(b"\n").expect("final newline");
        file
    }

    /// Mirror of the status-mapping arms inside [`post_json_and_parse`];
    /// kept here so unit tests can pin the wire-error contract
    /// without a live broker.
    fn classify_status(status: u16, url: &str, body: &[u8]) -> Option<LoginError> {
        if (200..300).contains(&status) {
            return None;
        }
        if status == 401 && url.ends_with("/auth/login/finish") {
            return Some(LoginError::InvalidLogin("<unknown-tenant>".to_string()));
        }
        Some(LoginError::UnexpectedStatus {
            status,
            url: url.to_string(),
            body: truncate_body_bytes(body),
        })
    }

    #[test]
    fn truncate_body_caps_at_max() {
        let long = "x".repeat(2000);
        let truncated = truncate_body(long);
        assert!(truncated.ends_with('…'));
        // Bytes-wise: at most MAX + 4 (ellipsis is 3 bytes UTF-8).
        assert!(truncated.len() <= MAX_BODY_PREVIEW_BYTES + 4);
    }

    #[test]
    fn truncate_body_respects_char_boundaries() {
        // 256 multibyte chars × 3 bytes each → 768 bytes; truncating
        // at the 512-byte cap must not panic on a UTF-8 boundary.
        let s = "あ".repeat(256);
        let truncated = truncate_body(s);
        assert!(truncated.ends_with('…'));
        // The whole prefix must round-trip via from_utf8.
        let prefix = truncated.strip_suffix('…').unwrap();
        assert!(std::str::from_utf8(prefix.as_bytes()).is_ok());
    }

    #[test]
    fn status_mapping_invalidlogin_on_401_at_login_finish() {
        let err = classify_status(401, "http://x/auth/login/finish", b"").expect("401 must map");
        assert!(matches!(err, LoginError::InvalidLogin(_)), "got {err:?}");
    }

    #[test]
    fn status_mapping_unexpected_for_500() {
        let err = classify_status(500, "http://x/auth/check", b"boom").expect("500 must map");
        match err {
            LoginError::UnexpectedStatus { status, url, body } => {
                assert_eq!(status, 500);
                assert_eq!(url, "http://x/auth/check");
                assert_eq!(body, "boom");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn status_mapping_none_for_200() {
        assert!(classify_status(200, "http://x", b"{}").is_none());
        assert!(classify_status(201, "http://x", b"{}").is_none());
    }

    #[test]
    fn build_http_client_with_single_cert_pem_ok() {
        let file = write_temp_pem(SINGLE_CERT_PEM);
        let client = build_http_client_with_ca(Some(Path::new(file.path())));
        assert!(client.is_ok(), "expected success, got: {client:?}");
    }

    #[test]
    fn build_http_client_with_pem_bundle_ok() {
        let file = write_temp_pem(PEM_BUNDLE);
        let client = build_http_client_with_ca(Some(Path::new(file.path())));
        assert!(client.is_ok(), "expected success, got: {client:?}");
    }

    #[test]
    fn build_http_client_with_invalid_pem_is_actionable() {
        let file = write_temp_pem(b"not a pem");
        let err = build_http_client_with_ca(Some(Path::new(file.path()))).unwrap_err();
        match err {
            LoginError::CaCert(msg) => {
                assert!(
                    msg.contains("failed to parse PEM certificate(s) from")
                        || msg.contains("no valid PEM certificate found in")
                );
                assert!(msg.contains(&file.path().display().to_string()));
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    /// Verify the four endpoint URLs are built correctly from a base
    /// URL whether or not it has a trailing slash.
    #[test]
    fn endpoint_urls_are_built_correctly() {
        for base_str in &["http://127.0.0.1:9100", "http://127.0.0.1:9100/"] {
            let base = Url::parse(base_str).unwrap();
            assert_eq!(
                base.join("auth/login/start").unwrap().as_str(),
                "http://127.0.0.1:9100/auth/login/start",
                "login/start from {base_str}"
            );
            assert_eq!(
                base.join("auth/login/finish").unwrap().as_str(),
                "http://127.0.0.1:9100/auth/login/finish",
                "login/finish from {base_str}"
            );
            assert_eq!(
                base.join("auth/register/start").unwrap().as_str(),
                "http://127.0.0.1:9100/auth/register/start",
                "register/start from {base_str}"
            );
            assert_eq!(
                base.join("auth/register/finish").unwrap().as_str(),
                "http://127.0.0.1:9100/auth/register/finish",
                "register/finish from {base_str}"
            );
        }
    }
}
