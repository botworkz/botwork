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

        result.insert(
            name.to_string(),
            PluginConfig {
                image,
                port,
                network,
            },
        );
    }

    Ok(result)
}
