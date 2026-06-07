use std::path::{Component, Path, PathBuf};

use regex::Regex;

use crate::error::LauncherError;

const STAGING_BASE: &str = "/var/lib/botwork/tenants";
const AGENTS_BASE: &str = "/var/lib/botwork/tenants";
const TENANT_RE: &str = r"[a-z][a-z0-9-]{0,30}";
pub const RESERVED_ENV_NAMES: &[&str] = &["PATH", "HOME", "USER", "LD_PRELOAD", "LD_LIBRARY_PATH"];

#[derive(Clone, Debug)]
pub struct Validators {
    name_re: Regex,
    image_re: Regex,
    network_re: Regex,
    staging_path_re: Regex,
    agent_dir_re: Regex,
}

impl Validators {
    pub fn new(image_allowlist_regex: &str) -> Result<Self, String> {
        let name_re = Regex::new(r"^mcp_session_[a-f0-9]{12}$").map_err(|err| err.to_string())?;
        let image_re = Regex::new(image_allowlist_regex).map_err(|err| err.to_string())?;
        let network_re = Regex::new(r"^[a-z0-9_-]+$").map_err(|err| err.to_string())?;
        let staging_path_re = Regex::new(&format!(
            r"^/var/lib/botwork/tenants/{TENANT_RE}/staging/[a-f0-9]{{12}}$"
        ))
        .map_err(|err| err.to_string())?;
        let agent_dir_re = Regex::new(&format!(
            r"^/var/lib/botwork/tenants/{TENANT_RE}/agents/[A-Za-z0-9_-]{{1,64}}$"
        ))
        .map_err(|err| err.to_string())?;

        Ok(Self {
            name_re,
            image_re,
            network_re,
            staging_path_re,
            agent_dir_re,
        })
    }

    pub fn valid_name(&self, value: &str) -> bool {
        self.name_re.is_match(value)
    }

    pub fn valid_image(&self, value: &str) -> bool {
        self.image_re.is_match(value)
    }

    pub fn valid_network(&self, value: &str) -> bool {
        self.network_re.is_match(value)
    }

    pub fn valid_staging_path(&self, value: &str) -> bool {
        self.staging_path_re.is_match(value)
    }

    pub fn valid_agent_dir(&self, value: &str) -> bool {
        self.agent_dir_re.is_match(value)
    }

    pub fn valid_env_name(&self, name: &str) -> bool {
        valid_env_name(name)
    }

    pub fn safe_staging_path(&self, value: &str) -> Result<String, LauncherError> {
        if !self.valid_staging_path(value) {
            return Err(LauncherError::BadRequest(
                "invalid staging_path".to_string(),
            ));
        }
        let safe = normalize_path(value);
        if !safe.starts_with(&format!("{STAGING_BASE}/")) {
            return Err(LauncherError::BadRequest(
                "invalid staging_path".to_string(),
            ));
        }
        Ok(safe)
    }

    pub fn safe_agent_dir(&self, value: &str) -> Result<String, LauncherError> {
        if !self.valid_agent_dir(value) {
            return Err(LauncherError::BadRequest("invalid agent_dir".to_string()));
        }
        let safe = normalize_path(value);
        if !safe.starts_with(&format!("{AGENTS_BASE}/")) {
            return Err(LauncherError::BadRequest("invalid agent_dir".to_string()));
        }
        Ok(safe)
    }
}

pub fn valid_env_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return false;
    }

    let first = bytes[0];
    if !(first.is_ascii_uppercase() || first == b'_') {
        return false;
    }

    if bytes
        .iter()
        .skip(1)
        .any(|byte| !(byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_'))
    {
        return false;
    }

    if RESERVED_ENV_NAMES.contains(&name) {
        return false;
    }

    // Block all Docker-related env overrides in addition to specific reserved names.
    !name.starts_with("DOCKER_")
}

fn normalize_path(path: &str) -> String {
    let mut normalized = PathBuf::new();

    for component in Path::new(path).components() {
        match component {
            Component::RootDir => normalized.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::Prefix(_) => {}
        }
    }

    normalized.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::{valid_env_name, Validators, RESERVED_ENV_NAMES};

    fn validators() -> Validators {
        Validators::new(r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$").expect("validators")
    }

    #[test]
    fn validators_accept_expected_values() {
        let validators = validators();

        assert!(validators.valid_name("mcp_session_aabbccddeeff"));
        assert!(validators.valid_image("botwork/mcp-echo:local"));
        assert!(validators.valid_image("botwork/mcp-echo:local"));
        assert!(validators.valid_network("botwork_network-1"));
        assert!(validators.valid_staging_path("/var/lib/botwork/tenants/acme/staging/aabbccddeeff"));
        assert!(validators.valid_agent_dir("/var/lib/botwork/tenants/acme/agents/my_agent-1"));
    }

    #[test]
    fn validators_reject_obvious_bad_values() {
        let validators = validators();

        assert!(!validators.valid_name("mcp_session_abcdef"));
        assert!(!validators.valid_image("botspace/mcp-echo:local"));
        assert!(!validators.valid_image("ghcr.io/phlax/botwork:latest"));
        assert!(!validators.valid_image("foo/mcp-echo:local"));
        assert!(!validators.valid_network("bad network"));
        assert!(
            !validators.valid_staging_path("/var/lib/botwork/tenants/Acme/staging/aabbccddeeff")
        );
        assert!(!validators.valid_staging_path("/var/lib/botwork/tenants/acme/staging/aabbccddee"));
        assert!(
            !validators.valid_staging_path("/var/lib/botwork/tenants/acme/staging/../aabbccddeeff")
        );
        assert!(!validators.valid_agent_dir("/var/lib/botwork/tenants/acme/agents/invalid.agent"));
        assert!(!validators.valid_agent_dir("/tmp/agents/agentA"));
    }

    #[test]
    fn safe_paths_normalize_without_symlink_resolution() {
        let validators = validators();

        let staging = validators
            .safe_staging_path("/var/lib/botwork/tenants/acme/staging/aabbccddeeff")
            .expect("staging path should validate");
        assert_eq!(
            staging,
            "/var/lib/botwork/tenants/acme/staging/aabbccddeeff"
        );

        let agent = validators
            .safe_agent_dir("/var/lib/botwork/tenants/acme/agents/agent_A")
            .expect("agent dir should validate");
        assert_eq!(agent, "/var/lib/botwork/tenants/acme/agents/agent_A");
    }

    #[test]
    fn valid_env_name_enforces_shape_and_reserved_names() {
        assert!(valid_env_name("BOTWORK_SECRET_GITHUB_COM_PAT"));
        assert!(valid_env_name("_BOTWORK_SECRET_1"));

        assert!(!valid_env_name(""));
        assert!(!valid_env_name("botwork_secret"));
        assert!(!valid_env_name("BOTWORK-SECRET"));
        assert!(!valid_env_name("1BOTWORK_SECRET"));
        assert!(!valid_env_name("BOTWORK=SECRET"));
        assert!(!valid_env_name("BOTWORK_\0_SECRET"));
        assert!(!valid_env_name("DOCKER_SECRET"));

        for name in RESERVED_ENV_NAMES {
            assert!(!valid_env_name(name));
        }
    }

    #[test]
    fn validators_expose_valid_env_name() {
        let validators = validators();
        assert!(validators.valid_env_name("BOTWORK_SECRET"));
        assert!(!validators.valid_env_name("PATH"));
    }
}
