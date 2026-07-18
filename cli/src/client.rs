//! Wire client: drives `botwork_opaque_handshake` against the
//! broker's `/auth/{register,login}/{start,finish}` endpoints.
//!
//! Layered as plain `pub async fn`s rather than a `Client` struct so
//! consumers (the CLI subcommands today, a future web/admin UI
//! tomorrow) compose them at will. The `reqwest::Client` is created
//! once per call — the round-1a hot path is one
//! `botwork-cli` invocation per ~7-day lease, so connection-pool
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
    client as opaque_client, LoginFinalization, LoginRequest, LoginResponse, OpaqueError,
    RegistrationRequest, RegistrationResponse, RegistrationUpload, SessionKey,
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
    let start_body = login_start_body(tenant, credential_identifier, &cl.request, lease_seconds);
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
    let finish_body = login_finish_body(body.handshake_id, &finish.finalization);
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
/// CLI exposes it under `bw register`. The broker's
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
    let start_body = register_start_body(tenant, credential_identifier, &cr.request);
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
    let finish_body = register_finish_body(tenant, credential_identifier, &cf.upload);
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

fn login_start_body(
    tenant: &str,
    credential_identifier: &str,
    login_request: &LoginRequest,
    lease_seconds: u64,
) -> serde_json::Value {
    json!({
        "tenant": tenant,
        "credential_identifier": credential_identifier,
        "login_request": URL_SAFE_NO_PAD.encode(login_request.serialize()),
        "lease_seconds_requested": lease_seconds,
    })
}

fn login_finish_body(handshake_id: Uuid, finalization: &LoginFinalization) -> serde_json::Value {
    json!({
        "handshake_id": handshake_id,
        "login_finalization": URL_SAFE_NO_PAD.encode(finalization.serialize()),
    })
}

fn register_start_body(
    tenant: &str,
    credential_identifier: &str,
    registration_request: &RegistrationRequest,
) -> serde_json::Value {
    json!({
        "tenant": tenant,
        "credential_identifier": credential_identifier,
        "registration_request": URL_SAFE_NO_PAD.encode(registration_request.serialize()),
    })
}

