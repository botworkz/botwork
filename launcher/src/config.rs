use std::env;

use nix::unistd::{getegid, geteuid, Group};

pub const PREFIX: &str = "[botwork-launcher]";
pub const DEFAULT_SOCKET_PATH: &str = "/run/botwork/launcher.sock";
pub const DEFAULT_BROKER_SOCKET_PATH: &str = "/run/botwork/broker.sock";
// Override with BOTWORK_LAUNCHER_IMAGE_ALLOWLIST_REGEX when needed.
pub const DEFAULT_IMAGE_ALLOWLIST: &str = r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$";
pub const DEFAULT_CONTAINER_PIDS_LIMIT: u32 = 256;
pub const DEFAULT_CONTAINER_CPU_LIMIT: &str = "1.0";
pub const DEFAULT_CONTAINER_MEMORY_LIMIT: &str = "512m";

#[derive(Clone, Debug)]
pub struct Config {
    pub socket_path: String,
    pub socket_group: Option<u32>,
    pub allowed_peer_uid: Option<u32>,
    pub allowed_peer_gid: Option<u32>,
    pub plugin_uid: u32,
    pub plugin_gid: u32,
    pub image_allowlist_regex: String,
    pub container_pids_limit: u32,
    pub container_cpu_limit: String,
    pub container_memory_limit: String,
    pub container_read_only_rootfs: bool,
    pub broker_socket_path: String,
    /// Docker network name plugin containers are spawned into. Required —
    /// `from_env` refuses to construct a `Config` if `BOTWORK_LAUNCHER_DEFAULT_NETWORK`
    /// is unset, because the launcher cannot guess which network is correct
    /// in the deployment and silently picking one would defeat the network
    /// isolation boundary it exists to enforce.
    pub default_network: String,
    /// Optional URL of the egress proxy plugin containers should route
    /// outbound HTTP/HTTPS through. When set, the launcher injects
    /// `HTTPS_PROXY`, `HTTP_PROXY`, and `NO_PROXY` env vars into every
    /// spawned container (see `docker::PROXY_ENV_INJECTIONS`).
    ///
    /// When unset (the default in dev / pre-cycle-2B deployments), no
    /// proxy env vars are injected and plugins reach the network
    /// directly. The variable is intentionally opt-in so a launcher
    /// rolled out before its corresponding egress envoy unit doesn't
    /// silently break every plugin's outbound traffic — `vm 0.3.4+`
    /// sets it on `botwork-launcher.service`'s `Environment=`.
    ///
    /// Validation: must parse as `http://<host>[:port]` or
    /// `https://<host>[:port]`. Rejected at construction time so an
    /// operator typo (e.g. forgetting the `http://`) fails launcher
    /// startup loudly rather than producing confusing
    /// curl-can't-find-proxy errors hours later inside a plugin.
    pub egress_proxy: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let socket_path = env::var("BOTWORK_LAUNCHER_SOCKET_PATH")
            .unwrap_or_else(|_| DEFAULT_SOCKET_PATH.to_string());
        let socket_group = env::var("BOTWORK_LAUNCHER_SOCKET_GROUP")
            .ok()
            .map(|value| resolve_group_spec(&value))
            .transpose()?;
        let allowed_peer_uid = env::var("BOTWORK_LAUNCHER_ALLOWED_UID")
            .ok()
            .map(|value| parse_u32_env("BOTWORK_LAUNCHER_ALLOWED_UID", &value))
            .transpose()?;
        let allowed_peer_gid = env::var("BOTWORK_LAUNCHER_ALLOWED_GID")
            .ok()
            .map(|value| parse_u32_env("BOTWORK_LAUNCHER_ALLOWED_GID", &value))
            .transpose()?;
        let (allowed_peer_uid, allowed_peer_gid) =
            if allowed_peer_uid.is_none() && allowed_peer_gid.is_none() {
                // Default to our own identity for local dev, but production should still set the
                // broker uid/gid explicitly so the launcher is not trusting ambient host state.
                (Some(geteuid().as_raw()), Some(getegid().as_raw()))
            } else {
                (allowed_peer_uid, allowed_peer_gid)
            };
        let plugin_uid = env::var("BOTWORK_PLUGIN_UID")
            .unwrap_or_else(|_| "1000".to_string())
            .parse::<u32>()
            .map_err(|err| format!("invalid BOTWORK_PLUGIN_UID: {err}"))?;
        let plugin_gid = env::var("BOTWORK_PLUGIN_GID")
            .unwrap_or_else(|_| "1000".to_string())
            .parse::<u32>()
            .map_err(|err| format!("invalid BOTWORK_PLUGIN_GID: {err}"))?;
        let image_allowlist_regex = env::var("BOTWORK_LAUNCHER_IMAGE_ALLOWLIST_REGEX")
            .unwrap_or_else(|_| DEFAULT_IMAGE_ALLOWLIST.to_string());
        let container_pids_limit = env::var("BOTWORK_LAUNCHER_PIDS_LIMIT")
            .unwrap_or_else(|_| DEFAULT_CONTAINER_PIDS_LIMIT.to_string())
            .parse::<u32>()
            .map_err(|err| format!("invalid BOTWORK_LAUNCHER_PIDS_LIMIT: {err}"))?;
        let container_cpu_limit = env::var("BOTWORK_LAUNCHER_CPU_LIMIT")
            .unwrap_or_else(|_| DEFAULT_CONTAINER_CPU_LIMIT.to_string());
        if container_cpu_limit.trim().is_empty() {
            return Err("invalid BOTWORK_LAUNCHER_CPU_LIMIT: must not be empty".to_string());
        }
        let container_memory_limit = env::var("BOTWORK_LAUNCHER_MEMORY_LIMIT")
            .unwrap_or_else(|_| DEFAULT_CONTAINER_MEMORY_LIMIT.to_string());
        if container_memory_limit.trim().is_empty() {
            return Err("invalid BOTWORK_LAUNCHER_MEMORY_LIMIT: must not be empty".to_string());
        }
        let container_read_only_rootfs =
            parse_bool_env("BOTWORK_LAUNCHER_READ_ONLY_ROOTFS")?.unwrap_or(false);
        let broker_socket_path = env::var("BOTWORK_BROKER_SOCKET_PATH")
            .unwrap_or_else(|_| DEFAULT_BROKER_SOCKET_PATH.to_string());
        let default_network = env::var("BOTWORK_LAUNCHER_DEFAULT_NETWORK").map_err(|_| {
            "BOTWORK_LAUNCHER_DEFAULT_NETWORK must be set: the launcher refuses to \
             guess which docker network plugin containers belong to. Set it to the \
             network alias plugins should join (e.g. `botwork-plugin`)."
                .to_string()
        })?;
        let default_network = default_network.trim().to_string();
        if default_network.is_empty() {
            return Err("BOTWORK_LAUNCHER_DEFAULT_NETWORK must not be empty".to_string());
        }
        if !default_network
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            return Err(format!(
                "BOTWORK_LAUNCHER_DEFAULT_NETWORK has invalid characters: {default_network}; \
                 expected ^[a-z0-9_-]+$ (must match docker network naming)"
            ));
        }
        let egress_proxy = parse_egress_proxy_env()?;

        Ok(Self {
            socket_path,
            socket_group,
            allowed_peer_uid,
            allowed_peer_gid,
            plugin_uid,
            plugin_gid,
            image_allowlist_regex,
            container_pids_limit,
            container_cpu_limit,
            container_memory_limit,
            container_read_only_rootfs,
            broker_socket_path,
            default_network,
            egress_proxy,
        })
    }
}

