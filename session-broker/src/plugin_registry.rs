use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;
use thiserror::Error;

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
            _ if value.starts_with("bearer/") => {
                let service = &value["bearer/".len()..];
                if service.is_empty() {
                    return Err(PluginRegistryError::Invalid(format!(
                        "plugin '{name}' has invalid 'upstream_auth': bearer requires a service: use bearer/<service>"
                    )));
                }
                if service.contains('/') {
                    return Err(PluginRegistryError::Invalid(format!(
                        "plugin '{name}' has invalid 'upstream_auth': unknown form '{value}'"
                    )));
                }
                if service.chars().any(char::is_whitespace) {
                    return Err(PluginRegistryError::Invalid(format!(
                        "plugin '{name}' has invalid 'upstream_auth': expected 'none' or 'bearer/<service>'"
                    )));
                }
                Ok(Self::Bearer {
                    service: service.to_string(),
                })
            }
            _ => Err(PluginRegistryError::Invalid(format!(
                "plugin '{name}' has invalid 'upstream_auth': expected 'none' or 'bearer/<service>'"
            ))),
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

        result.insert(
            name.to_string(),
            PluginConfig {
                image,
                port,
                network,
                path,
                upstream_auth,
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
    fn load_rejects_bearer_three_segment() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    upstream_auth: bearer/github.com/pat
",
        );

        let err = load(&path).expect_err("invalid upstream_auth should fail");
        let err = err.to_string();
        assert!(err.contains("plugin 'p' has invalid 'upstream_auth'"));
        assert!(err.contains("unknown form 'bearer/github.com/pat'"));
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
}
