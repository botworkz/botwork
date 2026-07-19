use std::collections::HashSet;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http1;
use hyper::{Method, Request, Uri};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::{log_info, redact_token};

pub(crate) const MAX_ENV_ENTRIES: usize = 64;
pub(crate) const MAX_ENV_VALUE_BYTES: usize = 64 * 1024;
pub(crate) const SECRET_ENV_PREFIX: &str = "BOTWORK_SECRET_";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedSecret {
    pub service: String,
    pub name: String,
    pub kind: String,
    pub value: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("bad response: {0}")]
    BadResponse(String),
    #[error("transport error: {0}")]
    Transport(String),
}

#[derive(Debug, Deserialize)]
struct FetchSecretsResponse {
    secrets: Vec<FetchSecretItem>,
}

#[derive(Debug, Deserialize)]
struct FetchSecretItem {
    service: String,
    name: String,
    kind: String,
    value_b64: String,
}

pub async fn fetch_secrets(
    broker_url: &str,
    cap: &str,
    request_timeout: Duration,
) -> Result<Vec<FetchedSecret>, SecretsError> {
    let endpoint = format!("{}/secrets/fetch", broker_url.trim_end_matches('/'));
    log_info(&format!(
        "secrets/fetch: calling auth-broker cap={} url={endpoint}",
        redact_token(cap)
    ));

    let result = async {
        let uri: Uri = endpoint
            .parse()
            .map_err(|e| SecretsError::Transport(format!("invalid auth broker URL: {e}")))?;
        let host = uri
            .host()
            .ok_or_else(|| SecretsError::Transport("missing host in auth broker URL".to_string()))?
            .to_string();
        let port = uri.port_u16().unwrap_or(80);
        let authority = if let Some(explicit_port) = uri.port_u16() {
            format!("{host}:{explicit_port}")
        } else {
            host.clone()
        };
        let path = uri
            .path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_else(|| "/secrets/fetch".to_string());

        let mut sender = timeout(request_timeout, async {
            let stream = TcpStream::connect((host.as_str(), port))
                .await
                .map_err(|e| SecretsError::Transport(e.to_string()))?;
            let io = TokioIo::new(stream);
            let (sender, conn) = http1::handshake(io)
                .await
                .map_err(|e| SecretsError::Transport(e.to_string()))?;
            tokio::spawn(async move {
                if let Err(err) = conn.await {
                    log_info(&format!("auth-broker HTTP connection error: {err}"));
                }
            });
            Ok::<_, SecretsError>(sender)
        })
        .await
        .map_err(|e| SecretsError::Transport(e.to_string()))??;

        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("http://{authority}{path}"))
            .header("Host", authority)
            .header("x-botwork-cap", cap)
            .header("Content-Length", "0")
            .body(Full::new(Bytes::new()))
            .map_err(|e| SecretsError::Transport(e.to_string()))?;

        let response = timeout(request_timeout, sender.send_request(request))
            .await
            .map_err(|e| SecretsError::Transport(e.to_string()))?
            .map_err(|e| SecretsError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        let response_body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| SecretsError::Transport(e.to_string()))?
            .to_bytes()
            .to_vec();

        if status == 401 {
            return Err(SecretsError::Unauthorized);
        }
        if status != 200 {
            return Err(SecretsError::BadResponse(format!(
                "HTTP {status}: {}",
                String::from_utf8_lossy(&response_body).trim()
            )));
        }

        let parsed: FetchSecretsResponse = serde_json::from_slice(&response_body)
            .map_err(|e| SecretsError::BadResponse(format!("invalid JSON: {e}")))?;
        parsed
            .secrets
            .into_iter()
            .map(|secret| {
                let value = STANDARD
                    .decode(secret.value_b64.as_bytes())
                    .map_err(|e| SecretsError::BadResponse(format!("invalid base64 value: {e}")))?;
                Ok(FetchedSecret {
                    service: secret.service,
                    name: secret.name,
                    kind: secret.kind,
                    value,
                })
            })
            .collect::<Result<Vec<_>, _>>()
    }
    .await;

    match &result {
        Ok(secrets) => {
            let services = secret_services(secrets);
            log_info(&format!(
                "secrets/fetch: ok secrets={} services=[{}]",
                secrets.len(),
                services.join(",")
            ));
        }
        Err(SecretsError::Unauthorized) => {
            log_info("secrets/fetch: error variant=SecretsError::Unauthorized");
        }
        Err(SecretsError::Transport(err)) => {
            log_info(&format!(
                "secrets/fetch: error variant=SecretsError::Transport err={err}"
            ));
        }
        Err(SecretsError::BadResponse(err)) => {
            log_info(&format!(
                "secrets/fetch: error variant=SecretsError::BadResponse err={err}"
            ));
        }
    }

    result
}