/// Parse and validate the optional `BOTWORK_LAUNCHER_EGRESS_PROXY` env
/// var. Returns `Ok(None)` when unset (the default), `Ok(Some(url))`
/// when set and valid, `Err(_)` on a malformed value.
///
/// Validation is intentionally conservative — we only need to confirm
/// the value looks like an absolute `http://` / `https://` URL with a
/// reasonable host part. We are not running a full URL parser here
/// because:
///
/// * The launcher hands the value straight into the spawned
///   container's env, never to a docker arg or a network call of its
///   own. Anything that's plausibly an HTTP_PROXY-compatible URL is
///   passed verbatim. The plugin's HTTP client validates it for real.
/// * The deployment shape is fixed — `http://egress_envoy:3128` — so
///   the wire shape we accept doesn't need to cover oddities like
///   userinfo or paths.
/// * Strict early rejection of the typo cases (missing scheme, bare
///   hostname, embedded whitespace) is what catches the realistic
///   operator mistake; deeper RFC 3986 conformance gains nothing.
fn parse_egress_proxy_env() -> Result<Option<String>, String> {
    let raw = match env::var("BOTWORK_LAUNCHER_EGRESS_PROXY") {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            return Err("invalid BOTWORK_LAUNCHER_EGRESS_PROXY: not valid unicode".to_string());
        }
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        // Unset and set-to-whitespace are the same intent. Treat as
        // unset rather than fail-start; this matches operator
        // expectations from systemd unit files where an
        // `Environment=BOTWORK_LAUNCHER_EGRESS_PROXY=` line is the
        // most natural way to "switch off" the injection.
        return Ok(None);
    }
    validate_egress_proxy(trimmed)?;
    Ok(Some(trimmed.to_string()))
}

