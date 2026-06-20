use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::log_info;

pub fn utc_now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Error returned by `load_from_disk` when the on-disk registry cannot be
/// adopted as-is.  This is distinct from "file does not exist" (caller's
/// concern) — these variants represent files that exist but cannot be safely
/// loaded.
#[derive(Debug)]
pub enum RegistryLoadError {
    /// I/O or JSON-parse failure on the registry file itself.
    Io(String),
    /// One or more session entries were missing required fields (tenant,
    /// workspace) — almost always the signature of a pre-workspace registry
    /// from before the URL-shape cutover.  Refusing to load is intentional:
    /// silently dropping such entries would orphan the underlying containers
    /// (no DELETE-routed teardown, no admin visibility), so we make the
    /// operator deal with the migration explicitly.
    SchemaMismatch {
        /// Container names of the offending entries, surfaced so the operator
        /// can `docker rm -f` them after deciding the registry file is safe
        /// to remove.
        offending: Vec<String>,
    },
}

impl std::fmt::Display for RegistryLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(msg) => write!(f, "{msg}"),
            Self::SchemaMismatch { offending } => write!(
                f,
                "session registry contains {} entries from a pre-workspace broker (missing tenant/workspace); \
                 refusing to load to avoid silently orphaning containers. \
                 Affected containers: [{}]. \
                 To migrate: stop the broker, `docker rm -f` any orphaned containers, \
                 remove the registry file, then restart.",
                offending.len(),
                offending.join(", ")
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub container: String,
    pub staging_path: String,
    pub tenant: String,
    pub workspace: String,
    pub mcp_session_id: Option<String>,
    pub agent_id: Option<String>,
    pub image: String,
    pub created_at: String,
    pub bound_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryData {
    pub version: u32,
    pub updated_at: String,
    pub sessions: HashMap<String, SessionEntry>,
}

impl Default for RegistryData {
    fn default() -> Self {
        Self {
            version: 1,
            updated_at: utc_now(),
            sessions: HashMap::new(),
        }
    }
}

pub struct SessionRegistry {
    pub path: String,
    data: Mutex<RegistryData>,
}

impl SessionRegistry {
    pub fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
            data: Mutex::new(RegistryData::default()),
        }
    }

    pub async fn read(&self) -> RegistryData {
        self.data.lock().await.clone()
    }

    /// Loads the registry from disk and reconciles it against the live docker
    /// container set.  Returns `Err` if the on-disk file exists but cannot be
    /// safely adopted (e.g. pre-workspace schema); callers should treat such
    /// errors as fatal and refuse to start.
    pub async fn load_and_reconcile(&self) -> Result<(), RegistryLoadError> {
        if !Path::new(&self.path).exists() {
            return Ok(());
        }
        let mut data = self.data.lock().await;
        let disk_data = load_from_disk(&self.path)?;
        *data = disk_data;

        let running = try_list_running_session_containers();
        if let Some(running) = running {
            data.sessions.retain(|name, _| running.contains(name));
        }
        data.updated_at = utc_now();
        if let Err(e) = write_atomic(&self.path, &data) {
            log_info(&format!("failed to write session registry: {e}"));
        }
        Ok(())
    }

    pub async fn record_spawn(
        &self,
        container: &str,
        staging_path: &str,
        tenant: &str,
        workspace: &str,
        image: &str,
        created_at: &str,
    ) {
        let mut data = self.data.lock().await;
        data.sessions.insert(
            container.to_string(),
            SessionEntry {
                container: container.to_string(),
                staging_path: staging_path.to_string(),
                tenant: tenant.to_string(),
                workspace: workspace.to_string(),
                mcp_session_id: None,
                agent_id: None,
                image: image.to_string(),
                created_at: created_at.to_string(),
                bound_at: None,
            },
        );
        data.updated_at = utc_now();
        if let Err(e) = write_atomic(&self.path, &data) {
            log_info(&format!("failed to write session registry: {e}"));
        }
    }

    pub async fn record_mcp_session_id(&self, container: &str, mcp_session_id: &str) {
        let mut data = self.data.lock().await;
        let session = match data.sessions.get_mut(container) {
            Some(s) => s,
            None => return,
        };
        if let Some(existing) = &session.mcp_session_id {
            if existing != mcp_session_id {
                log_info(&format!(
                    "registry ignoring mcp_session_id overwrite (container={container} existing={existing} incoming={mcp_session_id})"
                ));
            }
            return;
        }
        session.mcp_session_id = Some(mcp_session_id.to_string());
        data.updated_at = utc_now();
        if let Err(e) = write_atomic(&self.path, &data) {
            log_info(&format!("failed to write session registry: {e}"));
        }
    }

    pub async fn record_agent_bound(&self, container: &str, agent_id: &str, bound_at: &str) {
        let mut data = self.data.lock().await;
        let session = match data.sessions.get_mut(container) {
            Some(s) => s,
            None => return,
        };
        if let Some(existing) = &session.agent_id {
            if existing != agent_id {
                log_info(&format!(
                    "registry ignoring agent_id overwrite (container={container} existing={existing} incoming={agent_id})"
                ));
            }
            return;
        }
        session.agent_id = Some(agent_id.to_string());
        session.bound_at = Some(bound_at.to_string());
        data.updated_at = utc_now();
        if let Err(e) = write_atomic(&self.path, &data) {
            log_info(&format!("failed to write session registry: {e}"));
        }
    }

    pub async fn record_teardown(&self, container: &str) {
        let mut data = self.data.lock().await;
        if data.sessions.remove(container).is_none() {
            log_info(&format!(
                "registry teardown: container={container} not present (no-op)"
            ));
            return;
        }
        log_info(&format!("registry teardown: container={container}"));
        data.updated_at = utc_now();
        if let Err(e) = write_atomic(&self.path, &data) {
            log_info(&format!("failed to write session registry: {e}"));
        }
    }
}

