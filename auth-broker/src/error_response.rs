use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// User-facing docs URL that auth-broker points clients at when an
/// authentication attempt fails. Static today; the per-tenant override knob
/// listed in issue #125 "out of scope" is a deliberate follow-up — keeping
/// this constant means a future config-driven path is a one-line change here
/// rather than a structural one.
pub const DOCS_URL: &str = "https://botspace.docs/auth";

/// Machine-readable taxonomy of 401 reasons returned by auth-broker.
///
/// The set is intentionally fixed by issue #125:
///
/// - `MissingBearer` / `InvalidBearer` cover the bearer-as-password world
///   (round 1a) and ARE emitted today.
/// - `ExpiredLease` / `RevokedLease` are part of the public schema so
///   downstream consumers can branch on them already, but are not emitted
///   yet — the lease lifecycle they describe lands with #123 round 1a.4 / 1b.
///   They are kept in this enum (rather than tacked on later) so the JSON
///   contract is stable across that future change: only the *set of codes
///   actually observed* widens, never the *shape* of the response.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    MissingBearer,
    InvalidBearer,
    // Reserved by #125 for the lease work in #123. Spec-frozen here so the
    // public response shape is stable when round 1a.4 / 1b start emitting
    // them; intentionally not used by any current call site.
    #[allow(dead_code)]
    ExpiredLease,
    #[allow(dead_code)]
    RevokedLease,
}

