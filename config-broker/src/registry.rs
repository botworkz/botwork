//! Plugin registry: parses `plugins.yaml` into `PluginEntry` values that the
//! handler renders into wire-shape `PluginDescriptor` JSON.
//!
//! Lifted verbatim from `session-broker/src/plugin_registry.rs`. Validation
//! rules (env name regex, size caps, `BOTWORK_MCP_CONFIG` reservation,
//! `upstream_auth` grammar, resources schema) are unchanged.
//!
//! Constants that previously lived in `session-broker/src/secrets.rs` are
//! duplicated here. They are *contract* values shared with the launcher's
//! env validation; both producers (config-broker → session-broker) and
//! consumers (launcher) must keep them in sync.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;
use thiserror::Error;

// Keep in sync with launcher/src/validate.rs RESERVED_ENV_NAMES.
const RESERVED_ENV_NAMES: &[&str] = &["PATH", "LD_PRELOAD", "LD_LIBRARY_PATH"];

/// Maximum number of static env entries per plugin (leaves headroom under
/// launcher's MAX_ENV_ENTRIES = 64 for vault-derived secrets).
const MAX_STATIC_ENV_ENTRIES: usize = 32;

/// Maximum size of any single env value (or serialised config blob).
/// Keep in sync with `session-broker::secrets::MAX_ENV_VALUE_BYTES`.
const MAX_ENV_VALUE_BYTES: usize = 64 * 1024;

/// Reserved prefix for vault-derived secret env entries; operators may not
/// declare env keys starting with this string.
/// Keep in sync with `session-broker::secrets::SECRET_ENV_PREFIX`.
const SECRET_ENV_PREFIX: &str = "BOTWORK_SECRET_";

/// Env var name under which compact-JSON structured config is injected.
///
/// This name is reserved: operators must express structured config through the
/// `config:` field in `plugins.yaml`, not via the flat `env:` mapping.
pub const CONFIG_ENV_NAME: &str = "BOTWORK_MCP_CONFIG";

static PLUGIN_NAME_RE: OnceLock<Regex> = OnceLock::new();

fn plugin_name_re() -> &'static Regex {
    PLUGIN_NAME_RE.get_or_init(|| Regex::new(r"^[a-z][a-z0-9-]{0,30}$").unwrap())
}

/// In-memory representation of a single plugin's `plugins.yaml` entry.
///
/// `PluginDescriptor` (the wire shape) is rendered from this by the handler.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginEntry {
    pub image: String,
    pub port: u16,
    pub network: String,
    pub path: String,
    pub upstream_auth: UpstreamAuth,
    pub env: Vec<(String, String)>,
    pub resources: PluginResources,
    /// Structured config, stored as a JSON value at parse time and serialised
    /// to compact JSON at the wire boundary. `None` means the operator did not
    /// set `config:` and the env var must not be injected.
    pub config: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpstreamAuth {
    #[default]
    None,
    Bearer {
        service: String,
    },
}

impl UpstreamAuth {
    /// Render to the on-the-wire string form: `"none"` or `"bearer/<service>"`.
    pub fn to_wire(&self) -> String {
        match self {
            Self::None => "none".to_string(),
            Self::Bearer { service } => format!("bearer/{service}"),
        }
    }

