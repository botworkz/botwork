use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;
use thiserror::Error;

use crate::secrets;

// Keep in sync with launcher/src/validate.rs RESERVED_ENV_NAMES.
const RESERVED_ENV_NAMES: &[&str] = &["PATH", "HOME", "USER", "LD_PRELOAD", "LD_LIBRARY_PATH"];

/// Maximum number of static env entries per plugin (leaves headroom under
/// launcher's MAX_ENV_ENTRIES = 64 for vault-derived secrets).
const MAX_STATIC_ENV_ENTRIES: usize = 32;

static PLUGIN_NAME_RE: OnceLock<Regex> = OnceLock::new();

fn plugin_name_re() -> &'static Regex {
    PLUGIN_NAME_RE.get_or_init(|| Regex::new(r"^[a-z][a-z0-9-]{0,30}$").unwrap())
}

#[derive(Debug, Clone, PartialEq)]
pub struct PluginConfig {
    pub image: String,
    pub port: u16,
    pub network: String,
    pub path: String,
    pub upstream_auth: UpstreamAuth,
    pub env: Vec<(String, String)>,
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
    fn from_yaml_value(name: &str, value: &serde_yaml::Value) -> Result<Self, PluginRegistryError> {
        if value.is_null() {
            return Ok(Self::None);
        }

        let Some(value) = value.as_str() else {
            return Err(PluginRegistryError::Invalid(format!(
                "plugin '{name}' has invalid 'upstream_auth': expected 'none' or 'bearer/<service>'"
            )));
        };

        match value {
            "none" => Ok(Self::None),
            "bearer" | "bearer/" => Err(PluginRegistryError::Invalid(format!(
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
                Err(PluginRegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'upstream_auth': expected 'none' or 'bearer/<service>'"
                )))
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum PluginRegistryError {
    #[error("plugin registry file not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
}

pub type PluginRegistry = HashMap<String, PluginConfig>;

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
) -> Result<Vec<(String, String)>, PluginRegistryError> {
    let env_val = &config_val["env"];
    if env_val.is_null() {
        return Ok(Vec::new());
    }

    let mapping = env_val.as_mapping().ok_or_else(|| {
        PluginRegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'env': expected a mapping"
        ))
    })?;

    if mapping.len() > MAX_STATIC_ENV_ENTRIES {
        return Err(PluginRegistryError::Invalid(format!(
            "plugin '{plugin_name}' has too many 'env' entries: maximum is {MAX_STATIC_ENV_ENTRIES}"
        )));
    }

    let mut result = Vec::with_capacity(mapping.len());

    for (key_val, value_val) in mapping {
        let key = key_val.as_str().ok_or_else(|| {
            PluginRegistryError::Invalid(format!(
                "plugin '{plugin_name}' has invalid 'env' key: expected string"
            ))
        })?;

        // Validate the name shape before the value so users see name errors
        // first when both are wrong.
        if key.starts_with(secrets::SECRET_ENV_PREFIX) {
            return Err(PluginRegistryError::Invalid(format!(
                "plugin '{plugin_name}' env key '{key}': names starting with '{}' are reserved for vault-derived secrets",
                secrets::SECRET_ENV_PREFIX
            )));
        }

        if !valid_env_name(key) {
            return Err(PluginRegistryError::Invalid(format!(
                "plugin '{plugin_name}' env key '{key}': invalid name (must match [A-Z_][A-Z0-9_]*, not reserved or DOCKER_-prefixed)"
            )));
        }

        // Reject non-string values with a helpful hint to quote them; render
        // the offending scalar literally (not as Rust Debug) so the hint is
        // copy-pasteable.
        let value = match value_val {
            serde_yaml::Value::String(s) => s.clone(),
            serde_yaml::Value::Bool(b) => {
                return Err(PluginRegistryError::Invalid(format!(
                    "plugin '{plugin_name}' env key '{key}': value must be a string (quote it: \"{b}\")"
                )));
            }
            serde_yaml::Value::Number(n) => {
                return Err(PluginRegistryError::Invalid(format!(
                    "plugin '{plugin_name}' env key '{key}': value must be a string (quote it: \"{n}\")"
                )));
            }
            _ => {
                return Err(PluginRegistryError::Invalid(format!(
                    "plugin '{plugin_name}' env key '{key}': value must be a string"
                )));
            }
        };

        if value.len() > secrets::MAX_ENV_VALUE_BYTES {
            return Err(PluginRegistryError::Invalid(format!(
                "plugin '{plugin_name}' env key '{key}': value exceeds maximum size of {} bytes",
                secrets::MAX_ENV_VALUE_BYTES
            )));
        }

        // Note: duplicate keys within a single plugin's env block are not
        // explicitly rejected here. serde_yaml's Mapping deduplicates on
        // insert (last-wins), which matches YAML 1.2 sequencing rules and is
        // what most operators expect. Documented in README.
        result.push((key.to_string(), value));
    }

    Ok(result)
}

pub fn load(path: &str) -> Result<PluginRegistry, PluginRegistryError> {
    if !std::path::Path::new(path).exists() {
        return Err(PluginRegistryError::NotFound(path.to_string()));
    }

    let content = std::fs::read_to_string(path)
        .map_err(|e| PluginRegistryError::Invalid(format!("failed to read {path}: {e}")))?;

    let payload: serde_yaml::Value = serde_yaml::from_str(&content)
        .map_err(|e| PluginRegistryError::Invalid(format!("failed to parse YAML: {e}")))?;

    if !payload.is_mapping() {
        return Err(PluginRegistryError::Invalid(
            "invalid plugin registry: top-level YAML value must be a map".to_string(),
        ));
    }

    let plugins = payload["plugins"]
        .as_mapping()
        .filter(|m| !m.is_empty())
        .ok_or_else(|| {
            PluginRegistryError::Invalid(
                "invalid plugin registry: 'plugins' must be a non-empty map".to_string(),
            )
        })?;

    let mut result = PluginRegistry::new();

    for (name_val, config_val) in plugins {
        let name = name_val.as_str().ok_or_else(|| {
            PluginRegistryError::Invalid(format!(
                "invalid plugin name '{name_val:?}': expected ^[a-z][a-z0-9-]{{0,30}}$"
            ))
        })?;

        if !plugin_name_re().is_match(name) {
            return Err(PluginRegistryError::Invalid(format!(
                "invalid plugin name '{name}': expected ^[a-z][a-z0-9-]{{0,30}}$"
            )));
        }

        if !config_val.is_mapping() {
            return Err(PluginRegistryError::Invalid(format!(
                "invalid plugin config for '{name}': expected map"
            )));
        }

        let image = config_val["image"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                PluginRegistryError::Invalid(format!(
                    "plugin '{name}' is missing required non-empty 'image'"
                ))
            })?
            .trim()
            .to_string();

        let port = if config_val["port"].is_null() {
            8000u16
        } else {
            let p = config_val["port"].as_u64().ok_or_else(|| {
                PluginRegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'port': expected integer 1-65535"
                ))
            })?;
            if p == 0 || p > 65535 {
                return Err(PluginRegistryError::Invalid(format!(
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
                    PluginRegistryError::Invalid(format!(
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
                PluginRegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': expected non-empty string"
                ))
            })?;
            let path = raw_path.trim();
            if path.is_empty() {
                return Err(PluginRegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': expected non-empty string"
                )));
            }
            if !path.starts_with('/') {
                return Err(PluginRegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': must start with '/'"
                )));
            }
            if path.chars().any(|c| c.is_whitespace()) {
                return Err(PluginRegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': must not contain whitespace"
                )));
            }
            if path.contains('?') || path.contains('#') {
                return Err(PluginRegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': must not contain '?' or '#'"
                )));
            }
            if path != "/" && path.ends_with('/') {
                return Err(PluginRegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': must not end with '/' unless path is exactly '/'"
                )));
            }
            path.to_string()
        };
        let upstream_auth = UpstreamAuth::from_yaml_value(name, &config_val["upstream_auth"])?;
        let env = parse_env(name, config_val)?;

        result.insert(
            name.to_string(),
            PluginConfig {
                image,
                port,
                network,
                path,
                upstream_auth,
                env,
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
            "plugins:
  p:
    image: botwork/mcp-p:local
",
        );

        let loaded = load(&path).expect("load plugins");
        assert_eq!(loaded["p"].upstream_auth, UpstreamAuth::None);
    }

    #[test]
    fn load_upstream_auth_explicit_none() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    upstream_auth: none
