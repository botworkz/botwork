//! Cold-start recovery: rebuild `transport_sessions` from `docker ps` +
//! `docker inspect` + `session_worker` rows.
//!
//! Pre-RFE-#105 the recovery shape was: load `/var/lib/botwork/sessions.json`,
//! filter against running containers, adopt. Round-3 (the cutover this
//! file is part of) replaces that with:
//!
//! 1. **`docker ps --filter name=mcp_session_*`** to enumerate every
//!    currently-running plugin container on the host;
//! 2. **`docker inspect`** each one to pull its `io.botworkz.*`
//!    labels (set by the launcher under RFE #105 round-3 step-1, see
//!    #115) and its IPv4 on the plugin network;
//! 3. **`SELECT … WHERE reaped_at IS NULL FROM session_worker`** to
//!    pull the matching DB rows;
//! 4. **Reconcile** the two sets:
//!    - container present + row present → seed `transport_sessions`;
//!    - container present + row absent → **reap immediately** (per
//!      the design call: "if it's not in DB it shouldn't be
//!      running"). `docker rm -f` the container, log a warn;
//!    - row present + container absent → mark `reaped_at = now()` on
//!      the DB row so the next recovery cycle sees it as audit-only.
//!
//! The "reap-immediately" posture for orphan containers matches the
//! existing failure model: spawn-time INSERT into session_worker is
//! fail-soft (warn + continue), so the only way a container exists
//! without a row is the broker crashed between `docker run` and the
//! INSERT. In that window control-plane hasn't been told about the
//! session and routing has never worked, so reaping is the cleanest
//! posture.
//!
//! # mcp_session_id
//!
//! Only rows whose `mcp_session_id` is non-empty get into
//! `transport_sessions` — keying is by `mcp_session_id`, and a row
//! whose initialize response hasn't landed yet has nothing useful
//! for routing. Such rows survive the recovery (the next request
//! against the container will surface its session id and backfill
//! via `record_mcp_session_id`); but in practice the spawn-to-
//! initialize-response window is sub-second and a broker restart
//! mid-window is exceedingly rare.

use std::collections::HashSet;
use std::sync::Arc;

use sea_orm::DatabaseConnection;
use tracing::warn;

use crate::config_broker;
use crate::log_info;
use crate::session_worker::{LiveWorker, SessionWorkerWriter};
use crate::{AppState, TransportState};

/// Pull the running `mcp_session_*` set out of `docker ps`. Returns
/// `None` when docker is unreachable so the caller can skip recovery
/// (rather than treat the empty set as authoritative and reap every
/// DB row).
fn list_running_session_containers() -> Option<HashSet<String>> {
    let output = std::process::Command::new("docker")
        .args([
            "ps",
            "--filter",
            "name=^mcp_session_",
            "--format",
            "{{.Names}}",
        ])
        .output();
    match output {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            warn!(
                "[session-broker] docker CLI not available during recovery; \
                 skipping cold-start reconciliation"
            );
            None
        }
        Err(err) => {
            warn!("[session-broker] docker ps failed during recovery: {err}");
            None
        }
        Ok(out) if !out.status.success() => {
            warn!(
                "[session-broker] docker ps exited {:?} during recovery; \
                 skipping reconciliation",
                out.status.code()
            );
            None
        }
        Ok(out) => Some(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        ),
    }
}