    fn from_yaml_value(name: &str, value: &serde_yaml::Value) -> Result<Self, RegistryError> {
        if value.is_null() {
            return Ok(Self::None);
        }

        let Some(value) = value.as_str() else {
            return Err(RegistryError::Invalid(format!(
                "plugin '{name}' has invalid 'upstream_auth': expected 'none' or 'bearer/<service>'"
            )));
        };

        match value {
            "none" => Ok(Self::None),
            "bearer" | "bearer/" => Err(RegistryError::Invalid(format!(
                "plugin '{name}' has invalid 'upstream_auth': bearer requires a service: use bearer/<service>"
            ))),
            _ => {
                if let Some(service) = value.strip_prefix("bearer/") {
                    if !service.is_empty()
                        && !service.contains('/')
                        && !service.chars().any(char::is_whitespace)
                    {
                        return Ok(Self::Bearer {
                            service: service.to_string(),
                        });
                    }
                }
                Err(RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'upstream_auth': expected 'none' or 'bearer/<service>'"
                )))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginResources {
    pub cpus: Option<String>,
    pub memory: Option<String>,
    pub pids: Option<u32>,
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("plugin registry file not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
}

pub type PluginRegistry = HashMap<String, PluginEntry>;

fn valid_env_name(name: &str) -> bool {
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
        .any(|b| !(b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_'))
    {
        return false;
    }
    if RESERVED_ENV_NAMES.contains(&name) {
        return false;
    }
    if name.starts_with("DOCKER_") {
        return false;
    }
    true
}

fn parse_env(
    plugin_name: &str,
    config_val: &serde_yaml::Value,
) -> Result<Vec<(String, String)>, RegistryError> {
    let env_val = &config_val["env"];
    if env_val.is_null() {
        return Ok(Vec::new());
    }

    let mapping = env_val.as_mapping().ok_or_else(|| {
        RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'env': expected a mapping"
        ))
    })?;

    if mapping.len() > MAX_STATIC_ENV_ENTRIES {
        return Err(RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has too many 'env' entries: maximum is {MAX_STATIC_ENV_ENTRIES}"
        )));
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut result = Vec::with_capacity(mapping.len());

    for (key_val, value_val) in mapping {
        let key = key_val.as_str().ok_or_else(|| {
            RegistryError::Invalid(format!(
                "plugin '{plugin_name}' has invalid 'env' key: expected string"
            ))
        })?;

        // Reject non-string values with a helpful hint to quote them.
        let value = match value_val {
            serde_yaml::Value::String(s) => s.clone(),
            serde_yaml::Value::Bool(_) | serde_yaml::Value::Number(_) => {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{plugin_name}' env key '{key}': value must be a string (quote it: \"{value_val:?}\")"
                )));
            }
            _ => {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{plugin_name}' env key '{key}': value must be a string"
                )));
            }
        };

        if key.starts_with(SECRET_ENV_PREFIX) {
            return Err(RegistryError::Invalid(format!(
                "plugin '{plugin_name}' env key '{key}': names starting with '{SECRET_ENV_PREFIX}' are reserved for vault-derived secrets"
            )));
        }

        if key == CONFIG_ENV_NAME {
            return Err(RegistryError::Invalid(format!(
                "plugin '{plugin_name}' env key '{key}': reserved for structured config injection; use the 'config:' field instead"
            )));
        }

        if !valid_env_name(key) {
            return Err(RegistryError::Invalid(format!(
                "plugin '{plugin_name}' env key '{key}': invalid name (must match [A-Z_][A-Z0-9_]*, not reserved or DOCKER_-prefixed)"
            )));
        }

        if value.len() > MAX_ENV_VALUE_BYTES {
            return Err(RegistryError::Invalid(format!(
                "plugin '{plugin_name}' env key '{key}': value exceeds maximum size of {MAX_ENV_VALUE_BYTES} bytes"
            )));
        }

        if !seen.insert(key.to_string()) {
            return Err(RegistryError::Invalid(format!(
                "plugin '{plugin_name}' env key '{key}': duplicate key"
            )));
        }

        result.push((key.to_string(), value));
    }

    Ok(result)
}

fn parse_config(
    plugin_name: &str,
    config_val: &serde_yaml::Value,
) -> Result<Option<serde_json::Value>, RegistryError> {
    let raw = &config_val["config"];
    if raw.is_null() {
        return Ok(None);
    }

    // Most well-formed YAML structures round-trip into JSON cleanly; failures
    // mean the operator used a YAML feature that has no JSON equivalent
    // (e.g. a null map key).
    let json_val: serde_json::Value = serde_json::to_value(raw).map_err(|e| {
        RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'config': cannot represent as JSON: {e}"
        ))
    })?;

    if !json_val.is_object() {
        return Err(RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'config': expected a mapping (got {})",
            json_val_type_name(&json_val)
        )));
    }

    // Treat an empty mapping the same as absent: no env var injected.
    if json_val.as_object().unwrap().is_empty() {
        return Ok(None);
    }

    // Guard against pathologically large blobs at load time.
    let serialized =
        serde_json::to_string(&json_val).expect("Value already validated as JSON-serializable");
    if serialized.len() > MAX_ENV_VALUE_BYTES {
        return Err(RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'config': serialized JSON exceeds maximum size of {MAX_ENV_VALUE_BYTES} bytes"
        )));
    }

    Ok(Some(json_val))
}