",
        );

        let loaded = load(&path).expect("load plugins");
        assert_eq!(loaded["p"].upstream_auth, UpstreamAuth::None);
    }

    #[test]
    fn load_upstream_auth_bearer_with_service() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    upstream_auth: bearer/github.com
",
        );

        let loaded = load(&path).expect("load plugins");
        assert_eq!(
            loaded["p"].upstream_auth,
            UpstreamAuth::Bearer {
                service: "github.com".to_string(),
            }
        );
    }

    #[test]
    fn load_upstream_auth_bearer_with_dotted_service() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    upstream_auth: bearer/npm-registry
",
        );

        let loaded = load(&path).expect("load plugins");
        assert_eq!(
            loaded["p"].upstream_auth,
            UpstreamAuth::Bearer {
                service: "npm-registry".to_string(),
            }
        );
    }

    #[test]
    fn load_rejects_bearer_without_service() {
        for upstream_auth in ["upstream_auth: bearer", "upstream_auth: bearer/"] {
            let dir = tempdir().expect("tempdir");
            let path = write_plugins(
                dir.path(),
                &format!(
                    "plugins:
  p:
    image: botwork/mcp-p:local
    {upstream_auth}
"
                ),
            );

            let err = load(&path).expect_err("invalid upstream_auth should fail");
            let err = err.to_string();
            assert!(err.contains("plugin 'p' has invalid 'upstream_auth'"));
            assert!(err.contains("bearer requires a service"));
        }
    }

    #[test]
    fn load_rejects_bearer_empty_service() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    upstream_auth: bearer/