impl ErrorCode {
    /// Stable snake_case token used in both the JSON body's `error.code`
    /// and the RFC 6750 `WWW-Authenticate` `error=` parameter. Kept as a
    /// `const fn` (rather than going through serde) so the header build path
    /// is allocation-free for the static portion.
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorCode::MissingBearer => "missing_bearer",
            ErrorCode::InvalidBearer => "invalid_bearer",
            ErrorCode::ExpiredLease => "expired_lease",
            ErrorCode::RevokedLease => "revoked_lease",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Remediation {
    pub command: String,
    pub docs_url: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: &'static str,
    pub message: String,
    pub remediation: Remediation,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

impl ErrorResponse {
    /// Build the JSON body for a 401, naming `tenant` in both the message and
    /// the suggested `bw` command when it is known. The 401 paths in
    /// `/auth/check` that fail before path validation pass `tenant = None`;
    /// every other 401 should pass the tenant captured from
    /// `x-envoy-original-path`.
    pub fn build(code: ErrorCode, tenant: Option<&str>) -> Self {
        let command = match tenant {
            Some(tenant) => format!("bw --tenant {tenant}"),
            None => "bw".to_string(),
        };
        let message = build_message(code, tenant);

        Self {
            error: ErrorBody {
                code: code.as_str(),
                message,
                remediation: Remediation {
                    command,
                    docs_url: DOCS_URL,
                },
            },
        }
    }
}

fn build_message(code: ErrorCode, tenant: Option<&str>) -> String {
    let suffix = match tenant {
        Some(t) => format!(" for tenant '{t}'; run `bw --tenant {t}`."),
        None => "; run `bw`.".to_string(),
    };
    let head = match code {
        ErrorCode::MissingBearer => "missing or empty Authorization bearer",
        ErrorCode::InvalidBearer => "invalid Authorization bearer",
        ErrorCode::ExpiredLease => "lease has expired",
        ErrorCode::RevokedLease => "lease has been revoked",
    };
    format!("{head}{suffix}")
}

fn realm(tenant: Option<&str>) -> String {
    // RFC 6750 leaves realm content opaque to the client; we use it as a
    // human-readable scope hint ("botspace/<tenant>"). When the tenant is
    // unknown (bad path before validation), drop to the bare "botspace/"
    // prefix rather than guessing.
    match tenant {
        Some(t) => format!("botspace/{t}"),
        None => "botspace/".to_string(),
    }
}

/// RFC 6750 §3 `WWW-Authenticate: Bearer ...` value.
///
/// `realm` is `botspace/<tenant>` when known else `botspace/`. `error` is the
/// machine-readable `code` token. `error_description` carries the same
/// remediation text as `error.message` in the JSON body; the duplication is
/// deliberate so clients that only read the header still see the user-actionable
/// hint without having to parse the body.
fn www_authenticate_value(code: ErrorCode, tenant: Option<&str>, description: &str) -> String {
    format!(
        "Bearer realm=\"{}\", error=\"{}\", error_description=\"{}\"",
        escape_quoted_string(&realm(tenant)),
        code.as_str(),
        escape_quoted_string(description),
    )
}

/// Escape the backslash and double-quote characters that are not part of
/// RFC 7230's `quoted-string` token. The tenant segment is already restricted
/// to `[A-Za-z0-9._-]+` upstream and our internal messages are pure ASCII, so
/// in practice this is a belt-and-braces guard; it exists so future callers
/// who pass user-influenced strings don't accidentally produce a malformed
/// header value.
fn escape_quoted_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Build the canonical 401 response: status code, `Content-Type:
/// application/json`, the structured `ErrorResponse` body, and the RFC 6750
/// `WWW-Authenticate: Bearer ...` header. All 401 paths in the broker must go
/// through here so the contract is enforced in one place.
pub fn unauthorized(code: ErrorCode, tenant: Option<&str>) -> Response {
    let body = ErrorResponse::build(code, tenant);
    let header_value = www_authenticate_value(code, tenant, &body.error.message);

    let mut response = (StatusCode::UNAUTHORIZED, Json(body)).into_response();
    // `escape_quoted_string` above ensures the value is ASCII-safe for header
    // serialization, so `HeaderValue::from_str` cannot realistically fail.
    // Use `from_maybe_shared` semantics via `from_str` and fall back silently
    // rather than producing a 500 if somehow it does — the body still carries
    // the structured payload.
    if let Ok(value) = HeaderValue::from_str(&header_value) {
        response.headers_mut().insert("www-authenticate", value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_serializes_to_snake_case() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::MissingBearer).unwrap(),
            "\"missing_bearer\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::InvalidBearer).unwrap(),
            "\"invalid_bearer\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::ExpiredLease).unwrap(),
            "\"expired_lease\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::RevokedLease).unwrap(),
            "\"revoked_lease\""
        );
    }

    #[test]
    fn error_response_includes_tenant_in_command_when_known() {
        let body = ErrorResponse::build(ErrorCode::InvalidBearer, Some("phlax"));
        assert_eq!(body.error.code, "invalid_bearer");
        assert_eq!(body.error.remediation.command, "bw --tenant phlax");
        assert_eq!(body.error.remediation.docs_url, DOCS_URL);
        assert!(body.error.message.contains("tenant 'phlax'"));
        assert!(body.error.message.contains("bw --tenant phlax"));
    }

    #[test]
    fn error_response_omits_tenant_when_unknown() {
        let body = ErrorResponse::build(ErrorCode::MissingBearer, None);
        assert_eq!(body.error.code, "missing_bearer");
        assert_eq!(body.error.remediation.command, "bw");
        assert!(!body.error.message.contains("tenant '"));
        assert!(body.error.message.contains("bw"));
    }

    #[test]
    fn www_authenticate_uses_realm_and_error_params() {
        let value = www_authenticate_value(ErrorCode::InvalidBearer, Some("phlax"), "msg");
        assert_eq!(
            value,
            "Bearer realm=\"botspace/phlax\", error=\"invalid_bearer\", \
             error_description=\"msg\""
        );
    }

    #[test]
    fn www_authenticate_realm_drops_tenant_when_unknown() {
        let value = www_authenticate_value(ErrorCode::MissingBearer, None, "msg");
        assert!(value.starts_with("Bearer realm=\"botspace/\","));
        assert!(value.contains("error=\"missing_bearer\""));
    }

    #[test]
    fn escape_quoted_string_handles_quote_and_backslash() {
        assert_eq!(escape_quoted_string(r#"a"b\c"#), r#"a\"b\\c"#);
    }
}
