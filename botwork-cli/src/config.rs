//! Resolved config: CLI flag > env > config file > built-in default.
//!
//! ## File location
//!
//! - `$BOTWORK_LOGIN_CONFIG` if set (used by tests + power users).
//! - Else `$XDG_CONFIG_HOME/botspace/config.toml` if `XDG_CONFIG_HOME`
//!   is set.
//! - Else `~/.config/botspace/config.toml`.
//! - Missing file → fall back to built-in defaults (not an error).
//!
//! ## TOML shape
//!
//! ```toml
//! server = "http://192.168.122.50:9100"
//! token_env = "BOTWORK_BEARER"
//!
//! [tenants.phlax]
//! credential_identifier = "phlax"
//! ```
//!
//! Every field is optional. Per-tenant overrides default the
//! `credential_identifier` to the tenant name itself.
//!
//! ## Server URL requirements
//!
//! The `server` value — whether supplied via `--server`, the
//! `BOTWORK_LOGIN_SERVER` environment variable, the config file, or
//! taken from the built-in [`DEFAULT_SERVER`] — **must** include an
//! explicit `http://` or `https://` scheme. Scheme-less values such
//! as `127.0.0.1:9100` or `example.com` are rejected at resolution
//! time with a [`crate::error::LoginError::InvalidServer`] error
//! that names the offending value and suggests the fix. No scheme is
//! ever silently prepended.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::LoginError;

/// Default broker URL for development against a same-host broker.
/// Production deploys override via `BOTWORK_LOGIN_SERVER`.
pub const DEFAULT_SERVER: &str = "http://127.0.0.1:9100";
/// Default token env var name for `bw env`'s `export …`
/// line. `BOTWORK_BEARER` is the canonical name goose's extension
/// config substitutes via `${BOTWORK_BEARER}`.
pub const DEFAULT_TOKEN_ENV: &str = "BOTWORK_BEARER";
/// Default lease window for the `--lease` flag. 7 days = 604_800 s.
pub const DEFAULT_LEASE_SECONDS: u64 = 7 * 86_400;
/// Environment variable that overrides the configured broker URL.
pub const ENV_SERVER: &str = "BOTWORK_LOGIN_SERVER";
/// Environment variable that points the CLI at a non-default config
/// file path. Used by the integration tests so they don't have to
/// scribble on the user's `$XDG_CONFIG_HOME`.
pub const ENV_CONFIG_PATH: &str = "BOTWORK_LOGIN_CONFIG";

/// On-disk config shape. All fields optional so a missing file or a
/// completely empty file is a valid configuration.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Default broker URL — used when `--server` isn't passed and
    /// `BOTWORK_LOGIN_SERVER` env var isn't set.
    pub server: Option<String>,
    /// Name of the env var the `env` subcommand emits an `export
    /// =…` line under. Default `BOTWORK_BEARER`.
    pub token_env: Option<String>,
    /// Per-tenant overrides. Map key is the tenant name.
    pub tenants: BTreeMap<String, TenantConfig>,
}

/// Per-tenant override. The only knob today is the OPAQUE
/// `credential_identifier` — tenant policy lives server-side.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TenantConfig {
    /// OPAQUE credential identifier. Defaults to the tenant name.
    pub credential_identifier: Option<String>,
}

impl Config {
    /// Load the resolved config from the standard search path, or
    /// return [`Config::default()`] if no file is found.
    ///
    /// `$BOTWORK_LOGIN_CONFIG` takes precedence over the XDG /
    /// `$HOME` fallbacks; callers that want to pin a specific file
    /// should set that env var and let this function do the rest.
    pub fn load() -> Result<Self, LoginError> {
        let path = match resolve_config_path() {
            Some(path) => path,
            None => return Ok(Self::default()),
        };
        Self::load_from(&path)
    }