fn json_val_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn parse_resources(
    plugin_name: &str,
    config_val: &serde_yaml::Value,
) -> Result<PluginResources, RegistryError> {
    let resources_val = &config_val["resources"];
    if resources_val.is_null() {
        return Ok(PluginResources::default());
    }
    let mapping = resources_val.as_mapping().ok_or_else(|| {
        RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'resources': expected a mapping"
        ))
    })?;

    let mut resources = PluginResources::default();
    for (key_val, value_val) in mapping {
        let key = key_val.as_str().ok_or_else(|| {
            RegistryError::Invalid(format!(
                "plugin '{plugin_name}' has invalid 'resources' key: expected string"
            ))
        })?;
        match key {
            "cpus" => {
                let value = value_val.as_str().ok_or_else(|| {
                    RegistryError::Invalid(format!(
                        "plugin '{plugin_name}' has invalid 'resources.cpus': expected non-empty string"
                    ))
                })?;
                if value.is_empty() {
                    return Err(RegistryError::Invalid(format!(
                        "plugin '{plugin_name}' has invalid 'resources.cpus': expected non-empty string"
                    )));
                }
                resources.cpus = Some(value.to_string());
            }
            "memory" => {
                let value = value_val.as_str().ok_or_else(|| {
                    RegistryError::Invalid(format!(
                        "plugin '{plugin_name}' has invalid 'resources.memory': expected non-empty string"
                    ))
                })?;
                if value.is_empty() {
                    return Err(RegistryError::Invalid(format!(
                        "plugin '{plugin_name}' has invalid 'resources.memory': expected non-empty string"
                    )));
                }
                resources.memory = Some(value.to_string());
            }
            "pids" => {
                let value = value_val.as_u64().ok_or_else(|| {
                    RegistryError::Invalid(format!(
                        "plugin '{plugin_name}' has invalid 'resources.pids': expected integer 1-4294967295"
                    ))
                })?;
                if value == 0 || value > u32::MAX as u64 {
                    return Err(RegistryError::Invalid(format!(
                        "plugin '{plugin_name}' has invalid 'resources.pids': expected integer 1-4294967295"
                    )));
                }
                resources.pids = Some(value as u32);
            }
            _ => {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{plugin_name}' has invalid 'resources' key: {key}"
                )))
            }
        }
    }

    Ok(resources)
}