fn load_from_disk(path: &str) -> Result<RegistryData, RegistryLoadError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| RegistryLoadError::Io(format!("failed to read {path}: {e}")))?;
    let payload: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| RegistryLoadError::Io(format!("failed to parse {path}: {e}")))?;

    if !payload.is_object() {
        return Ok(RegistryData::default());
    }

    let version = payload["version"].as_u64().unwrap_or(1) as u32;
    let updated_at = payload["updated_at"].as_str().unwrap_or("").to_string();
    let updated_at = if updated_at.is_empty() {
        utc_now()
    } else {
        updated_at
    };

    let sessions = match payload["sessions"].as_object() {
        None => HashMap::new(),
        Some(obj) => {
            let mut map = HashMap::new();
            let mut offending: Vec<String> = Vec::new();
            for (k, v) in obj {
                match serde_json::from_value::<SessionEntry>(v.clone()) {
                    Ok(entry) => {
                        map.insert(k.clone(), entry);
                    }
                    Err(e) => {
                        // The most likely cause is a pre-workspace entry
                        // missing `tenant`/`workspace`.  Surface the
                        // container name so the operator can clean it up.
                        log_info(&format!("rejecting malformed session entry '{k}': {e}"));
                        offending.push(k.clone());
                    }
                }
            }
            if !offending.is_empty() {
                return Err(RegistryLoadError::SchemaMismatch { offending });
            }
            map
        }
    };

    Ok(RegistryData {
        version,
        updated_at,
        sessions,
    })
}

fn write_atomic(path: &str, data: &RegistryData) -> Result<(), String> {
    let tmp_path = format!("{path}.tmp");
    let parent = Path::new(path).parent();
    if let Some(parent) = parent {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create parent directory: {e}"))?;
        }
    }

    let json = serde_json::to_string(data)
        .map_err(|e| format!("failed to serialize session registry: {e}"))?;

    std::fs::write(&tmp_path, json.as_bytes())
        .map_err(|e| format!("failed to write {tmp_path}: {e}"))?;

    std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("failed to chmod {tmp_path}: {e}"))?;

    std::fs::rename(&tmp_path, path)
        .map_err(|e| format!("failed to rename {tmp_path} -> {path}: {e}"))?;

    Ok(())
}

pub fn try_list_running_session_containers() -> Option<std::collections::HashSet<String>> {
    let result = std::process::Command::new("docker")
        .args([
            "ps",
            "--filter",
            "name=^mcp_session_",
            "--format",
            "{{.Names}}",
        ])
        .output();

    match result {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log_info("docker CLI not available in broker container; skipping reconcile");
            None
        }
        Err(e) => {
            log_info(&format!("docker ps failed: {e}"));
            None
        }
        Ok(output) if !output.status.success() => None,
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            Some(
                stdout
                    .lines()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
            )
        }
    }
}

/// Checks whether a specific container is currently running.
///
/// Returns `Some(true)` if running, `Some(false)` if the container exists but
/// is not running or does not exist, `None` when the docker CLI is unavailable
/// (treat as "unknown / assume alive" to avoid false-positive eviction).
pub fn is_container_running(name: &str) -> Option<bool> {
    let result = std::process::Command::new("docker")
        .args(["inspect", "--format", "{{.State.Running}}", name])
        .output();

    match result {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log_info("docker CLI not available; skipping liveness check");
            None
        }
        Err(e) => {
            log_info(&format!("docker inspect failed for {name}: {e}"));
            None
        }
        Ok(output) if !output.status.success() => {
            // Container not known to docker
            Some(false)
        }
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Some(stdout == "true")
        }
    }
}