fn secret_services(secrets: &[FetchedSecret]) -> Vec<String> {
    let mut services: Vec<String> = secrets
        .iter()
        .map(|secret| secret.service.clone())
        .collect();
    services.sort();
    services.dedup();
    services
}

/// Maps auth-broker secrets to launcher env entries:
///
/// - Name: `BOTWORK_SECRET_<SANITIZED_SERVICE>_<SANITIZED_NAME>`
/// - Value: decoded bytes interpreted with `String::from_utf8_lossy`
///
/// Sanitization is byte-wise:
/// - ASCII letters are upper-cased
/// - ASCII digits are preserved
/// - every other byte becomes `_`
///
/// Duplicates (after sanitization) keep first value and drop later entries.
/// Output preserves input order, is capped at 64 entries, and skips values over 64 KiB.
pub fn build_env_entries(secrets: &[FetchedSecret]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for secret in secrets {
        if out.len() >= MAX_ENV_ENTRIES {
            log_info("warn: secrets env entries exceed 64; truncating");
            break;
        }
        if secret.value.len() > MAX_ENV_VALUE_BYTES {
            log_info(&format!(
                "warn: secret env value too large; skipping service={} name={}",
                secret.service, secret.name
            ));
            continue;
        }
        let env_name = format!(
            "{SECRET_ENV_PREFIX}{}_{}",
            sanitize_segment(&secret.service),
            sanitize_segment(&secret.name)
        );
        if !seen.insert(env_name.clone()) {
            log_info(&format!(
                "warn: duplicate secret env name {}; keeping first value",
                env_name
            ));
            continue;
        }
        out.push((
            env_name,
            String::from_utf8_lossy(&secret.value).into_owned(),
        ));
    }

    out
}