pub fn load(path: &str) -> Result<PluginRegistry, RegistryError> {
    if !std::path::Path::new(path).exists() {
        return Err(RegistryError::NotFound(path.to_string()));
    }

    let content = std::fs::read_to_string(path)
        .map_err(|e| RegistryError::Invalid(format!("failed to read {path}: {e}")))?;

    let payload: serde_yaml::Value = serde_yaml::from_str(&content)
        .map_err(|e| RegistryError::Invalid(format!("failed to parse YAML: {e}")))?;

    if !payload.is_mapping() {
        return Err(RegistryError::Invalid(
            "invalid plugin registry: top-level YAML value must be a map".to_string(),
        ));
    }

    let plugins = payload["plugins"]
        .as_mapping()
        .filter(|m| !m.is_empty())
        .ok_or_else(|| {
            RegistryError::Invalid(
                "invalid plugin registry: 'plugins' must be a non-empty map".to_string(),
            )
        })?;

    let mut result = PluginRegistry::new();

    for (name_val, config_val) in plugins {
        let name = name_val.as_str().ok_or_else(|| {
            RegistryError::Invalid(format!(
                "invalid plugin name '{name_val:?}': expected ^[a-z][a-z0-9-]{{0,30}}$"
            ))
        })?;

        if !plugin_name_re().is_match(name) {
            return Err(RegistryError::Invalid(format!(
                "invalid plugin name '{name}': expected ^[a-z][a-z0-9-]{{0,30}}$"
            )));
        }

        if !config_val.is_mapping() {
            return Err(RegistryError::Invalid(format!(
                "invalid plugin config for '{name}': expected map"
            )));
        }

        let image = config_val["image"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                RegistryError::Invalid(format!(
                    "plugin '{name}' is missing required non-empty 'image'"
                ))
            })?
            .trim()
            .to_string();

        let port = if config_val["port"].is_null() {
            8000u16
        } else {
            let p = config_val["port"].as_u64().ok_or_else(|| {
                RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'port': expected integer 1-65535"
                ))
            })?;
            if p == 0 || p > 65535 {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'port': expected integer 1-65535"
                )));
            }
            p as u16
        };

        let network = if config_val["network"].is_null() {
            "botwork".to_string()
        } else {
            config_val["network"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    RegistryError::Invalid(format!(
                        "plugin '{name}' has invalid 'network': expected non-empty string"
                    ))
                })?
                .trim()
                .to_string()
        };

        let path = if config_val["path"].is_null() {
            "/".to_string()
        } else {
            let raw_path = config_val["path"].as_str().ok_or_else(|| {
                RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': expected non-empty string"
                ))
            })?;
            let path = raw_path.trim();
            if path.is_empty() {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': expected non-empty string"
                )));
            }
            if !path.starts_with('/') {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': must start with '/'"
                )));
            }
            if path.chars().any(|c| c.is_whitespace()) {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': must not contain whitespace"
                )));
            }
            if path.contains('?') || path.contains('#') {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': must not contain '?' or '#'"
                )));
            }
            if path != "/" && path.ends_with('/') {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': must not end with '/' unless path is exactly '/'"
                )));
            }
            path.to_string()
        };

        let upstream_auth = UpstreamAuth::from_yaml_value(name, &config_val["upstream_auth"])?;
        let env = parse_env(name, config_val)?;
        let resources = parse_resources(name, config_val)?;
        let config = parse_config(name, config_val)?;

        result.insert(
            name.to_string(),
            PluginEntry {
                image,
                port,
                network,
                path,
                upstream_auth,
                env,
                resources,
                config,
            },
        );
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_plugins(dir: &std::path::Path, content: &str) -> String {
        let path = dir.join("plugins.yaml");
        std::fs::write(&path, content).expect("write plugins");
        path.to_string_lossy().to_string()
    }

    #[test]
    fn load_path_defaults_and_explicit_values() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  a:
    image: botwork/mcp-a:local
  b:
    image: botwork/mcp-b:local
    path: /mcp
  c:
    image: botwork/mcp-c:local
    path: /api/v1