",
        );

        let err = load(&path).expect_err("invalid upstream_auth should fail");
        let err = err.to_string();
        assert!(err.contains("plugin 'p' has invalid 'upstream_auth'"));
        assert!(err.contains("bearer requires a service"));
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
            "plugins:
  p:
    image: botwork/mcp-p:local
",
        );
        let loaded = load(&path).expect("load plugins");
        assert!(loaded["p"].env.is_empty());
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
    fn load_env_rejects_non_string_value() {
        let cases = [
            ("SOME_FLAG: true", "bool"),
            ("SOME_COUNT: 42", "number"),
            ("SOME_LIST:\n      - a\n      - b", "list"),
        ];

        for (env_entry, kind) in cases {
            let dir = tempdir().expect("tempdir");
            let path = write_plugins(
                dir.path(),
                &format!(
                    "plugins:\n  p:\n    image: botwork/mcp-p:local\n    env:\n      {env_entry}\n"
                ),
            );
            let err = load(&path).expect_err(&format!("non-string {kind} value should fail"));
            assert!(
                err.to_string().contains("plugin 'p'"),
                "error should mention plugin name: {err}"
            );
        }
    }

    #[test]
    fn load_env_non_string_value_hint_renders_literal_scalar() {
        // Bool: hint must say (quote it: "true"), not (quote it: "Bool(true)").
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    env:
      SOME_FLAG: true
",
        );
        let err = load(&path).expect_err("bool value should fail").to_string();
        assert!(
            err.contains("quote it: \"true\""),
            "error should hint with literal scalar 'true', got: {err}"
        );
        assert!(
            !err.contains("Bool("),
            "error must not leak Rust Debug formatting: {err}"
        );

        // Number: same check with an integer.
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    env:
      SOME_COUNT: 42