    /// Read + parse a config from a specific file. Errors carry the
    /// path so the user-visible message can pinpoint the failure.
    pub fn load_from(path: &Path) -> Result<Self, LoginError> {
        match std::fs::read_to_string(path) {
            Ok(bytes) => toml::from_str(&bytes).map_err(|err| {
                LoginError::Config(format!("failed to parse {}: {err}", path.display()))
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(LoginError::Config(format!(
                "failed to read {}: {err}",
                path.display()
            ))),
        }
    }

    /// Resolve and validate the broker URL.
    ///
    /// Order: CLI flag > `BOTWORK_LOGIN_SERVER` > `server` in the
    /// config file > built-in [`DEFAULT_SERVER`].
    ///
    /// The resolved value is parsed and validated: it must be an
    /// absolute URL with an explicit `http` or `https` scheme.
    /// Scheme-less values (e.g. `127.0.0.1:9100`) are rejected with
    /// [`LoginError::InvalidServer`] rather than silently rewritten —
    /// no scheme is ever prepended. Only `http` and `https` are
    /// accepted; any other scheme is also rejected.
    pub fn resolve_server(&self, cli_value: Option<&str>) -> Result<Url, LoginError> {
        let raw = if let Some(value) = cli_value {
            value.trim().to_string()
        } else if let Ok(value) = std::env::var(ENV_SERVER) {
            if !value.is_empty() {
                value
            } else {
                self.server
                    .clone()
                    .unwrap_or_else(|| DEFAULT_SERVER.to_string())
            }
        } else {
            self.server
                .clone()
                .unwrap_or_else(|| DEFAULT_SERVER.to_string())
        };
        validate_server_url(&raw)
    }

    /// Resolve the env var name `bw env` emits.
    pub fn resolve_token_env(&self) -> String {
        self.token_env
            .clone()
            .unwrap_or_else(|| DEFAULT_TOKEN_ENV.to_string())
    }

    /// Resolve the OPAQUE credential identifier for a tenant.
    ///
    /// CLI flag overrides the per-tenant config; per-tenant config
    /// overrides the default-to-tenant-name fallback. Both server
    /// and client must agree on this value across registration /
    /// login — see [`botwork_opaque_handshake::server::registration_start`]
    /// for the underlying contract.
    pub fn resolve_credential_identifier(&self, tenant: &str, cli_value: Option<&str>) -> String {
        if let Some(value) = cli_value {
            return value.to_string();
        }
        if let Some(per_tenant) = self.tenants.get(tenant) {
            if let Some(value) = per_tenant.credential_identifier.as_deref() {
                return value.to_string();
            }
        }
        tenant.to_string()
    }

    /// Build the full set of derived server-side settings the
    /// subcommands consume. Pulled into a helper struct rather than
    /// inlined so a future caller (web UI, admin UI) has a single
    /// type to feed into the [`crate::client`] entry points.
    pub fn resolve(
        &self,
        tenant: &str,
        cli_server: Option<&str>,
        cli_credential: Option<&str>,
    ) -> Result<ResolvedServerSettings, LoginError> {
        Ok(ResolvedServerSettings {
            server: self.resolve_server(cli_server)?,
            credential_identifier: self.resolve_credential_identifier(tenant, cli_credential),
            token_env: self.resolve_token_env(),
        })
    }
}

/// Result of [`Config::resolve`]. Snapshot of every server-related
/// value a subcommand needs.
#[derive(Debug, Clone)]
pub struct ResolvedServerSettings {
    /// Broker base URL (parsed and validated — scheme is `http` or
    /// `https`).
    pub server: Url,
    /// OPAQUE credential identifier for this tenant.
    pub credential_identifier: String,
    /// Env var name `env` emits.
    pub token_env: String,
}

/// Probe the standard config locations and return the first one
/// that resolves. Does NOT check whether the file exists — callers
/// must handle the missing-file case (we treat it as
/// [`Config::default()`]).
pub fn resolve_config_path() -> Option<PathBuf> {
    if let Ok(value) = std::env::var(ENV_CONFIG_PATH) {
        if !value.is_empty() {
            return Some(PathBuf::from(value));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("botspace").join("config.toml"));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return Some(
                PathBuf::from(home)
                    .join(".config")
                    .join("botspace")
                    .join("config.toml"),
            );
        }
    }
    None
}

/// Parse and validate a server URL string.
///
/// The value must:
/// - be parseable as an absolute URL, and
/// - carry an explicit `http` or `https` scheme.
///
/// Scheme-less values (e.g. `127.0.0.1:9100`, `example.com`) are
/// rejected with a message that suggests the fix. Non-`http(s)`
/// schemes (e.g. `ftp://`) are rejected with a message naming the
/// unsupported scheme. No scheme is ever silently prepended.
fn validate_server_url(raw: &str) -> Result<Url, LoginError> {
    let trimmed = raw.trim();

    // Fast-path: detect a missing scheme before calling the URL
    // parser. The heuristic checks for `://` anywhere in the string,
    // which reliably covers all scheme-less inputs the user is likely
    // to type (bare host, host:port, domain name). Genuinely
    // malformed strings that happen to contain `://` (e.g. a typo
    // like `http://host://path`) fall through to the URL parser and
    // surface as a "could not be parsed" error — still actionable.
    if !trimmed.contains("://") {
        return Err(LoginError::InvalidServer {
            value: trimmed.to_string(),
            reason: format!("a scheme is required — use 'http://{trimmed}' or 'https://{trimmed}'"),
        });
    }

    let url = Url::parse(trimmed).map_err(|err| LoginError::InvalidServer {
        value: trimmed.to_string(),
        reason: format!("could not be parsed as a valid URL: {err}"),
    })?;

    match url.scheme() {
        "http" | "https" => Ok(url),
        scheme => Err(LoginError::InvalidServer {
            value: trimmed.to_string(),
            reason: format!("only http and https URLs are supported (got '{scheme}')"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Returns the `Mutex` that gates BOTWORK_LOGIN_SERVER mutation
    /// inside this test module. Spelt as a function so a future
    /// addition can move it without churning every call site.
    fn config_env_lock() -> &'static Mutex<()> {
        static LOCK: Mutex<()> = Mutex::new(());
        &LOCK
    }

    /// Acquire the env-mutation lock, recovering from a poisoned mutex
    /// so that a panicking test doesn't cascade failures into every
    /// subsequent test that touches BOTWORK_LOGIN_SERVER.
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        config_env_lock().lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn empty_config_resolves_to_built_in_defaults() {
        let _lock = lock_env();
        std::env::remove_var(ENV_SERVER);
        let cfg = Config::default();
        let url = cfg.resolve_server(None).unwrap();
        // Check components rather than the full string to be
        // independent of url-crate trailing-slash normalisation.
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.port(), Some(9100));
        assert_eq!(cfg.resolve_token_env(), DEFAULT_TOKEN_ENV);
        assert_eq!(
            cfg.resolve_credential_identifier("phlax", None),
            "phlax",
            "credential id defaults to tenant name"
        );
    }

    #[test]
    fn cli_flag_overrides_env_and_file() {
        let _lock = lock_env();
        std::env::remove_var(ENV_SERVER);
        let cfg = Config {
            server: Some("http://from-file:9100".into()),
            ..Config::default()
        };
        // CLI flag wins over file even with env unset.
        let url = cfg.resolve_server(Some("http://from-cli:9100")).unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("from-cli"));
        assert_eq!(url.port(), Some(9100));
    }

    #[test]
    fn env_var_overrides_file() {
        // SAFETY: we set + clear within the test, gated on a
        // per-process mutex shared with every other test in this
        // file that mutates BOTWORK_LOGIN_SERVER. tests in the
        // keyring_store module use a separate mutex because the
        // env vars don't overlap.
        let _lock = lock_env();
        std::env::set_var(ENV_SERVER, "http://from-env:9100");
        let cfg = Config {
            server: Some("http://from-file:9100".into()),
            ..Config::default()
        };
        let url = cfg.resolve_server(None).unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("from-env"));
        assert_eq!(url.port(), Some(9100));
        std::env::remove_var(ENV_SERVER);
    }

    #[test]
    fn per_tenant_credential_identifier_used_when_present() {
        let mut tenants = BTreeMap::new();
        tenants.insert(
            "phlax".to_string(),
            TenantConfig {
                credential_identifier: Some("phlax@example.com".into()),
            },
        );
        let cfg = Config {
            tenants,
            ..Config::default()
        };
        assert_eq!(
            cfg.resolve_credential_identifier("phlax", None),
            "phlax@example.com"
        );
        // CLI flag still wins.
        assert_eq!(
            cfg.resolve_credential_identifier("phlax", Some("override")),
            "override"
        );
        // Unknown tenant falls back to tenant-name default.
        assert_eq!(
            cfg.resolve_credential_identifier("someone-else", None),
            "someone-else"
        );
    }

    #[test]
    fn malformed_toml_surfaces_as_config_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "server = =\"oops\"").unwrap();
        let err = Config::load_from(&path).unwrap_err();
        assert!(matches!(err, LoginError::Config(_)), "got {err:?}");
    }

    #[test]
    fn missing_file_returns_defaults() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.toml");
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.server.is_none());
        assert!(cfg.tenants.is_empty());
    }

    #[test]
    fn round_trip_toml_serde() {
        let mut tenants = BTreeMap::new();
        tenants.insert(
            "phlax".to_string(),
            TenantConfig {
                credential_identifier: Some("phlax".into()),
            },
        );
        let cfg = Config {
            server: Some("http://192.168.122.50:9100".into()),
            token_env: Some("BOTWORK_BEARER".into()),
            tenants,
        };
        let s = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.server.as_deref(), Some("http://192.168.122.50:9100"));
        assert_eq!(back.token_env.as_deref(), Some("BOTWORK_BEARER"));
        assert_eq!(
            back.tenants
                .get("phlax")
                .and_then(|t| t.credential_identifier.as_deref()),
            Some("phlax")
        );
    }

    // ── server URL validation regression tests ──────────────────────

    /// Scheme-less host:port via CLI flag is rejected, not silently
    /// rewritten.
    #[test]
    fn schemeless_host_port_80_is_rejected() {
        let _lock = lock_env();
        std::env::remove_var(ENV_SERVER);
        let cfg = Config::default();
        let err = cfg.resolve_server(Some("127.0.0.1:80")).unwrap_err();
        assert!(
            matches!(err, LoginError::InvalidServer { ref value, .. } if value == "127.0.0.1:80"),
            "expected InvalidServer with value '127.0.0.1:80', got {err:?}"
        );
        assert!(
            err.to_string().contains("scheme is required"),
            "error should mention 'scheme is required': {err}"
        );
    }

    /// Scheme-less host:port 9100 via CLI flag is rejected.
    #[test]
    fn schemeless_host_port_9100_is_rejected() {
        let _lock = lock_env();
        std::env::remove_var(ENV_SERVER);
        let cfg = Config::default();
        let err = cfg.resolve_server(Some("127.0.0.1:9100")).unwrap_err();
        assert!(
            matches!(err, LoginError::InvalidServer { ref value, .. } if value == "127.0.0.1:9100"),
            "expected InvalidServer, got {err:?}"
        );
    }

    /// Bare host with no port and no scheme is rejected.
    #[test]
    fn schemeless_bare_host_is_rejected() {
        let _lock = lock_env();
        std::env::remove_var(ENV_SERVER);
        let cfg = Config::default();
        let err = cfg.resolve_server(Some("example.com")).unwrap_err();
        assert!(
            matches!(err, LoginError::InvalidServer { ref value, .. } if value == "example.com"),
            "expected InvalidServer, got {err:?}"
        );
        assert!(
            err.to_string().contains("scheme is required"),
            "error should mention 'scheme is required': {err}"
        );
    }

    /// Non-http(s) scheme is rejected with the "only http and https" message.
    #[test]
    fn ftp_scheme_is_rejected() {
        let _lock = lock_env();
        std::env::remove_var(ENV_SERVER);
        let cfg = Config::default();
        let err = cfg.resolve_server(Some("ftp://x")).unwrap_err();
        assert!(
            matches!(err, LoginError::InvalidServer { ref value, .. } if value == "ftp://x"),
            "expected InvalidServer, got {err:?}"
        );
        assert!(
            err.to_string().contains("only http and https"),
            "error should mention 'only http and https': {err}"
        );
    }

    /// Valid http URL is accepted and the parsed URL preserves scheme/host/port.
    #[test]
    fn http_url_is_accepted() {
        let _lock = lock_env();
        std::env::remove_var(ENV_SERVER);
        let cfg = Config::default();
        let url = cfg.resolve_server(Some("http://127.0.0.1:9100")).unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.port(), Some(9100));
    }

    /// Valid https URL is accepted.
    #[test]
    fn https_url_is_accepted() {
        let _lock = lock_env();
        std::env::remove_var(ENV_SERVER);
        let cfg = Config::default();
        let url = cfg
            .resolve_server(Some("https://broker.example:9100"))
            .unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("broker.example"));
        assert_eq!(url.port(), Some(9100));
    }

    /// The built-in DEFAULT_SERVER is accepted without modification.
    #[test]
    fn default_server_is_accepted() {
        let _lock = lock_env();
        std::env::remove_var(ENV_SERVER);
        let cfg = Config::default();
        // DEFAULT_SERVER = "http://127.0.0.1:9100"
        let url = cfg.resolve_server(None).unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.port(), Some(9100));
    }

    /// Scheme-less value in BOTWORK_LOGIN_SERVER env var is rejected.
    #[test]
    fn schemeless_env_var_is_rejected() {
        let _lock = lock_env();
        std::env::set_var(ENV_SERVER, "127.0.0.1:9100");
        let cfg = Config::default();
        let err = cfg.resolve_server(None).unwrap_err();
        assert!(
            matches!(err, LoginError::InvalidServer { ref value, .. } if value == "127.0.0.1:9100"),
            "expected InvalidServer from env var, got {err:?}"
        );
        std::env::remove_var(ENV_SERVER);
    }

    /// Scheme-less value in the config file's `server` field is rejected.
    #[test]
    fn schemeless_config_file_server_is_rejected() {
        let _lock = lock_env();
        std::env::remove_var(ENV_SERVER);
        let cfg = Config {
            server: Some("192.168.1.50:9100".into()),
            ..Config::default()
        };
        let err = cfg.resolve_server(None).unwrap_err();
        assert!(
            matches!(err, LoginError::InvalidServer { .. }),
            "expected InvalidServer from config file value, got {err:?}"
        );
    }
}
