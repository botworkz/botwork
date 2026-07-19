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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use bollard::errors::Error as BollardError;
use bollard::models::{ContainerInspectResponse, MountPoint};
use tracing::warn;

use crate::config_broker;
use crate::docker::{connect_docker, DockerApi};
use crate::log_info;
use crate::session_worker::LiveWorker;
use crate::{AppState, TransportState};

/// Returns the set of running `mcp_session_*` container names via the bollard
/// Docker socket API.  Returns `None` when docker is unreachable (transport
/// error / socket not found) so the caller can skip recovery entirely rather
/// than treating an empty set as authoritative and reaping every DB row.
/// Returns `Some(set)` — possibly empty — when docker responds successfully.
async fn list_running_session_containers_impl<D: DockerApi + ?Sized>(
    docker: &D,
) -> Option<HashSet<String>> {
    let mut filters = HashMap::new();
    filters.insert("name".to_string(), vec!["mcp_session_".to_string()]);
    match docker.list_containers(filters).await {
        Ok(summaries) => {
            let names = summaries
                .into_iter()
                .flat_map(|s| s.names.unwrap_or_default())
                .map(|n| n.trim_start_matches('/').to_string())
                .filter(|n| !n.is_empty())
                .collect();
            Some(names)
        }
        Err(e) => {
            warn!(
                "[session-broker] docker list_containers failed during recovery: {e}; \
                 skipping cold-start reconciliation"
            );
            None
        }
    }
}

/// Forces a running container off the host via the bollard Docker socket API.
/// Used to enforce the "live container with no DB row" reap posture during
/// cold start.  Success → log_info; 404 (already gone) → treated as success;
/// any other error → warn (non-fatal, never propagated).
async fn force_remove_container_impl<D: DockerApi + ?Sized>(container_name: &str, docker: &D) {
    match docker.remove_container(container_name).await {
        Ok(()) => {
            log_info(&format!(
                "recovery: reaped orphan container {container_name} (no matching DB row)"
            ));
        }
        Err(BollardError::DockerResponseServerError {
            status_code: 404, ..
        }) => {
            // Already gone — treat as success.
            log_info(&format!(
                "recovery: orphan container {container_name} already gone (404)"
            ));
        }
        Err(e) => {
            warn!("[session-broker] recovery: force-remove of {container_name} failed: {e}");
        }
    }
}

/// Production wrapper: connect to docker and force-remove a container.
/// NOT covered by offline unit tests (`connect_docker` requires the docker socket).
#[cfg(not(tarpaulin_include))]
async fn force_remove_container(container_name: &str) {
    match connect_docker() {
        Err(e) => {
            warn!(
                "[session-broker] recovery: docker socket unavailable for force-remove \
                 {container_name}: {e}"
            );
        }
        Ok(docker) => {
            force_remove_container_impl(container_name, &docker).await;
        }
    }
}

/// Entry point: enumerate live containers, walk `session_worker` rows,
/// reconcile both into `AppState::transport_sessions`.
///
/// Idempotent — re-running has no effect once steady state is reached.
/// Called once at startup from `run()`.
#[cfg(not(tarpaulin_include))]
pub async fn recover_live_workers(state: &AppState) {
    let endpoint = state.config_broker_endpoint.clone();

    // Connect to docker once; if unavailable, pass a None lister so the seam
    // skips recovery immediately (None-vs-Some(empty) contract preserved).
    let docker = match connect_docker() {
        Ok(d) => Arc::new(d),
        Err(e) => {
            warn!(
                "[session-broker] docker socket unavailable during recovery: {e}; \
                 skipping cold-start reconciliation"
            );
            recover_live_workers_with(
                state,
                || None,
                |_| None,
                move |tenant: String, workspace: String, plugin: String| {
                    let ep = endpoint.clone();
                    async move {
                        config_broker::resolve(
                            &ep,
                            &tenant,
                            &workspace,
                            &plugin,
                            Duration::from_secs(5),
                        )
                        .await
                    }
                },
            )
            .await;
            return;
        }
    };

    // Pre-compute the running set asynchronously, then hand it to the sync seam.
    let running = list_running_session_containers_impl(&*docker).await;

    // Pre-compute all inspect results so the inspector closure can be sync.
    let inspect_cache: Arc<HashMap<String, Option<InspectResult>>> = {
        let mut m = HashMap::new();
        if let Some(ref names) = running {
            for name in names {
                m.insert(
                    name.clone(),
                    inspect_container_for_recovery_impl(name, &*docker).await,
                );
            }
        }
        Arc::new(m)
    };

    let running_captured = running.clone();
    let endpoint2 = endpoint.clone();

    recover_live_workers_with(
        state,
        move || running_captured.clone(),
        move |name: &str| inspect_cache.get(name).cloned().flatten(),
        move |tenant: String, workspace: String, plugin: String| {
            let ep = endpoint2.clone();
            async move {
                config_broker::resolve(&ep, &tenant, &workspace, &plugin, Duration::from_secs(5))
                    .await
            }
        },
    )
    .await;
}