",
        );
        let err = load(&path)
            .expect_err("number value should fail")
            .to_string();
        assert!(
            err.contains("quote it: \"42\""),
            "error should hint with literal scalar '42', got: {err}"
        );
        assert!(
            !err.contains("Number("),
            "error must not leak Rust Debug formatting: {err}"
        );
    }

    #[test]
    fn load_env_validates_name_before_value() {
        // Both name and value are wrong; user should see the name error
        // first so they fix the more fundamental issue.
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    env:
      lowercase_key: true
",
        );
        let err = load(&path)
            .expect_err("invalid name + value should fail")
            .to_string();
        assert!(
            err.contains("invalid name"),
            "name error should come first, got: {err}"
        );
        assert!(
            !err.contains("must be a string"),
            "value error must not be reported when name is already invalid: {err}"
        );
    }

    #[test]
    fn load_env_rejects_invalid_name_shape() {
        let cases = [
            ("lowercase_key: val", "lowercase"),
            ("1LEADING_DIGIT: val", "leading digit"),
            ("HYPHEN-KEY: val", "hyphen"),
            ("EQUALS=KEY: val", "equals"),
        ];

        for (env_entry, reason) in cases {
            let dir = tempdir().expect("tempdir");
            let path = write_plugins(
                dir.path(),
                &format!(
                    "plugins:\n  p:\n    image: botwork/mcp-p:local\n    env:\n      {env_entry}\n"
                ),
            );
            let err = load(&path).expect_err(&format!("invalid name ({reason}) should fail"));
            assert!(
                err.to_string().contains("plugin 'p'"),
                "error should mention plugin: {err}"
            );
        }
    }

    #[test]
    fn load_env_rejects_reserved_name() {
        for reserved in ["PATH", "HOME", "USER", "LD_PRELOAD", "LD_LIBRARY_PATH"] {
            let dir = tempdir().expect("tempdir");
            let path = write_plugins(
                dir.path(),
                &format!(
                    "plugins:\n  p:\n    image: botwork/mcp-p:local\n    env:\n      {reserved}: val\n"
                ),
            );
            let err = load(&path).expect_err(&format!("reserved name {reserved} should fail"));
            assert!(
                err.to_string().contains("plugin 'p'"),
                "error should mention plugin: {err}"
            );
        }
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
        assert!(
            err.contains("plugin 'p'"),
            "error should mention plugin: {err}"
        );
        assert!(
            err.contains("BOTWORK_SECRET_"),
            "error should mention the reserved prefix: {err}"
        );
    }

    #[test]
    fn load_env_rejects_docker_prefix() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    env:
      DOCKER_HOST: val
",
        );
        let err = load(&path).expect_err("DOCKER_ prefix should fail");
        assert!(
            err.to_string().contains("plugin 'p'"),
            "error should mention plugin: {err}"
        );
    }

    #[test]
    fn load_env_rejects_value_over_64kib() {
        let dir = tempdir().expect("tempdir");
        let big_value = "x".repeat(64 * 1024 + 1);
        let path = write_plugins(
            dir.path(),
            &format!(
                "plugins:\n  p:\n    image: botwork/mcp-p:local\n    env:\n      BIG_VALUE: \"{big_value}\"\n"
            ),
        );
        let err = load(&path).expect_err("oversized value should fail");
        let err = err.to_string();
        assert!(
            err.contains("plugin 'p'"),
            "error should mention plugin: {err}"
        );
        assert!(
            err.contains("exceeds maximum size"),
            "error should mention size: {err}"
        );
    }

    #[test]
    fn load_env_rejects_more_than_32_entries() {
        let entries: String = (0..33).map(|i| format!("      KEY_{i}: value\n")).collect();
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            &format!("plugins:\n  p:\n    image: botwork/mcp-p:local\n    env:\n{entries}"),
        );
        let err = load(&path).expect_err("more than 32 entries should fail");
        let err = err.to_string();
        assert!(
            err.contains("plugin 'p'"),
            "error should mention plugin: {err}"
        );
        assert!(
            err.contains("too many"),
            "error should mention count: {err}"
        );
    }

    #[test]
    fn load_env_duplicate_keys_last_wins() {
        // YAML 1.2 and serde_yaml both deduplicate map keys on insert with
        // last-value-wins semantics. Document and lock that behaviour: a YAML
        // file with two identical keys is accepted, and the second value is
        // what ends up in the parsed config.
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    env:
      FOO: first
      FOO: second
",
        );
        let loaded = load(&path).expect("duplicate keys should parse (last-wins)");
        let env = &loaded["p"].env;
        assert_eq!(env.len(), 1, "duplicate key should collapse to one entry");
        assert_eq!(
            env[0],
            ("FOO".to_string(), "second".to_string()),
            "last value should win"
        );
    }
}
