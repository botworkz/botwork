//! `botwork-cli` error model + exit code mapping.
//!
//! The set is intentionally small and stable so consumers can branch
//! on the variant. Exit codes are documented in the crate README and
//! tracked here:
//!
//! | Variant                | Exit |
//! |------------------------|------|
//! | `InvalidLogin`         | 1    |
//! | `UnknownTenant`        | 1    |
//! | `AlreadyRegistered`    | 1    |
//! | `NoLease`              | 1    |
//! | `LeaseExpired`         | 1    |
//! | `Config(_)`            | 1    |
//! | `InvalidServer { .. }` | 1    |
//! | `Network { .. }`       | 2    |
//! | `UnexpectedStatus(..)` | 2    |
//! | `Keyring(_)`           | 3    |
//!
//! `Opaque(_)` collapses onto `InvalidLogin` when the
//! [`botwork_opaque_handshake::OpaqueError::InvalidLogin`] arm fires
//! (wrong password against a real tenant) and to exit 2 otherwise.

use thiserror::Error;

/// Top-level error type for every subcommand. The CLI converts this
/// into an exit code via [`exit_code_for`] and the library exposes
/// the variant directly so callers can branch.
#[derive(Debug, Error)]
pub enum LoginError {
    /// Wrong password against a tenant that exists on the server.
    /// The OPAQUE client surfaces this at `login_finish` time, before
    /// the broker ever sees the `login_finalization` for a real
    /// tenant. We surface it verbatim because that's the only
    /// password-was-wrong arm.
    #[error("incorrect password for tenant '{0}'")]
    InvalidLogin(String),

    /// The tenant does not exist on the server. Only emitted by
    /// `register` — `login` deliberately folds unknown tenants into
    /// the OPAQUE dummy flow for enumeration resistance, where the
    /// final wire shape is `InvalidLogin`.
    #[error("tenant '{0}' is not registered with this server; ask an operator to seed it")]
    UnknownTenant(String),

    /// `register` was run against a tenant that has already gone
    /// through OPAQUE registration. The broker returns 409.
    #[error("tenant '{0}' is already registered; use `login` instead of `register`")]
    AlreadyRegistered(String),

    /// `status` / `env` was run for a tenant with no keyring entry.
    #[error("no active lease for tenant '{0}'; run `bw --tenant {0}` first")]
    NoLease(String),

