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

use tracing::warn;

use crate::config_broker;
use crate::log_info;
use crate::session_worker::LiveWorker;
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
    recover_live_workers_with(
        state,
        list_running_session_containers,
        inspect_container_for_recovery,
    )
    .await;
}

/// Testable variant of [`recover_live_workers`] that accepts injectable
/// seams for the two docker CLI calls.  Production calls the public
/// `recover_live_workers` which wires the real implementations;
/// unit tests inject closures that return synthetic container sets /
/// inspect results so the reconciliation logic can be exercised
/// without a running docker daemon.
///
/// - `lister`: replaces `list_running_session_containers()`.  Returns
///   `None` to signal docker-unavailable (skip recovery); `Some(set)`
///   to provide the set of currently-running `mcp_session_*` names.
/// - `inspector`: replaces `inspect_container_for_recovery()`.  Returns
///   `None` to simulate a failed inspect (container already gone or
///   docker error); `Some(InspectResult)` with the parsed labels + IP.
pub(crate) async fn recover_live_workers_with<L, I>(state: &AppState, lister: L, inspector: I)
where
    L: Fn() -> Option<HashSet<String>>,
    I: Fn(&str) -> Option<InspectResult>,
{
    // Production always wires both. Tests use `None` for either; in
    // both cases we have nothing to do here. (Pre-PR2 tests + the
    // ext_proc_test suite intentionally hand AppState without DB.)
    let Some(writer) = state.session_worker_writer.as_ref() else {
        log_info("recovery: session_worker_writer not configured; skipping");
        return;
    };

    let running = match lister() {
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
            if let Err(err) = writer.record_reap(&row.container_name).await {
                warn!(
                    "[session-broker] recovery: record_reap failed for container={}: {err}",
                    row.container_name
                );
            }
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
        let plugin_name = match writer.resolve_plugin_name(row.plugin_id).await {
            Ok(Some(name)) => name,
            Ok(None) => {
                warn!(
                    "[session-broker] recovery: cannot resolve plugin name for \
                     container={container} plugin_id={plugin_id}; row will be \
                     reaped on next cycle once container exits",
                    plugin_id = row.plugin_id
                );
                continue;
            }
            Err(err) => {
                warn!(
                    "[session-broker] recovery: plugin-name lookup failed for \
                     container={container} plugin_id={plugin_id}: {err}",
                    plugin_id = row.plugin_id
                );
                continue;
            }
        };

        // Tenant + workspace come from the labels stamped at spawn
        // time (#115). `docker inspect` gives us those plus the
        // container's IPv4 on the plugin network.
        let Some(inspect) = inspector(container) else {
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

    let _ = writer;
}

/// Result of `docker inspect <container>` filtered down to the bits
/// we use during recovery.
pub(crate) struct InspectResult {
    pub(crate) container_ip: String,
    pub(crate) tenant: String,
    pub(crate) workspace: String,
    /// The agent-session-id label from #115, if present. NOT stamped
    /// at spawn (labels can't be added after `docker run`), so this
    /// is always None in PR2's shape. Kept here so a future iteration
    /// that adds the label via `docker container update --label-add`
    /// (or via a sidecar metadata channel) can backfill the field.
    pub(crate) agent_session_label: Option<String>,
    /// Inferred from the workspace mount path. NULL if the container
    /// was started without `--workspace`.
    pub(crate) staging_token: Option<String>,
}

/// Parse a `docker inspect --format {{json .}}` JSON blob into the fields
/// recovery needs.  Extracted from `inspect_container_for_recovery` so the
/// JSON-parsing branches can be exercised in unit tests without spawning a
/// real docker process.
pub(crate) fn parse_inspect_json(raw: &serde_json::Value) -> Option<InspectResult> {
    // `inspect` returns an array of one object for a single arg, but
    // with `--format {{json .}}` we get just the object — keep
    // future-flexibility for either shape.
    let obj: &serde_json::Value = match raw {
        serde_json::Value::Array(arr) if !arr.is_empty() => &arr[0],
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

fn inspect_container_for_recovery(container_name: &str) -> Option<InspectResult> {
    let output = std::process::Command::new("docker")
        .args(["inspect", "--format", "{{json .}}", container_name])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    parse_inspect_json(&raw)
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

// Silence the unused-imports lint for the SessionWorkerStore / Arc /
// LiveWorker symbols we deliberately keep in scope for readability
// even when the cfg gates don't end up using them.
#[allow(dead_code)]
fn _imports_keepalive(_: Arc<dyn crate::store::SessionWorkerStore>, _: LiveWorker) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::mock::MockSessionWorkerStore;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn app_state_with_writer(
        writer: Option<Arc<dyn crate::store::SessionWorkerStore>>,
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
            db: None,
        }
    }

    fn synthetic_inspect(ip: &str, tenant: &str, workspace: &str) -> InspectResult {
        InspectResult {
            container_ip: ip.to_string(),
            tenant: tenant.to_string(),
            workspace: workspace.to_string(),
            agent_session_label: None,
            staging_token: Some("tok1".to_string()),
        }
    }

    // ── migrate_legacy_sessions_json ─────────────────────────────────────────

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
    fn migrate_legacy_sessions_json_logs_content_and_removes() {
        let tmp = tempdir().expect("tempdir");
        let path = tmp.path().join("sessions.json");
        let content = r#"{"container":"mcp_session_abc"}"#;
        std::fs::write(&path, content).expect("write");
        crate::test_support::start_log_capture();
        migrate_legacy_sessions_json(path.to_str().expect("utf8 path"));
        let logs = crate::test_support::take_log_capture().join("\n");
        assert!(!path.exists(), "file should be unlinked");
        assert!(
            logs.contains("legacy sessions.json"),
            "should log detection"
        );
        assert!(logs.contains("removed"), "should log removal");
    }

    // ── list_running_session_containers ──────────────────────────────────────

    #[test]
    fn list_running_session_containers_is_total_function() {
        if let Some(names) = list_running_session_containers() {
            assert!(
                names.iter().all(|name| name.starts_with("mcp_session_")),
                "docker filter should only return mcp_session_* containers"
            );
        }
    }

    // ── inspect_container_for_recovery (real docker call — minimal) ──────────

    #[test]
    fn inspect_container_for_recovery_missing_container_returns_none() {
        assert!(
            inspect_container_for_recovery("mcp_session_definitely_missing_for_test").is_none()
        );
    }

    // ── parse_inspect_json ───────────────────────────────────────────────────

    #[test]
    fn parse_inspect_json_minimal_object() {
        let json = serde_json::json!({
            "Config": {
                "Labels": {
                    "io.botworkz.tenant": "acme",
                    "io.botworkz.workspace": "mcp"
                }
            },
            "NetworkSettings": {
                "Networks": {
                    "botwork-plugin": {
                        "IPAddress": "10.0.0.5"
                    }
                }
            },
            "Mounts": []
        });
        let result = parse_inspect_json(&json).expect("should parse");
        assert_eq!(result.tenant, "acme");
        assert_eq!(result.workspace, "mcp");
        assert_eq!(result.container_ip, "10.0.0.5");
        assert_eq!(result.agent_session_label, None);
        assert_eq!(result.staging_token, None);
    }

    #[test]
    fn parse_inspect_json_array_shape() {
        let json = serde_json::json!([{
            "Config": {
                "Labels": {
                    "io.botworkz.tenant": "t1",
                    "io.botworkz.workspace": "w1"
                }
            },
            "NetworkSettings": {
                "Networks": {
                    "botwork-plugin": { "IPAddress": "10.1.2.3" }
                }
            },
            "Mounts": []
        }]);
        let result = parse_inspect_json(&json).expect("array shape should parse");
        assert_eq!(result.tenant, "t1");
        assert_eq!(result.container_ip, "10.1.2.3");
    }

    #[test]
    fn parse_inspect_json_fallback_network() {
        // No botwork-plugin network — falls back to the first network found.
        let json = serde_json::json!({
            "Config": {
                "Labels": {
                    "io.botworkz.tenant": "t1",
                    "io.botworkz.workspace": "w1"
                }
            },
            "NetworkSettings": {
                "Networks": {
                    "bridge": { "IPAddress": "172.17.0.2" }
                }
            },
            "Mounts": []
        });
        let result = parse_inspect_json(&json).expect("fallback network");
        assert_eq!(result.container_ip, "172.17.0.2");
    }

    #[test]
    fn parse_inspect_json_missing_tenant_returns_none() {
        let json = serde_json::json!({
            "Config": {
                "Labels": {
                    "io.botworkz.workspace": "mcp"
                }
            },
            "NetworkSettings": {
                "Networks": { "bridge": { "IPAddress": "1.2.3.4" } }
            },
            "Mounts": []
        });
        assert!(
            parse_inspect_json(&json).is_none(),
            "missing tenant should return None"
        );
    }

    #[test]
    fn parse_inspect_json_missing_workspace_returns_none() {
        let json = serde_json::json!({
            "Config": {
                "Labels": {
                    "io.botworkz.tenant": "acme"
                }
            },
            "NetworkSettings": {
                "Networks": { "bridge": { "IPAddress": "1.2.3.4" } }
            },
            "Mounts": []
        });
        assert!(
            parse_inspect_json(&json).is_none(),
            "missing workspace should return None"
        );
    }

    #[test]
    fn parse_inspect_json_missing_ip_returns_none() {
        let json = serde_json::json!({
            "Config": {
                "Labels": {
                    "io.botworkz.tenant": "acme",
                    "io.botworkz.workspace": "mcp"
                }
            },
            "NetworkSettings": {
                "Networks": {}
            },
            "Mounts": []
        });
        assert!(
            parse_inspect_json(&json).is_none(),
            "missing IP should return None"
        );
    }

    #[test]
    fn parse_inspect_json_staging_token_from_workspace_mount() {
        let json = serde_json::json!({
            "Config": {
                "Labels": {
                    "io.botworkz.tenant": "acme",
                    "io.botworkz.workspace": "mcp"
                }
            },
            "NetworkSettings": {
                "Networks": {
                    "botwork-plugin": { "IPAddress": "10.0.0.1" }
                }
            },
            "Mounts": [
                {
                    "Source": "/var/lib/botwork/tenants/acme/staging/abc123",
                    "Destination": "/workspace"
                }
            ]
        });
        let result = parse_inspect_json(&json).expect("should parse");
        assert_eq!(result.staging_token, Some("abc123".to_string()));
    }

    #[test]
    fn parse_inspect_json_staging_token_skips_non_workspace_mount() {
        let json = serde_json::json!({
            "Config": {
                "Labels": {
                    "io.botworkz.tenant": "acme",
                    "io.botworkz.workspace": "mcp"
                }
            },
            "NetworkSettings": {
                "Networks": {
                    "botwork-plugin": { "IPAddress": "10.0.0.1" }
                }
            },
            "Mounts": [
                {
                    "Source": "/some/other/path/tok99",
                    "Destination": "/data"
                }
            ]
        });
        let result = parse_inspect_json(&json).expect("should parse");
        assert_eq!(result.staging_token, None);
    }

    #[test]
    fn parse_inspect_json_agent_session_label_present() {
        let json = serde_json::json!({
            "Config": {
                "Labels": {
                    "io.botworkz.tenant": "acme",
                    "io.botworkz.workspace": "mcp",
                    "io.botworkz.agent_session": "sess-abc"
                }
            },
            "NetworkSettings": {
                "Networks": {
                    "botwork-plugin": { "IPAddress": "10.0.0.1" }
                }
            },
            "Mounts": []
        });
        let result = parse_inspect_json(&json).expect("should parse");
        assert_eq!(result.agent_session_label, Some("sess-abc".to_string()));
    }

    #[test]
    fn parse_inspect_json_empty_array_returns_none() {
        // Empty array: no object to work with — tenant/workspace will
        // be missing, so None is returned.
        let json = serde_json::json!([]);
        assert!(parse_inspect_json(&json).is_none());
    }

    // ── force_remove_container ────────────────────────────────────────────────

    #[test]
    fn force_remove_container_total_function() {
        let before = list_running_session_containers();
        force_remove_container("mcp_session_definitely_missing_for_test");
        let after = list_running_session_containers();
        assert_eq!(before.is_some(), after.is_some());
    }

    // ── recover_live_workers (real entry point) ───────────────────────────────

    #[tokio::test]
    async fn recover_live_workers_noops_when_writer_or_db_missing() {
        let state = app_state_with_writer(None);
        recover_live_workers(&state).await;
        assert!(state.transport_sessions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn recover_live_workers_noops_when_docker_unavailable() {
        let writer: Arc<dyn crate::store::SessionWorkerStore> =
            Arc::new(MockSessionWorkerStore::new());
        let state = app_state_with_writer(Some(writer));

        recover_live_workers(&state).await;
        assert!(state.transport_sessions.lock().await.is_empty());
    }

    // ── recover_live_workers_with (seam-driven tests) ────────────────────────

    #[tokio::test]
    async fn recover_with_seam_noops_when_lister_returns_none() {
        let writer: Arc<dyn crate::store::SessionWorkerStore> =
            Arc::new(MockSessionWorkerStore::new());
        let state = app_state_with_writer(Some(writer));

        recover_live_workers_with(&state, || None, |_| None).await;
        assert!(state.transport_sessions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn recover_with_seam_skips_on_db_error() {
        // list_live() returns a DB error → skip reconciliation.
        let writer: Arc<dyn crate::store::SessionWorkerStore> =
            Arc::new(MockSessionWorkerStore::always_error("db-fail"));
        let state = app_state_with_writer(Some(writer));

        recover_live_workers_with(
            &state,
            || Some(HashSet::from(["mcp_session_abc".to_string()])),
            |_| None,
        )
        .await;
        assert!(state.transport_sessions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn recover_with_seam_reaps_orphan_containers() {
        // Running container "mcp_session_orphan" has no DB row → should be force-removed.
        // We can verify the call path runs without panicking (force_remove_container
        // calls docker which isn't available — it will warn, not crash).
        let writer: Arc<dyn crate::store::SessionWorkerStore> =
            Arc::new(MockSessionWorkerStore::new());
        let state = app_state_with_writer(Some(writer));

        recover_live_workers_with(
            &state,
            || Some(HashSet::from(["mcp_session_orphan".to_string()])),
            |_| None,
        )
        .await;
        // Nothing seeded; the orphan container gets force-removed (docker
        // call is a no-op here because that container does not exist).
        assert!(state.transport_sessions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn recover_with_seam_marks_db_rows_reaped_when_container_absent() {
        // DB has one live row for "mcp_session_gone" but docker set is empty
        // → record_reap should be called.  MockDatabase records the UPDATE.
        let pid = Uuid::new_v4();
        let writer = MockSessionWorkerStore::new().with_live_worker(
            "mcp_session_gone",
            "10.0.0.1",
            "sess-1",
            pid,
        );
        let state = app_state_with_writer(Some(Arc::new(writer.clone())));

        // Empty running set → every DB row should be marked reaped.
        recover_live_workers_with(&state, || Some(HashSet::new()), |_| None).await;
        assert!(state.transport_sessions.lock().await.is_empty());
        assert_eq!(
            writer.drain_recorded_reaps().await,
            vec!["mcp_session_gone".to_string()]
        );
    }

    #[tokio::test]
    async fn recover_with_seam_skips_intersection_when_plugin_not_resolved() {
        // Container in running set + matching DB row, but plugin lookup fails
        // (DB returns empty) → warn + skip, no session seeded.
        let pid = Uuid::new_v4();
        let state = app_state_with_writer(Some(Arc::new(
            MockSessionWorkerStore::new().with_live_worker(
                "mcp_session_x",
                "10.0.0.2",
                "sess-x",
                pid,
            ),
        )));

        recover_live_workers_with(
            &state,
            || Some(HashSet::from(["mcp_session_x".to_string()])),
            |_| None,
        )
        .await;
        assert!(state.transport_sessions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn recover_with_seam_skips_intersection_when_inspect_fails() {
        // Container in running + DB row present + plugin resolved, but
        // inspector returns None (docker inspect failed) → warn + skip.
        let pid = Uuid::new_v4();
        let state = app_state_with_writer(Some(Arc::new(
            MockSessionWorkerStore::new()
                .with_plugin(pid, "mcp-bash")
                .with_live_worker("mcp_session_y", "10.0.0.3", "sess-y", pid),
        )));

        recover_live_workers_with(
            &state,
            || Some(HashSet::from(["mcp_session_y".to_string()])),
            |_| None, // inspector returns None
        )
        .await;
        assert!(state.transport_sessions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn recover_with_seam_skips_empty_mcp_session_id() {
        // Container present + DB row + plugin resolved + inspect succeeds,
        // but mcp_session_id is empty (spawn-to-init-response window) →
        // skip, no session seeded.
        let pid = Uuid::new_v4();
        let state = app_state_with_writer(Some(Arc::new(
            MockSessionWorkerStore::new()
                .with_plugin(pid, "mcp-bash")
                .with_live_worker("mcp_session_z", "10.0.0.4", "", pid),
        )));
        crate::test_support::start_log_capture();

        recover_live_workers_with(
            &state,
            || Some(HashSet::from(["mcp_session_z".to_string()])),
            |_| {
                Some(InspectResult {
                    container_ip: "10.0.0.4".to_string(),
                    tenant: "acme".to_string(),
                    workspace: "mcp".to_string(),
                    agent_session_label: None,
                    staging_token: None,
                })
            },
        )
        .await;
        let logs = crate::test_support::take_log_capture().join("\n");
        assert!(state.transport_sessions.lock().await.is_empty());
        assert!(
            logs.contains("has no mcp_session_id yet"),
            "missing skip log: {logs}"
        );
    }

    #[tokio::test]
    async fn recover_with_seam_logs_ip_drift() {
        // IP in DB row differs from inspect result → log the drift.
        let pid = Uuid::new_v4();
        // config_broker_endpoint is unreachable (port 1), so config-broker
        // call will fail → the intersection is skipped after the IP-drift log.
        let state = app_state_with_writer(Some(Arc::new(
            MockSessionWorkerStore::new()
                .with_plugin(pid, "mcp-bash")
                .with_live_worker("mcp_session_drift", "10.0.0.99", "sess-drift", pid),
        )));
        crate::test_support::start_log_capture();

        recover_live_workers_with(
            &state,
            || Some(HashSet::from(["mcp_session_drift".to_string()])),
            |_| {
                Some(InspectResult {
                    container_ip: "10.0.0.50".to_string(), // different from row
                    tenant: "acme".to_string(),
                    workspace: "mcp".to_string(),
                    agent_session_label: None,
                    staging_token: None,
                })
            },
        )
        .await;
        let logs = crate::test_support::take_log_capture().join("\n");
        assert!(
            logs.contains("container_ip drift"),
            "expected IP-drift log, got: {logs}"
        );
    }

    #[tokio::test]
    async fn recover_with_seam_skips_when_config_broker_fails() {
        // Full intersection (container + row + plugin + inspect), but
        // config-broker is unreachable (port 1 as set in app_state_with_writer)
        // → config-broker resolve fails, warn + skip.
        let pid = Uuid::new_v4();
        let state = app_state_with_writer(Some(Arc::new(
            MockSessionWorkerStore::new()
                .with_plugin(pid, "mcp-bash")
                .with_live_worker("mcp_session_cb_fail", "10.0.0.5", "sess-cb", pid),
        )));

        recover_live_workers_with(
            &state,
            || Some(HashSet::from(["mcp_session_cb_fail".to_string()])),
            |_| Some(synthetic_inspect("10.0.0.5", "acme", "mcp")),
        )
        .await;
        // config-broker at 127.0.0.1:1 refuses connection → skip; nothing seeded.
        assert!(state.transport_sessions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn recover_with_seam_skips_when_record_reap_errors() {
        // DB row present, container absent → record_reap is called. When
        // record_reap returns an error the function should log and continue,
        // not panic.
        let pid = Uuid::new_v4();
        let writer = MockSessionWorkerStore::new()
            .with_live_worker("mcp_session_reap_fail", "10.0.0.1", "sess-rf", pid)
            .fail_on_reap();
        let state = app_state_with_writer(Some(Arc::new(writer)));
        crate::test_support::start_log_capture();

        recover_live_workers_with(&state, || Some(HashSet::new()), |_| None).await;
        let logs = crate::test_support::take_log_capture().join("\n");
        // record_reap failed but state is still consistent and no panic occurred.
        assert!(state.transport_sessions.lock().await.is_empty());
        assert!(
            logs.contains("record_reap failed") || logs.contains("reap"),
            "expected a reap-related log, got: {logs}"
        );
    }

    #[tokio::test]
    async fn recover_with_seam_skips_when_resolve_plugin_name_errors() {
        // Container + DB row + inspect succeeds, but resolve_plugin_name
        // returns Err → warn (via tracing, not log_info) + skip; nothing seeded.
        let pid = Uuid::new_v4();
        let writer = MockSessionWorkerStore::new()
            .with_live_worker("mcp_session_rpe", "10.0.0.2", "sess-rpe", pid)
            .fail_on_resolve_plugin_name();
        let state = app_state_with_writer(Some(Arc::new(writer)));

        recover_live_workers_with(
            &state,
            || Some(HashSet::from(["mcp_session_rpe".to_string()])),
            |_| Some(synthetic_inspect("10.0.0.2", "acme", "mcp")),
        )
        .await;
        // The Err arm should have skipped seeding; no session in the map.
        assert!(
            state.transport_sessions.lock().await.is_empty(),
            "expected no sessions seeded when resolve_plugin_name errors"
        );
    }

    #[tokio::test]
    async fn recover_with_seam_seeds_transport_when_config_broker_responds() {
        // Full happy path: container running + DB row + plugin resolved +
        // inspect succeeds + config-broker returns a valid descriptor.
        // Spin up a minimal TCP server that serves a one-shot descriptor.
        use tokio::io::AsyncWriteExt;
        let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = tcp_listener.local_addr().unwrap().port();
        let plugin_port: u16 = 8000;
        let descriptor_json = format!(
            r#"{{"image":"img:1","port":{plugin_port},"path":"/mcp","upstream_auth":"none"}}"#
        );
        let body_len = descriptor_json.len();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n{descriptor_json}"
        );
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = tcp_listener.accept().await {
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        let pid = Uuid::new_v4();
        let writer = MockSessionWorkerStore::new()
            .with_plugin(pid, "mcp-bash")
            .with_live_worker("mcp_session_seed", "10.0.0.3", "sess-seed", pid);
        let mut state = app_state_with_writer(Some(Arc::new(writer)));
        state.config_broker_endpoint = format!("http://127.0.0.1:{port}");

        recover_live_workers_with(
            &state,
            || Some(HashSet::from(["mcp_session_seed".to_string()])),
            |_| {
                Some(InspectResult {
                    container_ip: "10.0.0.3".to_string(),
                    tenant: "acme".to_string(),
                    workspace: "mcp".to_string(),
                    agent_session_label: None,
                    staging_token: Some("tok-seed".to_string()),
                })
            },
        )
        .await;

        let sessions = state.transport_sessions.lock().await;
        assert!(
            sessions.contains_key("sess-seed"),
            "transport should be seeded for mcp_session_id=sess-seed"
        );
        let t = sessions.get("sess-seed").unwrap();
        assert_eq!(t.container_name, "mcp_session_seed");
        assert_eq!(t.container_ip, "10.0.0.3");
        assert_eq!(t.tenant_name, "acme");
        assert_eq!(t.workspace, "mcp");
        assert_eq!(t.port, plugin_port);
    }
}