fn sanitize_segment(input: &str) -> String {
    input
        .as_bytes()
        .iter()
        .map(|b| {
            if b.is_ascii_alphabetic() {
                b.to_ascii_uppercase() as char
            } else if b.is_ascii_digit() {
                *b as char
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{log_capture_guard, start_log_capture, take_log_capture};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    fn secret(service: &str, name: &str, value: &[u8]) -> FetchedSecret {
        FetchedSecret {
            service: service.to_string(),
            name: name.to_string(),
            kind: "api-key".to_string(),
            value: value.to_vec(),
        }
    }

    #[test]
    fn build_env_entries_sanitises_service_and_name() {
        let entries = build_env_entries(&[
            secret("github.com", "pat", b"1"),
            secret("shared", "secret", b"2"),
            secret("npm-registry", "token", b"3"),
            secret("9-leading-digit", "x", b"4"),
        ]);

        let names: Vec<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "BOTWORK_SECRET_GITHUB_COM_PAT",
                "BOTWORK_SECRET_SHARED_SECRET",
                "BOTWORK_SECRET_NPM_REGISTRY_TOKEN",
                "BOTWORK_SECRET_9_LEADING_DIGIT_X",
            ]
        );
    }

    #[test]
    fn build_env_entries_handles_collisions_first_wins() {
        let entries = build_env_entries(&[
            secret("github.com", "pat", b"first"),
            secret("github/com", "pat", b"second"),
        ]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "BOTWORK_SECRET_GITHUB_COM_PAT");
        assert_eq!(entries[0].1, "first");
    }

    #[test]
    fn build_env_entries_truncates_at_64() {
        let secrets: Vec<FetchedSecret> = (0..65)
            .map(|idx| secret("service", &format!("name{idx}"), b"v"))
            .collect();
        let entries = build_env_entries(&secrets);
        assert_eq!(entries.len(), 64);
    }

    #[test]
    fn build_env_entries_skips_oversized_values() {
        let entries = build_env_entries(&[
            secret("a", "ok", b"ok"),
            secret("a", "too-big", &vec![b'a'; (64 * 1024) + 1]),
        ]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "BOTWORK_SECRET_A_OK");
    }

    #[test]
    fn build_env_entries_preserves_input_order() {
        let entries = build_env_entries(&[
            secret("svc", "b", b"1"),
            secret("svc", "a", b"2"),
            secret("svc", "c", b"3"),
        ]);
        let names: Vec<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "BOTWORK_SECRET_SVC_B",
                "BOTWORK_SECRET_SVC_A",
                "BOTWORK_SECRET_SVC_C",
            ]
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn fetch_secrets_decodes_value_b64() {
        let _guard = log_capture_guard();
        let captured_cap = Arc::new(Mutex::new(None));
        let url = spawn_http_server(
            200,
            r#"{"tenant":"phlax","plugin":"exec-bash","secrets":[{"service":"github.com","name":"pat","kind":"api-key","value_b64":"Z2hwX3h4eA=="}]}"#,
            Arc::clone(&captured_cap),
        )
        .await;

        start_log_capture();
        let secrets = fetch_secrets(&url, "cap-123", Duration::from_secs(2))
            .await
            .expect("fetch succeeds");
        let logs = take_log_capture().join("\n");

        assert_eq!(secrets.len(), 1);
        assert_eq!(secrets[0].service, "github.com");
        assert_eq!(secrets[0].name, "pat");
        assert_eq!(secrets[0].value, b"ghp_xxx");
        assert_eq!(
            captured_cap.lock().await.clone().as_deref(),
            Some("cap-123")
        );
        assert!(
            logs.contains("secrets/fetch: ok secrets=1 services=[github.com]"),
            "missing success summary log: {logs}"
        );
        assert!(
            logs.contains(&format!(
                "secrets/fetch: calling auth-broker cap={}",
                redact_token("cap-123")
            )),
            "missing call log with redacted cap: {logs}"
        );
        assert!(
            !logs.contains("ghp_xxx"),
            "logs should not contain decoded secret values: {logs}"
        );
        assert!(
            !logs.contains("Z2hwX3h4eA=="),
            "logs should not contain value_b64: {logs}"
        );
        assert!(
            !logs.contains("cap-123"),
            "logs should not contain full capability token: {logs}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn fetch_secrets_returns_unauthorized_for_401() {
        let _guard = log_capture_guard();
        let url = spawn_http_server(401, "{}", Arc::new(Mutex::new(None))).await;
        start_log_capture();
        let err = fetch_secrets(&url, "cap", Duration::from_secs(2))
            .await
            .expect_err("expected unauthorized");
        let logs = take_log_capture().join("\n");
        assert!(matches!(err, SecretsError::Unauthorized));
        assert!(
            logs.contains("variant=SecretsError::Unauthorized"),
            "missing unauthorized variant log: {logs}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn fetch_secrets_returns_bad_response_for_non_json() {
        let _guard = log_capture_guard();
        let url = spawn_http_server(200, "not-json", Arc::new(Mutex::new(None))).await;
        start_log_capture();
        let err = fetch_secrets(&url, "cap", Duration::from_secs(2))
            .await
            .expect_err("expected bad response");
        let logs = take_log_capture().join("\n");
        assert!(matches!(err, SecretsError::BadResponse(_)));
        assert!(
            logs.contains("variant=SecretsError::BadResponse"),
            "missing bad response variant log: {logs}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn fetch_secrets_returns_transport_for_unreachable_url() {
        let _guard = log_capture_guard();
        start_log_capture();
        let err = fetch_secrets("http://127.0.0.1:1", "cap", Duration::from_secs(1))
            .await
            .expect_err("expected transport error");
        let logs = take_log_capture().join("\n");
        assert!(matches!(err, SecretsError::Transport(_)));
        assert!(
            logs.contains("variant=SecretsError::Transport"),
            "missing transport variant log: {logs}"
        );
    }

    async fn spawn_http_server(
        status_code: u16,
        body: &'static str,
        captured_cap: Arc<Mutex<Option<String>>>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let request = read_http_request(&mut stream).await;
            let cap = extract_header(&request, "x-botwork-cap");
            *captured_cap.lock().await = cap;
            let reason = if status_code == 200 {
                "OK"
            } else {
                "Unauthorized"
            };
            let response = format!(
                "HTTP/1.1 {status_code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        format!("http://{addr}")
    }

    async fn read_http_request(stream: &mut TcpStream) -> String {
        let mut raw = Vec::new();
        let mut buf = [0_u8; 1024];
        let mut expected_total = None;
        loop {
            let read = stream.read(&mut buf).await.expect("read request");
            if read == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..read]);
            if expected_total.is_none() {
                if let Some((header_end, content_len)) = parse_header_end_and_length(&raw) {
                    expected_total = Some(header_end + 4 + content_len);
                }
            }
            if let Some(total) = expected_total {
                if raw.len() >= total {
                    break;
                }
            }
        }
        String::from_utf8(raw).expect("utf8 request")
    }

    fn parse_header_end_and_length(raw: &[u8]) -> Option<(usize, usize)> {
        let header_end = raw.windows(4).position(|chunk| chunk == b"\r\n\r\n")?;
        let headers = String::from_utf8(raw[..header_end].to_vec()).ok()?;
        let content_length = headers
            .split("\r\n")
            .find_map(|line| {
                line.split_once(": ").and_then(|(name, value)| {
                    if name.eq_ignore_ascii_case("content-length") {
                        value.parse::<usize>().ok()
                    } else {
                        None
                    }
                })
            })
            .unwrap_or(0);
        Some((header_end, content_length))
    }

    fn extract_header(request: &str, header_name: &str) -> Option<String> {
        request.split("\r\n").find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                if name.eq_ignore_ascii_case(header_name) {
                    Some(value.trim().to_string())
                } else {
                    None
                }
            })
        })
    }
}