",
        );
        let loaded = load(&path).expect("load plugins");
        assert_eq!(loaded["a"].path, "/");
        assert_eq!(loaded["b"].path, "/mcp");
        assert_eq!(loaded["c"].path, "/api/v1");
    }

    #[test]
    fn load_rejects_invalid_path_values() {
        let cases = [
            ("/mcp/", "must not end with '/'"),
            ("mcp", "must start with '/'"),
            ("", "expected non-empty string"),
            ("   ", "expected non-empty string"),
            ("/mcp?x=1", "must not contain '?' or '#'"),
            ("/mcp#v1", "must not contain '?' or '#'"),
            ("/m cp", "must not contain whitespace"),
        ];

        for (bad_path, expected) in cases {
            let dir = tempdir().expect("tempdir");
            let path = write_plugins(
                dir.path(),
                &format!(
                    "plugins:\n  p:\n    image: botwork/mcp-p:local\n    path: \"{bad_path}\"\n"
                ),
            );
            let err = load(&path).expect_err("invalid path should fail");
            let err = err.to_string();
            assert!(
                err.contains("plugin 'p' has invalid 'path'") && err.contains(expected),
                "error '{err}' should mention '{expected}'"
            );
        }
    }

    #[test]
    fn load_upstream_auth_defaults_when_absent() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n",
        );

        let loaded = load(&path).expect("load plugins");
        assert_eq!(loaded["p"].upstream_auth, UpstreamAuth::None);
    }

    #[test]
    fn load_upstream_auth_explicit_none() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n    upstream_auth: none\n",
        );

        let loaded = load(&path).expect("load plugins");
        assert_eq!(loaded["p"].upstream_auth, UpstreamAuth::None);
    }

    #[test]
    fn load_upstream_auth_bearer_with_service() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n    upstream_auth: bearer/github.com\n",
        );

        let loaded = load(&path).expect("load plugins");
        assert_eq!(
            loaded["p"].upstream_auth,
            UpstreamAuth::Bearer {
                service: "github.com".to_string(),
            }
        );
        assert_eq!(loaded["p"].upstream_auth.to_wire(), "bearer/github.com");
    }

    #[test]
    fn load_rejects_bearer_without_service() {
        for upstream_auth in ["upstream_auth: bearer", "upstream_auth: bearer/"] {
            let dir = tempdir().expect("tempdir");
            let path = write_plugins(
                dir.path(),
                &format!("plugins:\n  p:\n    image: botwork/mcp-p:local\n    {upstream_auth}\n"),
            );

            let err = load(&path).expect_err("invalid upstream_auth should fail");
            let err = err.to_string();
            assert!(err.contains("plugin 'p' has invalid 'upstream_auth'"));
            assert!(err.contains("bearer requires a service"));
        }
    }

    #[test]
    fn load_rejects_unknown_upstream_auth() {
        let cases = [
            "upstream_auth: vault",
            "upstream_auth: None",
            "upstream_auth: \"\"",
            "upstream_auth: \"   \"",
            "upstream_auth: 42",
            "upstream_auth:\n      mode: bearer",
            "upstream_auth: bearer/github.com/pat",
            "upstream_auth: \"bearer/foo bar\"",
        ];

        for upstream_auth in cases {
            let dir = tempdir().expect("tempdir");
            let path = write_plugins(
                dir.path(),
                &format!("plugins:\n  p:\n    image: botwork/mcp-p:local\n    {upstream_auth}\n"),
            );
            let err = load(&path).expect_err("invalid upstream_auth should fail");
            let err = err.to_string();
            assert!(
                err.contains("plugin 'p' has invalid 'upstream_auth'"),
                "error '{err}' should mention upstream_auth invalid"
            );
            assert!(
                err.contains("expected 'none' or 'bearer/<service>'"),
                "error '{err}' should list accepted values"
            );
        }
    }

    #[test]
    fn load_env_defaults_empty_when_absent() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n",
        );
        let loaded = load(&path).expect("load plugins");
        assert!(loaded["p"].env.is_empty());
    }

    #[test]
    fn load_resources_defaults_when_absent() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n",
        );
        let loaded = load(&path).expect("load plugins");
        assert_eq!(loaded["p"].resources, PluginResources::default());
    }

    #[test]
    fn load_resources_accepts_partial_overrides() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    resources:
      memory: 4g
      pids: 1024
",
        );
        let loaded = load(&path).expect("load plugins");
        assert_eq!(
            loaded["p"].resources,
            PluginResources {
                cpus: None,
                memory: Some("4g".to_string()),
                pids: Some(1024),
            }
        );
    }

    #[test]
    fn load_resources_rejects_invalid_shape_and_unknown_keys() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n    resources: 123\n",
        );
        let err = load(&path).expect_err("invalid resources");
        assert!(err
            .to_string()
            .contains("invalid 'resources': expected a mapping"));

        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    resources:
      memory_limit: 4g
",
        );
        let err = load(&path).expect_err("unknown resources key");
        assert!(err
            .to_string()
            .contains("invalid 'resources' key: memory_limit"));
    }

    #[test]
    fn load_env_accepts_mapping() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    env:
      GITHUB_TOOLSETS: default,actions
      GITHUB_TERSE_DESCRIPTIONS: \"true\"
",
        );
        let loaded = load(&path).expect("load plugins");
        let env = &loaded["p"].env;
        assert_eq!(env.len(), 2);
        assert!(env.contains(&("GITHUB_TOOLSETS".to_string(), "default,actions".to_string())));
        assert!(env.contains(&("GITHUB_TERSE_DESCRIPTIONS".to_string(), "true".to_string())));
    }

    #[test]
    fn load_env_rejects_botwork_secret_prefix() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    env:
      BOTWORK_SECRET_SHADOW: val