fn validate_egress_proxy(value: &str) -> Result<(), String> {
    let rest = if let Some(rest) = value.strip_prefix("http://") {
        rest
    } else if let Some(rest) = value.strip_prefix("https://") {
        rest
    } else {
        return Err(format!(
            "invalid BOTWORK_LAUNCHER_EGRESS_PROXY: must start with http:// or https:// (got {value:?})"
        ));
    };
    if rest.is_empty() {
        return Err("invalid BOTWORK_LAUNCHER_EGRESS_PROXY: empty host".to_string());
    }
    // No whitespace, no control chars; host[:port] must be a single
    // token (the env var is forwarded verbatim to the container, so
    // anything that breaks here would break inside the plugin too).
    if rest.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "invalid BOTWORK_LAUNCHER_EGRESS_PROXY: contains whitespace or control characters (got {value:?})"
        ));
    }
    // A path / query is allowed by the HTTP_PROXY convention but our
    // egress envoy doesn't honour one; reject so a misconfig is loud.
    if let Some(after_host) = rest.find('/') {
        if after_host < rest.len() - 1 || &rest[after_host..] != "/" {
            return Err(format!(
                "invalid BOTWORK_LAUNCHER_EGRESS_PROXY: must not include a path (got {value:?})"
            ));
        }
    }
    Ok(())
}

fn parse_u32_env(name: &str, value: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .map_err(|err| format!("invalid {name}: {err}"))
}

fn parse_bool_env(name: &str) -> Result<Option<bool>, String> {
    match env::var(name) {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(Some(true)),
            "0" | "false" | "no" | "off" => Ok(Some(false)),
            _ => Err(format!(
                "invalid {name}: expected one of 1,true,yes,on,0,false,no,off"
            )),
        },
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(format!("invalid {name}: not valid unicode")),
    }
}