fn register_finish_body(
    tenant: &str,
    credential_identifier: &str,
    registration_upload: &RegistrationUpload,
) -> serde_json::Value {
    json!({
        "tenant": tenant,
        "credential_identifier": credential_identifier,
        "registration_upload": URL_SAFE_NO_PAD.encode(registration_upload.serialize()),
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
    if let Some(err) = login_status_error(status, url, &bytes) {
        return Err(err);
    }
    parse_json_body(url, &bytes)
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
    if let Some(err) = register_status_error(status, url, &bytes, tenant) {
        return Err(err);
    }
    parse_json_body(url, &bytes)
}

fn parse_json_body<T: for<'de> Deserialize<'de>>(url: &str, bytes: &[u8]) -> Result<T, LoginError> {
    serde_json::from_slice(bytes).map_err(|source| LoginError::MalformedResponse {
        url: url.to_string(),
        source,
    })
}

fn login_status_error(status: reqwest::StatusCode, url: &str, body: &[u8]) -> Option<LoginError> {
    if status.is_success() {
        return None;
    }
    // 401 on `/auth/login/finish` is the OPAQUE-server-side
    // `InvalidLogin` arm — see the comment in `run_login` for the
    // double-detection rationale.
    if status == reqwest::StatusCode::UNAUTHORIZED && url.ends_with("/auth/login/finish") {
        return Some(LoginError::InvalidLogin("<unknown-tenant>".to_string()));
    }
    Some(LoginError::UnexpectedStatus {
        status: status.as_u16(),
        url: url.to_string(),
        body: truncate_body_bytes(body),
    })
}

fn register_status_error(
    status: reqwest::StatusCode,
    url: &str,
    body: &[u8],
    tenant: &str,
) -> Option<LoginError> {
    if status.is_success() {
        return None;
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Some(LoginError::UnknownTenant(tenant.to_string()));
    }
    if status == reqwest::StatusCode::CONFLICT {
        return Some(LoginError::AlreadyRegistered(tenant.to_string()));
    }
    Some(LoginError::UnexpectedStatus {
        status: status.as_u16(),
        url: url.to_string(),
        body: truncate_body_bytes(body),
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
    use botwork_opaque_handshake::{client, server, ServerSetup};
    use std::path::Path;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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

    #[derive(Debug, Deserialize)]
    struct Tiny {
        value: u8,
    }

    fn fixture_input() -> Vec<u8> {
        [104_u8, 117, 110, 116, 101, 114, 50].to_vec()
    }

    fn fixture_registration() -> (RegistrationRequest, RegistrationUpload) {
        let mut rng = rand::thread_rng();
        let setup = ServerSetup::generate(&mut rng);
        let input = fixture_input();
        let start = client::registration_start(&mut rng, input.as_slice()).unwrap();
        let request = start.request.clone();
        let response =
            server::registration_start(&setup, request.clone(), b"phlax@example.com").unwrap();
        let finish =
            client::registration_finish(&mut rng, start.state, input.as_slice(), response.response)
                .unwrap();
        (request, finish.upload)
    }

    fn fixture_login() -> (LoginRequest, LoginFinalization) {
        let mut rng = rand::thread_rng();
        let setup = ServerSetup::generate(&mut rng);
        let input = fixture_input();
        let registration = client::registration_start(&mut rng, input.as_slice()).unwrap();
        let response =
            server::registration_start(&setup, registration.request, b"phlax@example.com").unwrap();
        let finish = client::registration_finish(
            &mut rng,
            registration.state,
            input.as_slice(),
            response.response,
        )
        .unwrap();
        let password_file = server::registration_finish(finish.upload);
        let login = client::login_start(&mut rng, input.as_slice()).unwrap();
        let request = login.request.clone();
        let response = server::login_start(
            &mut rng,
            &setup,
            Some(&password_file),
            request.clone(),
            b"phlax@example.com",
        )
        .unwrap();
        let finish =
            client::login_finish(login.state, input.as_slice(), response.response).unwrap();
        (request, finish.finalization)
    }

    async fn spawn_one_shot_http(status: &str, body: &str, content_type: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let body = body.to_string();
        let status = status.to_string();
        let content_type = content_type.to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = [0_u8; 4096];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        format!("http://{addr}")
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
        let err = login_status_error(
            reqwest::StatusCode::UNAUTHORIZED,
            "http://x/auth/login/finish",
            b"",
        )
        .expect("401 must map");
        assert!(matches!(err, LoginError::InvalidLogin(_)), "got {err:?}");
    }

    #[test]
    fn status_mapping_unexpected_for_500() {
        let err = login_status_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "http://x/auth/check",
            b"boom",
        )
        .expect("500 must map");
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
        assert!(login_status_error(reqwest::StatusCode::OK, "http://x", b"{}").is_none());
        assert!(login_status_error(reqwest::StatusCode::CREATED, "http://x", b"{}").is_none());
    }

    #[test]
    fn status_mapping_401_on_other_endpoint_is_unexpected() {
        let err = login_status_error(
            reqwest::StatusCode::UNAUTHORIZED,
            "http://x/auth/login/start",
            b"denied",
        )
        .expect("401 must map");
        assert!(
            matches!(err, LoginError::UnexpectedStatus { status: 401, .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn register_status_mapping_special_cases() {
        assert!(matches!(
            register_status_error(reqwest::StatusCode::NOT_FOUND, "http://x", b"", "phlax"),
            Some(LoginError::UnknownTenant(ref tenant)) if tenant == "phlax"
        ));
        assert!(matches!(
            register_status_error(reqwest::StatusCode::CONFLICT, "http://x", b"", "phlax"),
            Some(LoginError::AlreadyRegistered(ref tenant)) if tenant == "phlax"
        ));
        assert!(register_status_error(reqwest::StatusCode::OK, "http://x", b"", "phlax").is_none());
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

    #[test]
    fn build_http_client_with_missing_pem_is_actionable() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("missing.pem");
        let err = build_http_client_with_ca(Some(path.as_path())).unwrap_err();
        assert!(
            matches!(err, LoginError::CaCert(ref msg) if msg.contains("failed to read --cacert file")),
            "got {err:?}"
        );
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

    #[test]
    fn login_request_bodies_encode_expected_fields() {
        let (request, finalization) = fixture_login();
        let handshake_id = Uuid::new_v4();

        let start_body = login_start_body("phlax", "phlax@example.com", &request, 604_800);
        assert_eq!(start_body["tenant"], "phlax");
        assert_eq!(start_body["credential_identifier"], "phlax@example.com");
        assert_eq!(start_body["lease_seconds_requested"], 604_800);
        assert_eq!(
            start_body["login_request"],
            URL_SAFE_NO_PAD.encode(request.serialize())
        );

        let finish_body = login_finish_body(handshake_id, &finalization);
        assert_eq!(finish_body["handshake_id"], handshake_id.to_string());
        assert_eq!(
            finish_body["login_finalization"],
            URL_SAFE_NO_PAD.encode(finalization.serialize())
        );
    }

    #[test]
    fn register_request_bodies_encode_expected_fields() {
        let (request, upload) = fixture_registration();
        let start_body = register_start_body("phlax", "phlax@example.com", &request);
        assert_eq!(start_body["tenant"], "phlax");
        assert_eq!(start_body["credential_identifier"], "phlax@example.com");
        assert_eq!(
            start_body["registration_request"],
            URL_SAFE_NO_PAD.encode(request.serialize())
        );

        let finish_body = register_finish_body("phlax", "phlax@example.com", &upload);
        assert_eq!(finish_body["tenant"], "phlax");
        assert_eq!(finish_body["credential_identifier"], "phlax@example.com");
        assert_eq!(
            finish_body["registration_upload"],
            URL_SAFE_NO_PAD.encode(upload.serialize())
        );
    }

    #[test]
    fn parse_json_body_success_and_error_are_shaped() {
        let parsed: Tiny = parse_json_body("http://x", br#"{"value":7}"#).unwrap();
        assert_eq!(parsed.value, 7);

        let err = parse_json_body::<Tiny>("http://x", b"not-json").unwrap_err();
        assert!(matches!(err, LoginError::MalformedResponse { ref url, .. } if url == "http://x"));
    }

    #[test]
    fn decode_b64_success_and_error_are_shaped() {
        let decoded = decode_b64("http://x", "field", &URL_SAFE_NO_PAD.encode(b"hello")).unwrap();
        assert_eq!(decoded, b"hello");

        let err = decode_b64("http://x", "field", "%%%").unwrap_err();
        match err {
            LoginError::MalformedResponse { url, source } => {
                assert_eq!(url, "http://x");
                assert!(source
                    .to_string()
                    .contains("`field` is not valid url-safe-base64"));
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_json_and_parse_covers_success_status_and_decode_failure_paths() {
        let client = reqwest::Client::new();
        let body = serde_json::json!({"ignored": true});

        let ok_url = spawn_one_shot_http("200 OK", r#"{"value":9}"#, "application/json").await;
        let parsed: Tiny = post_json_and_parse(&client, &ok_url, &body)
            .await
            .expect("parse ok response");
        assert_eq!(parsed.value, 9);

        let status_url =
            spawn_one_shot_http("500 Internal Server Error", "down", "text/plain").await;
        let err = post_json_and_parse::<Tiny>(&client, &status_url, &body)
            .await
            .expect_err("500 should fail");
        assert!(matches!(
            err,
            LoginError::UnexpectedStatus { status: 500, .. }
        ));

        let malformed_url = spawn_one_shot_http("200 OK", "not-json", "text/plain").await;
        let err = post_json_and_parse::<Tiny>(&client, &malformed_url, &body)
            .await
            .expect_err("malformed body should fail");
        assert!(matches!(err, LoginError::MalformedResponse { .. }));
    }

    #[tokio::test]
    async fn post_json_and_parse_with_tenant_arms_maps_special_statuses() {
        let client = reqwest::Client::new();
        let body = serde_json::json!({"ignored": true});

        let missing_url = spawn_one_shot_http("404 Not Found", "missing", "text/plain").await;
        let err =
            post_json_and_parse_with_tenant_arms::<Tiny>(&client, &missing_url, &body, "phlax")
                .await
                .expect_err("404 should map");
        assert!(matches!(err, LoginError::UnknownTenant(ref t) if t == "phlax"));

        let conflict_url = spawn_one_shot_http("409 Conflict", "exists", "text/plain").await;
        let err =
            post_json_and_parse_with_tenant_arms::<Tiny>(&client, &conflict_url, &body, "phlax")
                .await
                .expect_err("409 should map");
        assert!(matches!(err, LoginError::AlreadyRegistered(ref t) if t == "phlax"));

        let unexpected_url = spawn_one_shot_http("418 I'm a teapot", "teapot", "text/plain").await;
        let err =
            post_json_and_parse_with_tenant_arms::<Tiny>(&client, &unexpected_url, &body, "phlax")
                .await
                .expect_err("other status should map to unexpected");
        assert!(matches!(
            err,
            LoginError::UnexpectedStatus { status: 418, .. }
        ));
    }
}