",
        );
        let err = load(&path).expect_err("BOTWORK_SECRET_ prefix should fail");
        let err = err.to_string();
        assert!(err.contains("plugin 'p'"));
        assert!(err.contains("BOTWORK_SECRET_"));
    }

    #[test]
    fn load_env_rejects_botwork_mcp_config() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    env:
      BOTWORK_MCP_CONFIG: \"{}\"
",
        );
        let err = load(&path).expect_err("BOTWORK_MCP_CONFIG in env should fail");
        let err = err.to_string();
        assert!(err.contains("plugin 'p'"));
        assert!(err.contains("BOTWORK_MCP_CONFIG"));
        assert!(err.contains("'config:' field"));
    }

    #[test]
    fn load_config_defaults_none_when_absent() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n",
        );
        let loaded = load(&path).expect("load plugins");
        assert!(loaded["p"].config.is_none());
    }

    #[test]
    fn load_config_normalises_empty_mapping_to_none() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n    config: {}\n",
        );
        let loaded = load(&path).expect("load plugins");
        assert!(loaded["p"].config.is_none());
    }

    #[test]
    fn load_config_accepts_nested_structure() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    config:
      routes:
        - owner: botworkz
          token_env: BOTWORK_SECRET_GITHUB_BOTWORKZ
        - owner: phlax
          token_env: BOTWORK_SECRET_GITHUB_PHLAX
",
        );
        let loaded = load(&path).expect("load plugins");
        let config = loaded["p"].config.as_ref().expect("config should be Some");
        let routes = config["routes"].as_array().expect("routes array");
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0]["owner"].as_str().unwrap(), "botworkz");
    }

    #[test]
    fn load_config_rejects_non_mapping() {
        let cases = [
            ("config: \"a string\"", "string"),
            ("config: 42", "number"),
            ("config: true", "bool"),
            ("config:\n      - item1\n      - item2", "array"),
        ];
        for (entry, kind) in cases {
            let dir = tempdir().expect("tempdir");
            let path = write_plugins(
                dir.path(),
                &format!("plugins:\n  p:\n    image: botwork/mcp-p:local\n    {entry}\n"),
            );
            let err = load(&path).expect_err(&format!("non-mapping config ({kind}) should fail"));
            let err = err.to_string();
            assert!(
                err.contains("plugin 'p'") && err.contains("invalid 'config'"),
                "error '{err}' should mention plugin and invalid config"
            );
        }
    }

    #[test]
    fn load_config_rejects_oversized_value() {
        let entries: String = (0..1000)
            .map(|i| format!("      k{i}: \"{}\"\n", "x".repeat(64)))
            .collect();
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            &format!("plugins:\n  p:\n    image: botwork/mcp-p:local\n    config:\n{entries}"),
        );
        let err = load(&path).expect_err("oversized config should fail");
        let err = err.to_string();
        assert!(
            err.contains("plugin 'p'") && err.contains("exceeds maximum size"),
            "error should mention plugin and size: {err}"
        );
    }

    #[test]
    fn load_missing_file_raises_not_found() {
        let dir = tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist.yaml");
        let err = load(missing.to_str().unwrap()).expect_err("missing file should fail");
        assert!(matches!(err, RegistryError::NotFound(_)));
    }

    #[test]
    fn load_empty_plugins_map_raises() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(dir.path(), "plugins: {}\n");
        let err = load(&path).expect_err("empty plugins should fail");
        assert!(err
            .to_string()
            .contains("'plugins' must be a non-empty map"));
    }

    #[test]
    fn load_bad_name_raises() {
        for bad_name in ["Fs", "a/b", &"a".repeat(32)] {
            let dir = tempdir().expect("tempdir");
            let path = write_plugins(
                dir.path(),
                &format!("plugins:\n  {bad_name}:\n    image: botwork/x:local\n"),
            );
            let err = load(&path).expect_err("bad name should fail");
            assert!(
                err.to_string().contains("invalid plugin name"),
                "error: {err}"
            );
        }
    }
}