/// Forces a running container off the host. Used to enforce the
/// "live container with no DB row" reap posture during cold start.
fn force_remove_container(container_name: &str) {
    let result = std::process::Command::new("docker")
        .args(["rm", "-f", container_name])
        .output();
    match result {
        Ok(out) if out.status.success() => {
            log_info(&format!(
                "recovery: reaped orphan container {container_name} (no matching DB row)"
            ));
        }
        Ok(out) => {
            warn!(
                "[session-broker] recovery: docker rm -f {container_name} \
                 exited {:?} (stderr: {})",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Err(err) => {
            warn!(
                "[session-broker] recovery: docker rm -f {container_name} \
                 failed to spawn: {err}"
            );
        }
    }
}

/// Entry point: enumerate live containers, walk `session_worker` rows,
/// reconcile both into `AppState::transport_sessions`.
///
/// Idempotent — re-running has no effect once steady state is reached.
/// Called once at startup from `run()`.
pub async fn recover_live_workers(state: &AppState) {
    // Production always wires both. Tests use `None` for either; in
    // both cases we have nothing to do here. (Pre-PR2 tests + the
    // ext_proc_test suite intentionally hand AppState without DB.)
    let (Some(writer), Some(db)) = (state.session_worker_writer.as_ref(), state.db.as_ref()) else {
        log_info("recovery: session_worker_writer not configured; skipping");
        return;
    };

    let running = match list_running_session_containers() {
        Some(set) => set,
        None => return,
    };

    let live_rows = match writer.list_live().await {
        Ok(rows) => rows,
        Err(err) => {
            warn!(
                "[session-broker] recovery: SELECT live session_worker rows failed: {err}; \
                 skipping reconciliation"
            );
            return;
        }
    };

    log_info(&format!(
        "recovery: docker has {} container(s), DB has {} live worker row(s)",
        running.len(),
        live_rows.len()
    ));

    let mut row_by_name = std::collections::HashMap::new();
    for row in &live_rows {
        row_by_name.insert(row.container_name.clone(), row.clone());
    }

    // Orphan containers (docker has it, DB doesn't) → reap.
    for container in &running {
        if !row_by_name.contains_key(container) {
            force_remove_container(container);
        }
    }

    // DB rows without a running container → mark reaped so the
    // session_worker table reflects on-host reality.
    for row in &live_rows {
        if !running.contains(&row.container_name) {
            log_info(&format!(
                "recovery: marking session_worker row reaped (container={} not running)",
                row.container_name
            ));
            writer.record_reap(&row.container_name).await;
        }
    }

    // The intersection — DB row + live container — needs to be
    // rehydrated into AppState.transport_sessions so the broker can
    // route to it. To do that we need the plugin's static descriptor
    // (port, path, upstream_auth, egress) which the row doesn't
    // carry; resolve via config-broker the same way the spawn path
    // does. Anything missing the corresponding plugin name on the
    // row makes us skip + warn.
    let mut seeded = 0usize;
    for container in &running {
        let Some(row) = row_by_name.get(container) else {
            continue;
        };
        // Row carries `plugin_id`, not the plugin name. Resolve the
        // name from the DB (single SELECT per recovered container —
        // recovery is rare, so the lack of caching is acceptable).
        let plugin_name = match resolve_plugin_name(db, row.plugin_id).await {
            Some(name) => name,
            None => {
                warn!(
                    "[session-broker] recovery: cannot resolve plugin name for \
                     container={container} plugin_id={plugin_id}; row will be \
                     reaped on next cycle once container exits",
                    plugin_id = row.plugin_id
                );
                continue;
            }
        };

        // Tenant + workspace come from the labels stamped at spawn
        // time (#115). `docker inspect` gives us those plus the
        // container's IPv4 on the plugin network.
        let Some(inspect) = inspect_container_for_recovery(container) else {
            warn!(
                "[session-broker] recovery: docker inspect failed for \
                 {container}; skipping rehydration"
            );
            continue;
        };

        if inspect.container_ip != row.container_ip {
            // The row's IP is from the last spawn; if it drifted
            // (docker restart can re-IPAM) we trust the inspect
            // result so routing reaches the right address.
            log_info(&format!(
                "recovery: container_ip drift for {container} \
                 (db={row_ip} live={live_ip}); using live",
                row_ip = row.container_ip,
                live_ip = inspect.container_ip
            ));
        }

        // No mcp_session_id yet on the row? The container is between
        // spawn and initialize-response. Routing keys on
        // mcp_session_id so there's nothing to seed; the next
        // request will repopulate via the normal handler path.
        let mcp_session_id = if row.mcp_session_id.is_empty() {
            log_info(&format!(
                "recovery: container={container} has no mcp_session_id yet; \
                 leaving for handler path to populate"
            ));
            continue;
        } else {
            row.mcp_session_id.clone()
        };

        // Resolve the plugin descriptor so we can populate the
        // static routing fields on TransportState.
        let descriptor = match config_broker::resolve(
            &state.config_broker_endpoint,
            &inspect.tenant,
            &inspect.workspace,
            &plugin_name,
            std::time::Duration::from_secs(5),
        )
        .await
        {
            Ok(desc) => desc,
            Err(err) => {
                warn!(
                    "[session-broker] recovery: config-broker resolve failed for \
                     tenant={tenant} workspace={workspace} plugin={plugin}: {err}",
                    tenant = inspect.tenant,
                    workspace = inspect.workspace,
                    plugin = plugin_name
                );
                continue;
            }
        };

        let transport = TransportState {
            container_name: container.clone(),
            container_ip: inspect.container_ip.clone(),
            staging_token: inspect.staging_token.clone().unwrap_or_default(),
            tenant_name: inspect.tenant.clone(),
            workspace: inspect.workspace.clone(),
            plugin_name: plugin_name.clone(),
            port: descriptor.port,
            path: descriptor.path.clone(),
            upstream_auth: descriptor.upstream_auth.clone(),
            // upstream_authorization gets rebuilt on first request
            // (the spawn path called auth-broker at spawn time; the
            // recovery path defers that to the next inbound request).
            upstream_authorization: None,
            // agent_id may already exist on the DB row; surface it
            // for the admin /sessions view and so the next bind
            // doesn't see a phantom "agent_id changed" warning.
            agent_id: inspect.agent_session_label.clone(),
            egress_policy: descriptor.egress.clone(),
        };

        let mut sessions = state.transport_sessions.lock().await;
        sessions.insert(mcp_session_id.clone(), transport);
        seeded += 1;
    }

    log_info(&format!(
        "recovery: seeded {seeded} transport session(s) into in-memory map"
    ));

    let _ = (db, writer);
}

/// Result of `docker inspect <container>` filtered down to the bits
/// we use during recovery.
struct InspectResult {
    container_ip: String,
    tenant: String,
    workspace: String,
    /// The agent-session-id label from #115, if present. NOT stamped
    /// at spawn (labels can't be added after `docker run`), so this
    /// is always None in PR2's shape. Kept here so a future iteration
    /// that adds the label via `docker container update --label-add`
    /// (or via a sidecar metadata channel) can backfill the field.
    agent_session_label: Option<String>,
    /// Inferred from the workspace mount path. NULL if the container
    /// was started without `--workspace`.
    staging_token: Option<String>,
}

fn inspect_container_for_recovery(container_name: &str) -> Option<InspectResult> {
    let output = std::process::Command::new("docker")
        .args(["inspect", "--format", "{{json .}}", container_name])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    // `inspect` returns an array of one object for a single arg, but
    // with `--format {{json .}}` we get just the object — keep
    // future-flexibility for either shape.
    let obj = match raw {
        serde_json::Value::Array(mut arr) if !arr.is_empty() => arr.remove(0),
        v => v,
    };

    // Labels live at .Config.Labels (a string→string map).
    let labels = obj
        .pointer("/Config/Labels")
        .and_then(serde_json::Value::as_object);
    let tenant = labels
        .and_then(|m| m.get("io.botworkz.tenant"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)?;
    let workspace = labels
        .and_then(|m| m.get("io.botworkz.workspace"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)?;
    let agent_session_label = labels
        .and_then(|m| m.get("io.botworkz.agent_session"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    // Container IPv4 on `botwork-plugin`. Same shape we use during
    // spawn — fall back to the first network if the named one isn't
    // populated (some test paths run on a single network).
    let ip = obj
        .pointer("/NetworkSettings/Networks/botwork-plugin/IPAddress")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            obj.pointer("/NetworkSettings/Networks")
                .and_then(serde_json::Value::as_object)
                .and_then(|m| m.values().next())
                .and_then(|n| n.get("IPAddress"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })?;

    // Staging token: peel it out of the bind-mount destination if
    // present. Inferring from the mount means the broker doesn't
    // need a new label for it (the staging path is reconstructed
    // from `tenant_name + staging_token` anyway).
    let staging_token = obj
        .pointer("/Mounts")
        .and_then(serde_json::Value::as_array)
        .and_then(|mounts| {
            mounts.iter().find_map(|m| {
                let dest = m.get("Destination").and_then(serde_json::Value::as_str)?;
                if dest != "/workspace" {
                    return None;
                }
                let source = m.get("Source").and_then(serde_json::Value::as_str)?;
                source.rsplit('/').next().map(str::to_string)
            })
        });

    Some(InspectResult {
        container_ip: ip,
        tenant,
        workspace,
        agent_session_label,
        staging_token,
    })
}

/// Single-shot SELECT to resolve `plugin.id` → `plugin.name` during
/// recovery. Not cached because recovery is a one-shot operation.
async fn resolve_plugin_name(db: &DatabaseConnection, plugin_id: uuid::Uuid) -> Option<String> {
    use sea_orm::EntityTrait;
    botwork_entity::plugin::Entity::find_by_id(plugin_id)
        .one(db)
        .await
        .ok()
        .flatten()
        .map(|row| row.name)
}

/// Soft-handle a sessions.json file left from the pre-cutover broker.
///
/// Round-3 PR2 deletes the JSON write path entirely, but a fresh
/// broker pointed at an existing `/var/lib/botwork` directory will
/// see the old file from the previous installation. We log the
/// container names (so an operator can audit them against `docker
/// ps`) and unlink the file. Containers themselves are reaped by
/// the normal `recover_live_workers` orphan-reap pass; this
/// function just gets the operator-visible file out of the way so
/// nothing's confused into thinking it's authoritative.
pub fn migrate_legacy_sessions_json(path: &str) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    log_info(&format!(
        "round-3 cutover: legacy sessions.json detected at {path}; \
         contents will be discarded after a one-time WARN dump below \
         (the new session_worker table is authoritative)"
    ));
    log_info(&format!(
        "round-3 cutover: legacy sessions.json content (verbatim, \
         for operator audit): {content}"
    ));
    if let Err(err) = std::fs::remove_file(path) {
        warn!(
            "[session-broker] round-3 cutover: failed to remove legacy \
             sessions.json at {path}: {err}; manual cleanup required"
        );
    } else {
        log_info(&format!(
            "round-3 cutover: removed legacy sessions.json at {path}"
        ));
    }
}

// Silence the unused-imports lint for the SessionWorkerWriter / Arc /
// LiveWorker symbols we deliberately keep in scope for readability
// even when the cfg gates don't end up using them.
#[allow(dead_code)]
fn _imports_keepalive(_: Arc<SessionWorkerWriter>, _: LiveWorker) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_worker::SessionWorkerWriter;
    use sea_orm::{DatabaseBackend, MockDatabase};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    fn app_state_with_writer(
        writer: Option<Arc<SessionWorkerWriter>>,
        db: Option<Arc<sea_orm::DatabaseConnection>>,
    ) -> AppState {
        AppState {
            transport_sessions: Arc::new(Mutex::new(HashMap::new())),
            pending_init: Arc::new(Mutex::new(HashMap::new())),
            launcher_socket_path: "/tmp/missing.sock".to_string(),
            auth_broker_url: "http://127.0.0.1:1".to_string(),
            config_broker_endpoint: "http://127.0.0.1:1".to_string(),
            control_plane_endpoint: "http://127.0.0.1:1".to_string(),
            tombstones: Arc::new(Mutex::new(HashMap::new())),
            liveness_cache: Arc::new(Mutex::new(HashMap::new())),
            stream_liveness: Arc::new(Mutex::new(HashMap::new())),
            disconnect_grace: std::time::Duration::from_secs(30),
            agent_session_writer: None,
            session_worker_writer: writer,
            db,
        }
    }

    #[test]
    fn migrate_legacy_sessions_json_removes_file() {
        let tmp = tempdir().expect("tempdir");
        let path = tmp.path().join("sessions.json");
        std::fs::write(&path, r#"{"sess":"value"}"#).expect("write");

        migrate_legacy_sessions_json(path.to_str().expect("utf8 path"));

        assert!(!path.exists(), "legacy file should be removed");
    }

    #[test]
    fn migrate_legacy_sessions_json_missing_file_is_noop() {
        let tmp = tempdir().expect("tempdir");
        let path = tmp.path().join("does-not-exist.json");
        migrate_legacy_sessions_json(path.to_str().expect("utf8 path"));
        assert!(!path.exists());
    }

    #[test]
    fn list_running_session_containers_is_total_function() {
        let _ = list_running_session_containers();
    }

    #[test]
    fn inspect_container_for_recovery_missing_container_returns_none() {
        assert!(
            inspect_container_for_recovery("mcp_session_definitely_missing_for_test").is_none()
        );
    }

    #[tokio::test]
    async fn recover_live_workers_noops_when_writer_or_db_missing() {
        let state = app_state_with_writer(None, None);
        recover_live_workers(&state).await;
        assert!(state.transport_sessions.lock().await.is_empty());
    }

    #[test]
    fn force_remove_container_total_function() {
        force_remove_container("mcp_session_definitely_missing_for_test");
    }

    #[tokio::test]
    async fn recover_live_workers_noops_when_docker_unavailable() {
        let writer = Arc::new(SessionWorkerWriter::new(Arc::new(
            MockDatabase::new(DatabaseBackend::Postgres).into_connection(),
        )));
        let db = Arc::new(MockDatabase::new(DatabaseBackend::Postgres).into_connection());
        let state = app_state_with_writer(Some(writer), Some(db));

        recover_live_workers(&state).await;
        assert!(state.transport_sessions.lock().await.is_empty());
    }
}