/// Testable variant of [`recover_live_workers`] that accepts injectable
/// seams for the three external calls. Production calls the public
/// `recover_live_workers` which wires the real implementations;
/// unit tests inject closures so the reconciliation logic can be exercised
/// without a running docker daemon or config-broker.
///
/// - `lister`: replaces `list_running_session_containers()`.  Returns
///   `None` to signal docker-unavailable (skip recovery); `Some(set)`
///   to provide the set of currently-running `mcp_session_*` names.
/// - `inspector`: replaces `inspect_container_for_recovery()`.  Returns
///   `None` to simulate a failed inspect (container already gone or
///   docker error); `Some(InspectResult)` with the parsed labels + IP.
/// - `resolver`: replaces `config_broker::resolve(...)`.  Receives the
///   plugin's `(tenant, workspace, plugin_name)` as owned `String`s and
///   returns a `Future` with the same `Result` shape as the real call.
pub(crate) async fn recover_live_workers_with<L, I, RF, Fut>(
    state: &AppState,
    lister: L,
    inspector: I,
    resolver: RF,
) where
    L: Fn() -> Option<HashSet<String>>,
    I: Fn(&str) -> Option<InspectResult>,
    RF: Fn(String, String, String) -> Fut,
    Fut: std::future::Future<
        Output = Result<config_broker::PluginDescriptor, config_broker::ConfigBrokerError>,
    >,
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
            force_remove_container(container).await;
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
        let descriptor = match resolver(
            inspect.tenant.clone(),
            inspect.workspace.clone(),
            plugin_name.clone(),
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
#[derive(Clone)]
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

/// Extract the recovery fields from a typed bollard `ContainerInspectResponse`.
///
/// Returns `None` when any required field (tenant, workspace, container IP) is
/// absent — the caller skips that container.
///
/// This replaces the old raw-JSON `parse_inspect_json` function.  The
/// field-extraction semantics are identical:
/// - `tenant` ← `Config.Labels["io.botworkz.tenant"]` (required)
/// - `workspace` ← `Config.Labels["io.botworkz.workspace"]` (required)
/// - `agent_session_label` ← `Config.Labels["io.botworkz.agent_session"]` (optional)
/// - `container_ip` ← `NetworkSettings.Networks["botwork-plugin"].IPAddress`,
///   falling back to the first network's `IPAddress` if absent (required)
/// - `staging_token` ← last path segment of the `Source` for the mount whose
///   `Destination == "/workspace"` (optional)
pub(crate) fn extract_inspect_result(r: &ContainerInspectResponse) -> Option<InspectResult> {
    let labels = r.config.as_ref().and_then(|c| c.labels.as_ref());

    let tenant = labels.and_then(|m| m.get("io.botworkz.tenant")).cloned()?;
    let workspace = labels
        .and_then(|m| m.get("io.botworkz.workspace"))
        .cloned()?;
    let agent_session_label = labels
        .and_then(|m| m.get("io.botworkz.agent_session"))
        .cloned();

    // Container IPv4 on `botwork-plugin`; fall back to the first network.
    let networks = r
        .network_settings
        .as_ref()
        .and_then(|ns| ns.networks.as_ref());
    let ip = networks
        .and_then(|m| m.get("botwork-plugin"))
        .and_then(|ep| ep.ip_address.as_deref())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            networks
                .and_then(|m| m.values().next())
                .and_then(|ep| ep.ip_address.as_deref())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })?;

    // Staging token: last path segment of the `/workspace` bind-mount source.
    let staging_token = r.mounts.as_deref().and_then(|mounts: &[MountPoint]| {
        mounts.iter().find_map(|m| {
            let dest = m.destination.as_deref()?;
            if dest != "/workspace" {
                return None;
            }
            let source = m.source.as_deref()?;
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

/// Inspect a single container via the bollard Docker socket API and extract
/// the recovery fields.  Returns `None` on any error (container gone, docker
/// unreachable, required label missing, etc.).
async fn inspect_container_for_recovery_impl<D: DockerApi + ?Sized>(
    container_name: &str,
    docker: &D,
) -> Option<InspectResult> {
    match docker.inspect_container(container_name).await {
        Ok(response) => extract_inspect_result(&response),
        Err(BollardError::DockerResponseServerError {
            status_code: 404, ..
        }) => {
            log_info(&format!(
                "recovery: container {container_name} gone before inspect; skipping"
            ));
            None
        }
        Err(e) => {
            warn!(
                "[session-broker] recovery: inspect failed for {container_name}: {e}; \
                 skipping rehydration"
            );
            None
        }
    }
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
    use bollard::errors::Error as BollardError;
    use bollard::models::{
        ContainerConfig, ContainerInspectResponse, ContainerSummary, EndpointSettings, MountPoint,
        NetworkSettings,
    };
    use futures_util::future::BoxFuture;
    use futures_util::FutureExt;
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;
    use tokio::sync::Mutex as TokioMutex;
    use uuid::Uuid;

    // ── FakeDocker ────────────────────────────────────────────────────────────
    //
    // Minimal in-memory fake that returns pre-canned responses for the three
    // DockerApi methods used in recovery.  All queues are optional — tests that
    // don't exercise a given method leave its queue empty and the method panics
    // if unexpectedly called (makes bugs obvious).
    type FakeListQueue = Arc<Mutex<VecDeque<Result<Vec<ContainerSummary>, BollardError>>>>;
    type FakeInspectQueue = Arc<Mutex<VecDeque<Result<ContainerInspectResponse, BollardError>>>>;
    type FakeRemoveQueue = Arc<Mutex<VecDeque<Result<(), BollardError>>>>;

    #[derive(Default, Clone)]
    struct FakeDocker {
        list_results: FakeListQueue,
        inspect_results: FakeInspectQueue,
        remove_results: FakeRemoveQueue,
    }

    impl FakeDocker {
        fn with_list(self, results: Vec<Result<Vec<ContainerSummary>, BollardError>>) -> Self {
            *self.list_results.lock().expect("list lock") = VecDeque::from(results);
            self
        }

        fn with_inspect(
            self,
            results: Vec<Result<ContainerInspectResponse, BollardError>>,
        ) -> Self {
            *self.inspect_results.lock().expect("inspect lock") = VecDeque::from(results);
            self
        }

        fn with_remove(self, results: Vec<Result<(), BollardError>>) -> Self {
            *self.remove_results.lock().expect("remove lock") = VecDeque::from(results);
            self
        }
    }

    impl DockerApi for FakeDocker {
        fn inspect_container<'a>(
            &'a self,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<ContainerInspectResponse, BollardError>> {
            async move {
                self.inspect_results
                    .lock()
                    .expect("inspect lock")
                    .pop_front()
                    .expect("unexpected inspect_container call")
            }
            .boxed()
        }

        fn list_containers<'a>(
            &'a self,
            _filters: HashMap<String, Vec<String>>,
        ) -> BoxFuture<'a, Result<Vec<ContainerSummary>, BollardError>> {
            async move {
                self.list_results
                    .lock()
                    .expect("list lock")
                    .pop_front()
                    .expect("unexpected list_containers call")
            }
            .boxed()
        }

        fn remove_container<'a>(
            &'a self,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<(), BollardError>> {
            async move {
                self.remove_results
                    .lock()
                    .expect("remove lock")
                    .pop_front()
                    .expect("unexpected remove_container call")
            }
            .boxed()
        }
    }

    // ── typed fixture helpers ─────────────────────────────────────────────────

    /// Build a minimal `ContainerInspectResponse` with the given labels, network
    /// IP, and optional workspace mount source.
    fn inspect_response(
        labels: HashMap<&str, &str>,
        network_name: &str,
        ip: &str,
        workspace_mount_source: Option<&str>,
    ) -> ContainerInspectResponse {
        let label_map: HashMap<String, String> = labels
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let ep = EndpointSettings {
            ip_address: if ip.is_empty() {
                None
            } else {
                Some(ip.to_string())
            },
            ..Default::default()
        };
        let mut networks = HashMap::new();
        if !network_name.is_empty() {
            networks.insert(network_name.to_string(), ep);
        }

        let mounts = workspace_mount_source.map(|src| {
            vec![MountPoint {
                destination: Some("/workspace".to_string()),
                source: Some(src.to_string()),
                ..Default::default()
            }]
        });

        ContainerInspectResponse {
            config: Some(ContainerConfig {
                labels: Some(label_map),
                ..Default::default()
            }),
            network_settings: Some(NetworkSettings {
                networks: Some(networks),
                ..Default::default()
            }),
            mounts,
            ..Default::default()
        }
    }

    fn required_labels<'a>(tenant: &'a str, workspace: &'a str) -> HashMap<&'a str, &'a str> {
        [
            ("io.botworkz.tenant", tenant),
            ("io.botworkz.workspace", workspace),
        ]
        .into_iter()
        .collect()
    }

    fn container_summary(name: &str) -> ContainerSummary {
        ContainerSummary {
            names: Some(vec![format!("/{name}")]),
            ..Default::default()
        }
    }

    fn err_404() -> BollardError {
        BollardError::DockerResponseServerError {
            status_code: 404,
            message: "No such container".into(),
        }
    }

    fn err_500() -> BollardError {
        BollardError::DockerResponseServerError {
            status_code: 500,
            message: "Internal server error".into(),
        }
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    fn app_state_with_writer(
        writer: Option<Arc<dyn crate::store::SessionWorkerStore>>,
    ) -> AppState {
        AppState {
            transport_sessions: Arc::new(TokioMutex::new(HashMap::new())),
            pending_init: Arc::new(TokioMutex::new(HashMap::new())),
            launcher_socket_path: "/tmp/missing.sock".to_string(),
            auth_broker_url: "http://127.0.0.1:1".to_string(),
            config_broker_endpoint: "http://127.0.0.1:1".to_string(),
            control_plane_endpoint: "http://127.0.0.1:1".to_string(),
            tombstones: Arc::new(TokioMutex::new(HashMap::new())),
            liveness_cache: Arc::new(TokioMutex::new(HashMap::new())),
            stream_liveness: Arc::new(TokioMutex::new(HashMap::new())),
            disconnect_grace: std::time::Duration::from_secs(30),
            cold_start_timeout: crate::COLD_START_TIMEOUT,
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
        let _guard = crate::test_support::log_capture_guard();
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

    // ── list_running_session_containers_impl (FakeDocker) ────────────────────

    #[tokio::test]
    async fn list_running_session_containers_impl_returns_names_stripped_of_slash() {
        let docker = FakeDocker::default().with_list(vec![Ok(vec![
            container_summary("mcp_session_abc"),
            container_summary("mcp_session_def"),
        ])]);
        let result = list_running_session_containers_impl(&docker)
            .await
            .expect("should return Some");
        assert!(result.contains("mcp_session_abc"));
        assert!(result.contains("mcp_session_def"));
        assert_eq!(result.len(), 2);
    }

    #[tokio::test]
    async fn list_running_session_containers_impl_returns_some_empty_on_no_containers() {
        // Docker reachable but no containers — Some(empty), NOT None.
        let docker = FakeDocker::default().with_list(vec![Ok(vec![])]);
        let result = list_running_session_containers_impl(&docker).await;
        assert_eq!(result, Some(HashSet::new()));
    }

    #[tokio::test]
    async fn list_running_session_containers_impl_returns_none_on_transport_error() {
        // Docker unreachable — None (skip recovery, don't reap everything).
        let docker = FakeDocker::default().with_list(vec![Err(err_500())]);
        let result = list_running_session_containers_impl(&docker).await;
        assert!(result.is_none());
    }

    // ── extract_inspect_result (typed, ported from parse_inspect_json) ────────

    #[test]
    fn extract_inspect_result_minimal_object() {
        let r = inspect_response(
            required_labels("acme", "mcp"),
            "botwork-plugin",
            "10.0.0.5",
            None,
        );
        let result = extract_inspect_result(&r).expect("should parse");
        assert_eq!(result.tenant, "acme");
        assert_eq!(result.workspace, "mcp");
        assert_eq!(result.container_ip, "10.0.0.5");
        assert_eq!(result.agent_session_label, None);
        assert_eq!(result.staging_token, None);
    }

    #[test]
    fn extract_inspect_result_fallback_network() {
        // No botwork-plugin network — falls back to the first network found.
        let r = inspect_response(required_labels("t1", "w1"), "bridge", "172.17.0.2", None);
        let result = extract_inspect_result(&r).expect("fallback network");
        assert_eq!(result.container_ip, "172.17.0.2");
    }

    #[test]
    fn extract_inspect_result_missing_tenant_returns_none() {
        let labels: HashMap<&str, &str> = [("io.botworkz.workspace", "mcp")].into_iter().collect();
        let r = inspect_response(labels, "bridge", "1.2.3.4", None);
        assert!(
            extract_inspect_result(&r).is_none(),
            "missing tenant should return None"
        );
    }

    #[test]
    fn extract_inspect_result_missing_workspace_returns_none() {
        let labels: HashMap<&str, &str> = [("io.botworkz.tenant", "acme")].into_iter().collect();
        let r = inspect_response(labels, "bridge", "1.2.3.4", None);
        assert!(
            extract_inspect_result(&r).is_none(),
            "missing workspace should return None"
        );
    }

    #[test]
    fn extract_inspect_result_missing_ip_returns_none() {
        // Empty networks map → no IP anywhere → None.
        let r = ContainerInspectResponse {
            config: Some(ContainerConfig {
                labels: Some(
                    required_labels("acme", "mcp")
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                ),
                ..Default::default()
            }),
            network_settings: Some(NetworkSettings {
                networks: Some(HashMap::new()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(
            extract_inspect_result(&r).is_none(),
            "missing IP should return None"
        );
    }

    #[test]
    fn extract_inspect_result_staging_token_from_workspace_mount() {
        let r = inspect_response(
            required_labels("acme", "mcp"),
            "botwork-plugin",
            "10.0.0.1",
            Some("/var/lib/botwork/tenants/acme/staging/abc123"),
        );
        let result = extract_inspect_result(&r).expect("should parse");
        assert_eq!(result.staging_token, Some("abc123".to_string()));
    }

    #[test]
    fn extract_inspect_result_staging_token_skips_non_workspace_mount() {
        // Mount with Destination != "/workspace" is ignored.
        let r = ContainerInspectResponse {
            config: Some(ContainerConfig {
                labels: Some(
                    required_labels("acme", "mcp")
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                ),
                ..Default::default()
            }),
            network_settings: Some(NetworkSettings {
                networks: Some(
                    [(
                        "botwork-plugin".to_string(),
                        EndpointSettings {
                            ip_address: Some("10.0.0.1".to_string()),
                            ..Default::default()
                        },
                    )]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            }),
            mounts: Some(vec![MountPoint {
                destination: Some("/data".to_string()),
                source: Some("/some/other/path/tok99".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let result = extract_inspect_result(&r).expect("should parse");
        assert_eq!(result.staging_token, None);
    }

    #[test]
    fn extract_inspect_result_agent_session_label_present() {
        let mut labels = required_labels("acme", "mcp");
        labels.insert("io.botworkz.agent_session", "sess-abc");
        let r = inspect_response(labels, "botwork-plugin", "10.0.0.1", None);
        let result = extract_inspect_result(&r).expect("should parse");
        assert_eq!(result.agent_session_label, Some("sess-abc".to_string()));
    }

    #[test]
    fn extract_inspect_result_no_config_returns_none() {
        // ContainerInspectResponse with no Config at all → None (tenant missing).
        let r = ContainerInspectResponse {
            ..Default::default()
        };
        assert!(extract_inspect_result(&r).is_none());
    }

    // ── inspect_container_for_recovery_impl (FakeDocker) ──────────────────────

    #[tokio::test]
    async fn inspect_impl_returns_result_on_success() {
        let response = inspect_response(
            required_labels("acme", "mcp"),
            "botwork-plugin",
            "10.0.0.7",
            None,
        );
        let docker = FakeDocker::default().with_inspect(vec![Ok(response)]);
        let result = inspect_container_for_recovery_impl("mcp_session_abc", &docker).await;
        let r = result.expect("should return Some on success");
        assert_eq!(r.tenant, "acme");
        assert_eq!(r.container_ip, "10.0.0.7");
    }

    #[tokio::test]
    async fn inspect_impl_returns_none_on_404() {
        let docker = FakeDocker::default().with_inspect(vec![Err(err_404())]);
        let result = inspect_container_for_recovery_impl("mcp_session_gone", &docker).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn inspect_impl_returns_none_on_api_error() {
        let docker = FakeDocker::default().with_inspect(vec![Err(err_500())]);
        let result = inspect_container_for_recovery_impl("mcp_session_abc", &docker).await;
        assert!(result.is_none());
    }

    // ── force_remove_container_impl (FakeDocker) ──────────────────────────────

    #[tokio::test]
    // The log-capture guard must be held across the await so all log output from
    // `force_remove_container_impl` is captured.  This mirrors the same pattern
    // used by other log-capture tests in this module.
    #[allow(clippy::await_holding_lock)]
    async fn force_remove_impl_logs_success() {
        let _guard = crate::test_support::log_capture_guard();
        crate::test_support::start_log_capture();
        let docker = FakeDocker::default().with_remove(vec![Ok(())]);
        force_remove_container_impl("mcp_session_orphan", &docker).await;
        let logs = crate::test_support::take_log_capture().join("\n");
        assert!(
            logs.contains("reaped orphan container"),
            "expected success log; got: {logs}"
        );
    }

    #[tokio::test]
    // See note on force_remove_impl_logs_success.
    #[allow(clippy::await_holding_lock)]
    async fn force_remove_impl_treats_404_as_success() {
        let _guard = crate::test_support::log_capture_guard();
        crate::test_support::start_log_capture();
        let docker = FakeDocker::default().with_remove(vec![Err(err_404())]);
        force_remove_container_impl("mcp_session_gone", &docker).await;
        let logs = crate::test_support::take_log_capture().join("\n");
        assert!(
            logs.contains("already gone"),
            "expected 404-gone log; got: {logs}"
        );
    }

    #[tokio::test]
    async fn force_remove_impl_warns_on_other_error_non_fatal() {
        // A non-404 error should warn (non-fatal) and not panic.
        let docker = FakeDocker::default().with_remove(vec![Err(err_500())]);
        // This must complete without panicking.
        force_remove_container_impl("mcp_session_err", &docker).await;
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

    /// Resolver that always returns a config-broker error. Used in tests whose
    /// code paths should never reach the resolver — if they do, the error still
    /// keeps assertions clean (no session seeded) while making it obvious that
    /// the resolver ran unexpectedly.
    fn err_resolver(
        _t: String,
        _w: String,
        _p: String,
    ) -> std::future::Ready<Result<config_broker::PluginDescriptor, config_broker::ConfigBrokerError>>
    {
        std::future::ready(Err(config_broker::ConfigBrokerError::Transport(
            "unreachable-in-test".to_string(),
        )))
    }

    #[tokio::test]
    async fn recover_with_seam_noops_when_lister_returns_none() {
        let writer: Arc<dyn crate::store::SessionWorkerStore> =
            Arc::new(MockSessionWorkerStore::new());
        let state = app_state_with_writer(Some(writer));

        recover_live_workers_with(&state, || None, |_| None, err_resolver).await;
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
            err_resolver,
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
            err_resolver,
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
        recover_live_workers_with(&state, || Some(HashSet::new()), |_| None, err_resolver).await;
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
            err_resolver,
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
            err_resolver,
        )
        .await;
        assert!(state.transport_sessions.lock().await.is_empty());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
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
        let _guard = crate::test_support::log_capture_guard();
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
            err_resolver,
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
    #[allow(clippy::await_holding_lock)]
    async fn recover_with_seam_logs_ip_drift() {
        // IP in DB row differs from inspect result → log the drift, then
        // resolver returns an error so the intersection is skipped.
        let pid = Uuid::new_v4();
        let state = app_state_with_writer(Some(Arc::new(
            MockSessionWorkerStore::new()
                .with_plugin(pid, "mcp-bash")
                .with_live_worker("mcp_session_drift", "10.0.0.99", "sess-drift", pid),
        )));
        let _guard = crate::test_support::log_capture_guard();
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
            err_resolver,
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
        // resolver returns an error → warn + skip, nothing seeded.
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
            err_resolver,
        )
        .await;
        assert!(state.transport_sessions.lock().await.is_empty());
    }

    // ── new: previously-uncovered paths ──────────────────────────────────────

    #[tokio::test]
    async fn recover_with_seam_record_reap_error_is_non_fatal() {
        // DB has a live row whose record_reap call returns an error.
        // Recovery should warn and continue — transport_sessions stays empty,
        // the function does not panic or return early.
        let pid = Uuid::new_v4();
        let writer = MockSessionWorkerStore::new()
            .with_live_worker("mcp_session_reap_err", "10.0.0.1", "sess-r", pid)
            .with_record_reap_error("mcp_session_reap_err", "db write fail");
        let state = app_state_with_writer(Some(Arc::new(writer)));

        // Running set is empty → every DB row triggers record_reap.
        recover_live_workers_with(&state, || Some(HashSet::new()), |_| None, err_resolver).await;
        // Even though record_reap errored, the function completes normally.
        assert!(state.transport_sessions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn recover_with_seam_resolve_plugin_name_error_skips_row() {
        // Container in running set + matching DB row, but resolve_plugin_name
        // returns a DB error → warn + skip, no session seeded.
        let pid = Uuid::new_v4();
        let state = app_state_with_writer(Some(Arc::new(
            MockSessionWorkerStore::new()
                .with_live_worker("mcp_session_pe", "10.0.0.2", "sess-pe", pid)
                .with_resolve_plugin_name_error("plugin lookup broke"),
        )));

        recover_live_workers_with(
            &state,
            || Some(HashSet::from(["mcp_session_pe".to_string()])),
            |_| None,
            err_resolver,
        )
        .await;
        assert!(state.transport_sessions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn recover_with_seam_full_success_seeds_transport_session() {
        // Full happy-path: container running + DB row + plugin resolved +
        // inspect succeeds + resolver returns Ok(descriptor) → session seeded
        // into transport_sessions.
        let pid = Uuid::new_v4();
        let state = app_state_with_writer(Some(Arc::new(
            MockSessionWorkerStore::new()
                .with_plugin(pid, "mcp-fetch")
                .with_live_worker("mcp_session_ok", "10.0.0.10", "sess-ok", pid),
        )));

        let ok_descriptor = config_broker::PluginDescriptor {
            image: "ghcr.io/example/mcp-fetch:latest".to_string(),
            port: 8080,
            path: "/mcp".to_string(),
            upstream_auth: config_broker::UpstreamAuth::None,
            resources: Default::default(),
            env: vec![],
            config_blob: None,
            egress: None,
        };

        recover_live_workers_with(
            &state,
            || Some(HashSet::from(["mcp_session_ok".to_string()])),
            |_| {
                Some(InspectResult {
                    container_ip: "10.0.0.10".to_string(),
                    tenant: "acme".to_string(),
                    workspace: "mcp".to_string(),
                    agent_session_label: Some("agent-abc".to_string()),
                    staging_token: Some("staging-tok".to_string()),
                })
            },
            {
                let desc = ok_descriptor.clone();
                move |_t: String, _w: String, _p: String| {
                    let d = desc.clone();
                    async move { Ok(d) }
                }
            },
        )
        .await;

        let sessions = state.transport_sessions.lock().await;
        assert_eq!(sessions.len(), 1, "one session should be seeded");
        let transport = sessions.get("sess-ok").expect("keyed by mcp_session_id");
        assert_eq!(transport.container_name, "mcp_session_ok");
        assert_eq!(transport.container_ip, "10.0.0.10");
        assert_eq!(transport.tenant_name, "acme");
        assert_eq!(transport.workspace, "mcp");
        assert_eq!(transport.plugin_name, "mcp-fetch");
        assert_eq!(transport.port, 8080);
        assert_eq!(transport.path, "/mcp");
        assert_eq!(transport.staging_token, "staging-tok");
        assert_eq!(transport.agent_id, Some("agent-abc".to_string()));
        assert!(transport.upstream_authorization.is_none());
    }

    #[tokio::test]
    async fn recover_with_seam_full_success_ip_drift_uses_inspect_ip() {
        // DB row has a stale IP; inspect returns a different one.
        // The seeded TransportState must use the inspect IP (the live address).
        let pid = Uuid::new_v4();
        let state = app_state_with_writer(Some(Arc::new(
            MockSessionWorkerStore::new()
                .with_plugin(pid, "mcp-fetch")
                // DB row has "10.0.0.99" (stale)
                .with_live_worker("mcp_session_drift2", "10.0.0.99", "sess-drift2", pid),
        )));

        let ok_descriptor = config_broker::PluginDescriptor {
            image: "ghcr.io/example/mcp-fetch:latest".to_string(),
            port: 9090,
            path: "/rpc".to_string(),
            upstream_auth: config_broker::UpstreamAuth::None,
            resources: Default::default(),
            env: vec![],
            config_blob: None,
            egress: Some(serde_json::json!({"allow": []})),
        };

        recover_live_workers_with(
            &state,
            || Some(HashSet::from(["mcp_session_drift2".to_string()])),
            |_| {
                Some(InspectResult {
                    container_ip: "10.0.0.77".to_string(), // live IP differs
                    tenant: "org".to_string(),
                    workspace: "ws1".to_string(),
                    agent_session_label: None,
                    staging_token: None,
                })
            },
            {
                let desc = ok_descriptor.clone();
                move |_t: String, _w: String, _p: String| {
                    let d = desc.clone();
                    async move { Ok(d) }
                }
            },
        )
        .await;

        let sessions = state.transport_sessions.lock().await;
        let transport = sessions
            .get("sess-drift2")
            .expect("keyed by mcp_session_id");
        // Must use the inspect (live) IP, not the DB row IP.
        assert_eq!(transport.container_ip, "10.0.0.77");
        assert_eq!(transport.port, 9090);
        assert_eq!(
            transport.egress_policy,
            Some(serde_json::json!({"allow": []}))
        );
    }
}