fn resolve_group_spec(spec: &str) -> Result<u32, String> {
    if let Ok(gid) = spec.parse::<u32>() {
        return Ok(gid);
    }

    // `nix::unistd::Group::from_name` wraps `getgrnam_r(3)` and
    // handles the ERANGE buffer-doubling retry loop internally, so we
    // get the same semantics as the hand-rolled libc dance without any
    // unsafe code or sysconf bookkeeping.
    match Group::from_name(spec) {
        Ok(Some(group)) => Ok(group.gid.as_raw()),
        Ok(None) => Err(format!(
            "failed to resolve BOTWORK_LAUNCHER_SOCKET_GROUP={spec}: no such group"
        )),
        Err(err) => Err(format!(
            "failed to resolve BOTWORK_LAUNCHER_SOCKET_GROUP={spec}: {err}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use nix::unistd::{getegid, Group};

    use super::{
        parse_bool_env, resolve_group_spec, validate_egress_proxy, Config,
        DEFAULT_CONTAINER_CPU_LIMIT,
    };

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn resolve_group_spec_accepts_numeric_gid() {
        assert_eq!(resolve_group_spec("1234").expect("numeric gid"), 1234);
    }

    #[test]
    fn resolve_group_spec_accepts_existing_group_name() {
        // Look up the current gid via nix and round-trip its name back
        // through `resolve_group_spec`. Skip silently if the current
        // gid has no `/etc/group` entry — that happens on minimal
        // container/CI images where the workspace uid was set up
        // without a matching group line, and the test would be moot
        // there anyway because no name to round-trip exists.
        let current_gid = getegid();
        let Some(group) = Group::from_gid(current_gid).expect("getgrgid_r should succeed") else {
            eprintln!(
                "skipping resolve_group_spec_accepts_existing_group_name: \
                 no group entry for current gid {}",
                current_gid.as_raw()
            );
            return;
        };
        assert_eq!(
            resolve_group_spec(&group.name).expect("group name"),
            current_gid.as_raw()
        );
    }

    #[test]
    fn parse_bool_env_accepts_expected_values() {
        std::env::set_var("BOTWORK_TEST_BOOL_ENV", "yes");
        assert_eq!(
            parse_bool_env("BOTWORK_TEST_BOOL_ENV").expect("parse bool"),
            Some(true)
        );
        std::env::set_var("BOTWORK_TEST_BOOL_ENV", "off");
        assert_eq!(
            parse_bool_env("BOTWORK_TEST_BOOL_ENV").expect("parse bool"),
            Some(false)
        );
        std::env::remove_var("BOTWORK_TEST_BOOL_ENV");
        assert_eq!(
            parse_bool_env("BOTWORK_TEST_BOOL_ENV").expect("missing bool"),
            None
        );
    }

    #[test]
    fn from_env_uses_default_cpu_limit_when_unset() {
        let _guard = env_lock().lock().expect("env lock");
        std::env::remove_var("BOTWORK_LAUNCHER_CPU_LIMIT");
        std::env::set_var("BOTWORK_LAUNCHER_DEFAULT_NETWORK", "botwork-plugin");
        let config = Config::from_env().expect("config");
        assert_eq!(config.container_cpu_limit, DEFAULT_CONTAINER_CPU_LIMIT);
        assert_eq!(config.default_network, "botwork-plugin");
        std::env::remove_var("BOTWORK_LAUNCHER_DEFAULT_NETWORK");
    }

    #[test]
    fn from_env_rejects_empty_cpu_limit() {
        let _guard = env_lock().lock().expect("env lock");
        std::env::set_var("BOTWORK_LAUNCHER_CPU_LIMIT", "   ");
        std::env::set_var("BOTWORK_LAUNCHER_DEFAULT_NETWORK", "botwork-plugin");
        assert_eq!(
            Config::from_env().expect_err("empty cpu limit should fail"),
            "invalid BOTWORK_LAUNCHER_CPU_LIMIT: must not be empty"
        );
        std::env::remove_var("BOTWORK_LAUNCHER_CPU_LIMIT");
        std::env::remove_var("BOTWORK_LAUNCHER_DEFAULT_NETWORK");
    }

    #[test]
    fn from_env_rejects_missing_default_network() {
        let _guard = env_lock().lock().expect("env lock");
        std::env::remove_var("BOTWORK_LAUNCHER_DEFAULT_NETWORK");
        let err = Config::from_env().expect_err("missing default network should fail");
        assert!(
            err.contains("BOTWORK_LAUNCHER_DEFAULT_NETWORK must be set"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn from_env_rejects_empty_default_network() {
        let _guard = env_lock().lock().expect("env lock");
        std::env::set_var("BOTWORK_LAUNCHER_DEFAULT_NETWORK", "   ");
        let err = Config::from_env().expect_err("empty default network should fail");
        assert_eq!(err, "BOTWORK_LAUNCHER_DEFAULT_NETWORK must not be empty");
        std::env::remove_var("BOTWORK_LAUNCHER_DEFAULT_NETWORK");
    }

    #[test]
    fn from_env_rejects_default_network_with_invalid_characters() {
        let _guard = env_lock().lock().expect("env lock");
        // Whitespace, slashes, uppercase, dots — anything outside [a-z0-9_-] —
        // must be rejected at construction time so an operator typo cannot
        // produce a runtime docker error after the launcher has accepted the
        // config.
        for bad in [
            "botwork plugin",
            "botwork/plugin",
            "Botwork",
            "botwork.plugin",
        ] {
            std::env::set_var("BOTWORK_LAUNCHER_DEFAULT_NETWORK", bad);
            let err = Config::from_env().expect_err("invalid network should fail");
            assert!(
                err.contains("has invalid characters"),
                "unexpected error for {bad:?}: {err}"
            );
        }
        std::env::remove_var("BOTWORK_LAUNCHER_DEFAULT_NETWORK");
    }

    #[test]
    fn from_env_egress_proxy_unset_is_none() {
        let _guard = env_lock().lock().expect("env lock");
        std::env::remove_var("BOTWORK_LAUNCHER_EGRESS_PROXY");
        std::env::set_var("BOTWORK_LAUNCHER_DEFAULT_NETWORK", "botwork-plugin");
        let config = Config::from_env().expect("config");
        assert_eq!(config.egress_proxy, None);
        std::env::remove_var("BOTWORK_LAUNCHER_DEFAULT_NETWORK");
    }

    #[test]
    fn from_env_egress_proxy_whitespace_treated_as_unset() {
        // An empty / whitespace-only Environment= line in a systemd unit
        // is the obvious "switch off the proxy without rewriting the unit
        // file" gesture, so accept it as unset rather than fail-start.
        let _guard = env_lock().lock().expect("env lock");
        std::env::set_var("BOTWORK_LAUNCHER_EGRESS_PROXY", "   ");
        std::env::set_var("BOTWORK_LAUNCHER_DEFAULT_NETWORK", "botwork-plugin");
        let config = Config::from_env().expect("config");
        assert_eq!(config.egress_proxy, None);
        std::env::remove_var("BOTWORK_LAUNCHER_EGRESS_PROXY");
        std::env::remove_var("BOTWORK_LAUNCHER_DEFAULT_NETWORK");
    }

    #[test]
    fn from_env_egress_proxy_valid_http_url() {
        let _guard = env_lock().lock().expect("env lock");
        std::env::set_var("BOTWORK_LAUNCHER_EGRESS_PROXY", "http://egress_envoy:3128");
        std::env::set_var("BOTWORK_LAUNCHER_DEFAULT_NETWORK", "botwork-plugin");
        let config = Config::from_env().expect("config");
        assert_eq!(
            config.egress_proxy.as_deref(),
            Some("http://egress_envoy:3128")
        );
        std::env::remove_var("BOTWORK_LAUNCHER_EGRESS_PROXY");
        std::env::remove_var("BOTWORK_LAUNCHER_DEFAULT_NETWORK");
    }

    #[test]
    fn validate_egress_proxy_accepts_http_and_https() {
        validate_egress_proxy("http://egress_envoy:3128").expect("http url");
        validate_egress_proxy("https://proxy.example:8443").expect("https url");
        // Trailing root path is permitted (some HTTP_PROXY-honouring
        // libs add it themselves and we don't want to false-alarm on
        // an operator copying from one of those examples).
        validate_egress_proxy("http://egress_envoy:3128/").expect("trailing root");
    }

    #[test]
    fn validate_egress_proxy_rejects_missing_scheme() {
        let err = validate_egress_proxy("egress_envoy:3128").expect_err("must reject");
        assert!(err.contains("http://"), "{err}");
    }

    #[test]
    fn validate_egress_proxy_rejects_wrong_scheme() {
        for bad in ["ftp://egress_envoy:3128", "socks5://e:1080", "egress_envoy"] {
            let err = validate_egress_proxy(bad).expect_err("must reject");
            assert!(err.contains("http://"), "{err}");
        }
    }

    #[test]
    fn validate_egress_proxy_rejects_empty_host() {
        let err = validate_egress_proxy("http://").expect_err("empty host");
        assert!(err.contains("empty host"), "{err}");
    }

    #[test]
    fn validate_egress_proxy_rejects_whitespace_in_value() {
        // Verbatim forwarding into the container makes any whitespace a
        // confusing failure surface, so reject early.
        let err = validate_egress_proxy("http://egress envoy:3128").expect_err("ws");
        assert!(err.contains("whitespace"), "{err}");
    }

    #[test]
    fn validate_egress_proxy_rejects_path_component() {
        // Egress envoy doesn't honour a base path — reject to surface
        // misconfig instead of letting it through.
        let err = validate_egress_proxy("http://egress_envoy:3128/proxy").expect_err("path");
        assert!(err.contains("path"), "{err}");
    }
}