    /// `status` / `env` was run for a tenant whose lease has expired.
    /// The `expires_at` timestamp from the keyring entry is included
    /// so the caller can build a remediation message.
    #[error(
        "lease for tenant '{tenant}' expired at {expires_at}; run `bw --tenant {tenant}` again"
    )]
    LeaseExpired {
        /// Tenant name (carried through verbatim).
        tenant: String,
        /// `expires_at` from the keyring entry, RFC 3339-formatted by
        /// `chrono::DateTime<Utc>`.
        expires_at: String,
    },

    /// `reqwest` failed to reach the broker (DNS, TLS handshake,
    /// connection refused, …). The URL is captured so the user-facing
    /// message can say *which* server we couldn't reach.
    #[error("network error talking to {url}: {source}")]
    Network {
        /// Server URL the CLI was trying to talk to.
        url: String,
        /// Underlying `reqwest` error.
        #[source]
        source: reqwest::Error,
    },

    /// The broker returned a non-success status the CLI doesn't have
    /// a structured arm for. `url` names the endpoint; `body` carries
    /// the response body's first ~512 bytes for diagnostics.
    #[error("server returned unexpected status {status} from {url}: {body}")]
    UnexpectedStatus {
        /// HTTP status code (e.g. 500).
        status: u16,
        /// Endpoint URL we POSTed to.
        url: String,
        /// First ~512 bytes of the response body.
        body: String,
    },

    /// The OS keyring backend (`secret-service`, Keychain, Credential
    /// Manager) errored out and the file-fallback either failed too
    /// or wasn't applicable (non-Linux, or `HOME` unset).
    #[error("keyring error: {0}")]
    Keyring(#[from] keyring_storage_error::KeyringStorageError),

    /// Any error from [`botwork_opaque_handshake`] other than
    /// [`botwork_opaque_handshake::OpaqueError::InvalidLogin`] (which
    /// is mapped to [`Self::InvalidLogin`] at the call site).
    #[error("opaque error: {0}")]
    Opaque(#[from] botwork_opaque_handshake::OpaqueError),

    /// Config-file IO, TOML parse, or value-shape error. Carries a
    /// human-readable message — the structured arm differentiation
    /// belongs to the config module's internal errors.
    #[error("config error: {0}")]
    Config(String),

    /// The `--server` / `BOTWORK_LOGIN_SERVER` / config-file `server`
    /// value is missing a required scheme, has an unsupported scheme,
    /// or cannot be parsed as a valid absolute URL. A scheme of `http`
    /// or `https` is required; scheme-less values (e.g.
    /// `127.0.0.1:9100`, `example.com`) are rejected up front so the
    /// user sees an actionable message instead of an opaque network
    /// error.
    #[error("invalid --server value '{value}': {reason}")]
    InvalidServer {
        /// The raw value the user supplied (or the config/env value).
        value: String,
        /// Human-readable description of why the value was rejected.
        reason: String,
    },

    /// `--lease 7d` / `--lease 12h` / `--lease 600` failed
    /// [`humantime::parse_duration`].
    #[error("invalid `--lease` value '{value}': {reason}")]
    InvalidDuration {
        /// Raw value the user passed.
        value: String,
        /// `humantime` parser message.
        reason: String,
    },

    /// JSON parse of a broker response body failed. Distinct from
    /// [`Self::UnexpectedStatus`] — the status was 200 but the body
    /// didn't deserialise as the expected envelope.
    #[error("malformed response from {url}: {source}")]
    MalformedResponse {
        /// Endpoint URL.
        url: String,
        /// Underlying serde error.
        #[source]
        source: serde_json::Error,
    },

    /// Custom CA bundle path resolution / parse error.
    #[error("{0}")]
    CaCert(String),

    /// Catch-all for IO errors that don't fit any of the above
    /// (currently only emitted by the file-fallback keyring code
    /// path when neither `dirs` nor `HOME` lets us resolve a config
    /// directory).
    #[error("{0}")]
    Other(String),
}

/// Map a [`LoginError`] to an exit code.
///
/// The exit code is the primary thing scripts branch on, so the
/// taxonomy is intentionally small:
///
/// - **1** — user-recoverable: wrong password, no lease, expired
///   lease, missing tenant, malformed `--lease` flag, malformed
///   config.
/// - **2** — server / network: the operator probably has to look at
///   the broker.
/// - **3** — keyring backend: the OS keychain is unreachable or
///   refusing to write.
pub fn exit_code_for(err: &LoginError) -> i32 {
    match err {
        LoginError::InvalidLogin(_)
        | LoginError::UnknownTenant(_)
        | LoginError::AlreadyRegistered(_)
        | LoginError::NoLease(_)
        | LoginError::LeaseExpired { .. }
        | LoginError::InvalidDuration { .. }
        | LoginError::InvalidServer { .. }
        | LoginError::Config(_)
        | LoginError::CaCert(_) => 1,
        LoginError::Network { .. }
        | LoginError::UnexpectedStatus { .. }
        | LoginError::MalformedResponse { .. } => 2,
        LoginError::Keyring(_) => 3,
        LoginError::Opaque(err) => match err {
            botwork_opaque_handshake::OpaqueError::InvalidLogin => 1,
            _ => 2,
        },
        LoginError::Other(_) => 1,
    }
}

/// A thin newtype around [`keyring::Error`] plus the IO arm the
/// file-fallback raises. Kept in its own sub-module so the
/// `#[from]` impl in [`LoginError`] doesn't have to enumerate the
/// `keyring` crate's variants.
pub mod keyring_storage_error {
    use thiserror::Error;

    /// Combined error type covering both the OS-keyring backend and
    /// the file-fallback storage. The `Display` impl strips the
    /// implementation-defined backend name and emits a stable
    /// message the CLI can show to the user without leaking which
    /// arm tripped.
    #[derive(Debug, Error)]
    pub enum KeyringStorageError {
        /// Reading or writing the per-tenant lease file at
        /// `~/.config/botspace/keyring/<tenant>.json` failed.
        #[error("keyring file IO error: {0}")]
        Io(#[from] std::io::Error),
        /// Serialising / deserialising the keyring entry's JSON
        /// payload failed.
        #[error("keyring entry serialization error: {0}")]
        Serde(#[from] serde_json::Error),
        /// No usable storage directory: neither
        /// `BOTWORK_LOGIN_KEYRING_DIR` nor `XDG_CONFIG_HOME` nor
        /// `HOME` is set, or the tenant name is structurally
        /// unsafe (contains a path separator / `.` / `..`).
        #[error("no usable keyring backend: {0}")]
        NoBackend(String),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_categories() {
        assert_eq!(exit_code_for(&LoginError::InvalidLogin("t".into())), 1);
        assert_eq!(exit_code_for(&LoginError::NoLease("t".into())), 1);
        assert_eq!(
            exit_code_for(&LoginError::LeaseExpired {
                tenant: "t".into(),
                expires_at: "x".into()
            }),
            1
        );
        assert_eq!(
            exit_code_for(&LoginError::UnexpectedStatus {
                status: 500,
                url: "/x".into(),
                body: String::new(),
            }),
            2
        );
        assert_eq!(
            exit_code_for(&LoginError::InvalidDuration {
                value: "".into(),
                reason: "".into()
            }),
            1
        );
        assert_eq!(exit_code_for(&LoginError::Config("bad toml".into())), 1);
        assert_eq!(
            exit_code_for(&LoginError::InvalidServer {
                value: "127.0.0.1:80".into(),
                reason: "a scheme is required".into()
            }),
            1
        );
        assert_eq!(
            exit_code_for(&LoginError::Opaque(
                botwork_opaque_handshake::OpaqueError::InvalidLogin
            )),
            1
        );
        // Any other OpaqueError variant should land on 2 — pick a
        // wire-format failure as the representative.
        assert_eq!(
            exit_code_for(&LoginError::Opaque(
                botwork_opaque_handshake::OpaqueError::Serialization("synthetic")
            )),
            2
        );
    }

    #[test]
    fn display_messages_are_stable() {
        let err = LoginError::LeaseExpired {
            tenant: "phlax".into(),
            expires_at: "2026-07-18T16:00:00Z".into(),
        };
        assert_eq!(
            err.to_string(),
            "lease for tenant 'phlax' expired at 2026-07-18T16:00:00Z; run `bw --tenant phlax` again"
        );

        let invalid_server = LoginError::InvalidServer {
            value: "127.0.0.1:9100".into(),
            reason: "a scheme is required".into(),
        };
        assert_eq!(
            invalid_server.to_string(),
            "invalid --server value '127.0.0.1:9100': a scheme is required"
        );
    }

    #[test]
    fn keyring_storage_error_display_messages_are_stable() {
        assert_eq!(
            keyring_storage_error::KeyringStorageError::NoBackend("missing".into()).to_string(),
            "no usable keyring backend: missing"
        );
        assert!(keyring_storage_error::KeyringStorageError::Serde(
            serde_json::from_str::<serde_json::Value>("not-json").unwrap_err()
        )
        .to_string()
        .contains("keyring entry serialization error"));
    }
}
