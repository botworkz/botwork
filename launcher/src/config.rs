use std::env;
use std::ffi::CString;

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
                (
                    Some(unsafe { libc::geteuid() }),
                    Some(unsafe { libc::getegid() }),
                )
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
        })
    }
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

    let c_spec = CString::new(spec.as_bytes())
        .map_err(|err| format!("invalid BOTWORK_LAUNCHER_SOCKET_GROUP: {err}"))?;
    let mut buffer_len = group_lookup_buffer_len();
    for _ in 0..4 {
        let mut group = std::mem::MaybeUninit::<libc::group>::zeroed();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; buffer_len];
        let rc = unsafe {
            libc::getgrnam_r(
                c_spec.as_ptr(),
                group.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE {
            buffer_len *= 2;
            continue;
        }
        if rc != 0 {
            return Err(format!(
                "failed to resolve BOTWORK_LAUNCHER_SOCKET_GROUP={spec}: {}",
                std::io::Error::from_raw_os_error(rc)
            ));
        }
        if result.is_null() {
            return Err(format!(
                "failed to resolve BOTWORK_LAUNCHER_SOCKET_GROUP={spec}: no such group"
            ));
        }

        let group = unsafe { group.assume_init() };
        return Ok(group.gr_gid);
    }

    Err(format!(
        "failed to resolve BOTWORK_LAUNCHER_SOCKET_GROUP={spec}: group entry too large"
    ))
}

fn group_lookup_buffer_len() -> usize {
    let suggested = unsafe { libc::sysconf(libc::_SC_GETGR_R_SIZE_MAX) };
    if suggested > 0 {
        suggested as usize
    } else {
        16 * 1024
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;
    use std::sync::{Mutex, OnceLock};

    use super::{parse_bool_env, resolve_group_spec, Config, DEFAULT_CONTAINER_CPU_LIMIT};

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
        let current_gid = unsafe { libc::getegid() };
        let name = current_group_name(current_gid);
        assert_eq!(resolve_group_spec(&name).expect("group name"), current_gid,);
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

    fn current_group_name(gid: u32) -> String {
        let mut group = std::mem::MaybeUninit::<libc::group>::zeroed();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; 16 * 1024];
        let rc = unsafe {
            libc::getgrgid_r(
                gid,
                group.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        assert_eq!(rc, 0, "getgrgid_r should succeed");
        assert!(!result.is_null(), "current gid should resolve");
        let group = unsafe { group.assume_init() };
        unsafe { CStr::from_ptr(group.gr_name) }
            .to_str()
            .expect("group name utf8")
            .to_string()
    }
}
