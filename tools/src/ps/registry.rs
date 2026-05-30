use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, Deserialize)]
pub struct SessionEntry {
    pub container: String,
    pub staging_path: String,
    pub mcp_session_id: Option<String>,
    pub agent_id: Option<String>,
    pub image: String,
    pub created_at: String,
    pub bound_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegistryData {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub sessions: HashMap<String, SessionEntry>,
}

const fn default_version() -> u32 {
    1
}

pub fn load_registry(path: &Path) -> Result<RegistryData, RegistryError> {
    let content = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&content)?)
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("failed to read session registry: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse session registry JSON: {0}")]
    Parse(#[from] serde_json::Error),
}
