use std::collections::HashMap;
use std::fs;
use std::pin::Pin;

use bollard::errors::Error as BollardError;
use bollard::models::{
    ContainerCreateBody, ContainerInspectResponse, ContainerStateStatusEnum, EndpointSettings,
    EventMessage, HostConfig, Mount, MountBindOptions, MountBindOptionsPropagationEnum,
    MountTypeEnum, NetworkingConfig,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, EventsOptions, InspectContainerOptionsBuilder,
    RemoveContainerOptionsBuilder, UploadToContainerOptionsBuilder,
};
use bollard::Docker;
use bytes::Bytes;
use futures_util::future::{BoxFuture, FutureExt};
use futures_util::Stream;

use crate::cmd::{log_info, run_command, CommandOutput};
use crate::error::LauncherError;
use crate::mount::{is_not_mounted_or_einval, setup_staging_dir};
use crate::validate::{valid_env_name, Validators};

/// The container-local tmpfs path where secrets are delivered as files.
///
/// - Mode `0500` on the directory: the plugin uid can traverse and read but not write.
/// - Each file is mode `0400`, owned by `plugin_uid:plugin_gid`.
/// - The tmpfs is RAM-backed and destroyed with the container (`auto_remove: true`).
///   No host-side cleanup is needed for secrets.
pub(crate) const SECRETS_TMPFS_PATH: &str = "/run/botwork-secrets";

pub(crate) type DockerEventStream =
    Pin<Box<dyn Stream<Item = Result<EventMessage, BollardError>> + Send>>;

pub(crate) trait DockerApi {
    fn inspect_container<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<ContainerInspectResponse, BollardError>>;

    fn create_container<'a>(
        &'a self,
        name: &'a str,
        config: ContainerCreateBody,
    ) -> BoxFuture<'a, Result<(), BollardError>>;

    fn start_container<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), BollardError>>;

    fn remove_container_force<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<(), BollardError>>;

    /// Upload a tar archive into a running or stopped container at the given path.
    ///
    /// Used to populate the secrets tmpfs after `create_container` and before
    /// `start_container`.  The `tar` bytes must be a valid POSIX tar archive; the
    /// Docker daemon extracts it into `path` inside the container.
    fn upload_to_container<'a>(
        &'a self,
        name: &'a str,
        path: &'a str,
        tar: Vec<u8>,
    ) -> BoxFuture<'a, Result<(), BollardError>>;

    fn events(&self, options: Option<EventsOptions>) -> DockerEventStream;
}

impl DockerApi for Docker {
    fn inspect_container<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<ContainerInspectResponse, BollardError>> {
        let options = Some(InspectContainerOptionsBuilder::new().size(false).build());
        Docker::inspect_container(self, name, options).boxed()
    }

    fn create_container<'a>(
        &'a self,
        name: &'a str,
        config: ContainerCreateBody,
    ) -> BoxFuture<'a, Result<(), BollardError>> {
        let options = Some(
            CreateContainerOptionsBuilder::new()
                .name(name)
                .platform("")
                .build(),
        );
        async move {
            Docker::create_container(self, options, config).await?;
            Ok(())
        }
        .boxed()
    }

    fn start_container<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), BollardError>> {
        Docker::start_container(self, name, None).boxed()
    }

    fn remove_container_force<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<(), BollardError>> {
        let options = Some(RemoveContainerOptionsBuilder::new().force(true).build());
        Docker::remove_container(self, name, options).boxed()
    }

    fn upload_to_container<'a>(
        &'a self,
        name: &'a str,
        path: &'a str,
        tar: Vec<u8>,
    ) -> BoxFuture<'a, Result<(), BollardError>> {
        let options = Some(UploadToContainerOptionsBuilder::new().path(path).build());
        let body = bollard::body_full(Bytes::from(tar));
        Docker::upload_to_container(self, name, options, body).boxed()
    }

    fn events(&self, options: Option<EventsOptions>) -> DockerEventStream {
        Box::pin(Docker::events(self, options))
    }
}

pub(crate) fn connect_docker() -> Result<Docker, LauncherError> {
    Docker::connect_with_local_defaults()
        .map_err(|e| LauncherError::Internal(format!("failed to connect docker socket: {e}")))
}

pub struct ContainerLaunch<'a> {
    pub name: &'a str,
    pub image: &'a str,
    pub network: &'a str,
    pub staging_path: &'a str,
    pub with_workspace: bool,
    pub plugin_uid: u32,
    pub plugin_gid: u32,
    pub pids_limit: u32,
    pub cpu_limit: &'a str,
    pub memory_limit: &'a str,
    pub read_only_rootfs: bool,
    pub env: &'a [(String, String)],
    pub labels: &'a [(String, String)],
    /// Raw secret pairs from session-broker: `(sanitized_name, plaintext_value)`.
    ///
    /// The launcher owns ALL mangling:
    /// - On-disk file name: `<sanitized_name>` lowercased (e.g. `github_com_pat`)
    /// - Env pointer: `BOTWORK_SECRET_<SANITIZED_NAME_UPPERCASED>_FILE=<SECRETS_TMPFS_PATH>/<file_name>`
    ///
    /// Secret values are written via the Docker daemon put-archive API into a
    /// container-local tmpfs at `SECRETS_TMPFS_PATH` (never touches host disk).
    /// Each file has mode `0400`, owned by `plugin_uid:plugin_gid`.
    pub secrets: &'a [(String, String)],
}

#[derive(Debug)]
pub struct LaunchOutcome {
    pub status: &'static str,
    pub container_ip: String,
}

pub async fn ensure_container(
    request: &ContainerLaunch<'_>,
    validators: &Validators,
) -> Result<LaunchOutcome, LauncherError> {
    let docker = connect_docker()?;
    ensure_container_impl(request, validators, &docker).await
}

async fn ensure_container_impl<D: DockerApi + ?Sized>(
    request: &ContainerLaunch<'_>,
    validators: &Validators,
    docker: &D,
) -> Result<LaunchOutcome, LauncherError> {
    match docker.inspect_container(request.name).await {
        Ok(inspect) => {
            if is_running(&inspect) {
                let container_ip = inspect_container_ip(&inspect, request.network, request.name)?;
                log_info(&format!(
                    "{} already running (ip={container_ip})",
                    request.name
                ));
                return Ok(LaunchOutcome {
                    status: "already_running",
                    container_ip,
                });
            }

            docker
                .remove_container_force(request.name)
                .await
                .map_err(|e| {
                    LauncherError::Internal(format!("failed to remove {}: {e}", request.name))
                })?;
        }
        Err(e) if is_not_found(&e) => {}
        Err(e) => {
            return Err(LauncherError::Internal(format!(
                "failed to inspect {}: {e}",
                request.name
            )));
        }
    }

    if request.with_workspace {
        setup_staging_dir(
            request.staging_path,
            validators,
            request.plugin_uid,
            request.plugin_gid,
        )?;
    }

    let config = build_container_config(request)?;

    docker
        .create_container(request.name, config)
        .await
        .map_err(|e| LauncherError::Internal(format!("failed to start {}: {e}", request.name)))?;

    // After create, before start: upload secrets into the tmpfs via the daemon
    // put-archive API.  Nothing touches the host disk.  If the upload fails we
    // remove the container and return an error (fail-closed: never start a
    // container that could not receive its secrets).
    if !request.secrets.is_empty() {
        let tar = build_secrets_tar(request.secrets, request.plugin_uid, request.plugin_gid);
        if let Err(e) = docker
            .upload_to_container(request.name, SECRETS_TMPFS_PATH, tar)
            .await
        {
            let _ = docker.remove_container_force(request.name).await;
            return Err(LauncherError::Internal(format!(
                "failed to upload secrets to {}: {e}",
                request.name
            )));
        }
    }

    docker
        .start_container(request.name)
        .await
        .map_err(|e| LauncherError::Internal(format!("failed to start {}: {e}", request.name)))?;

    let inspect = docker
        .inspect_container(request.name)
        .await
        .map_err(|e| LauncherError::Internal(format!("failed to inspect {}: {e}", request.name)))?;

    let container_ip = inspect_container_ip(&inspect, request.network, request.name)?;

    log_info(&format!(
        "started {} with image={} network={} staging={} ip={}",
        request.name, request.image, request.network, request.staging_path, container_ip
    ));

    Ok(LaunchOutcome {
        status: "started",
        container_ip,
    })
}

fn is_running(inspect: &ContainerInspectResponse) -> bool {
    let state = match inspect.state.as_ref() {
        Some(state) => state,
        None => return false,
    };

    if let Some(ContainerStateStatusEnum::RUNNING) = state.status {
        return true;
    }

    state.running.unwrap_or(false)
}

fn inspect_container_ip(
    inspect: &ContainerInspectResponse,
    network: &str,
    name: &str,
) -> Result<String, LauncherError> {
    let ip = inspect
        .network_settings
        .as_ref()
        .and_then(|settings| settings.networks.as_ref())
        .and_then(|networks| networks.get(network))
        .and_then(|endpoint| endpoint.ip_address.as_deref())
        .unwrap_or_default();

    parse_container_ip(ip, name, network)
}

pub(crate) fn parse_container_ip(
    stdout: &str,
    name: &str,
    network: &str,
) -> Result<String, LauncherError> {
    let ip = stdout.trim().to_string();
    if ip.is_empty() {
        return Err(LauncherError::Internal(format!(
            "container {name} has no address on network {network} (inspect returned empty)"
        )));
    }
    if ip.parse::<std::net::Ipv4Addr>().is_err() {
        return Err(LauncherError::Internal(format!(
            "container {name} returned non-IPv4 address {ip:?} on network {network}"
        )));
    }

    Ok(ip)
}

fn build_container_config(
    request: &ContainerLaunch<'_>,
) -> Result<ContainerCreateBody, LauncherError> {
    let mut labels = HashMap::new();
    for (key, value) in request.labels {
        labels.insert(key.clone(), value.clone());
    }

    // Non-secret env entries go into the container env verbatim.
    let mut env = request
        .env
        .iter()
        .map(|(name, value)| {
            if !valid_env_name(name) {
                return Err(LauncherError::Internal(format!("invalid env name: {name}")));
            }
            Ok(format!("{name}={value}"))
        })
        .collect::<Result<Vec<_>, LauncherError>>()?;

    // Secrets: inject only the `*_FILE` path pointer; the plaintext value is
    // written to the container-local tmpfs via the daemon put-archive API
    // (see `ensure_container_impl`).
    //
    // Pointer naming:
    //   env var:  BOTWORK_SECRET_<SANITIZED_NAME_UPPERCASED>_FILE
    //   file:     <SECRETS_TMPFS_PATH>/<sanitized_name_lowercased>
    //
    // The sanitized name arrives from session-broker already in its canonical
    // form (e.g. `GITHUB_COM_PAT`).  We uppercase it here as a no-op safety
    // measure and lowercase for the on-disk path.
    for (name, _value) in request.secrets {
        let upper_name = name.to_ascii_uppercase();
        let file_name = name.to_ascii_lowercase();
        let env_name = format!("BOTWORK_SECRET_{upper_name}_FILE");
        let env_value = format!("{SECRETS_TMPFS_PATH}/{file_name}");
        if !valid_env_name(&env_name) {
            return Err(LauncherError::Internal(format!(
                "invalid secret env name: {env_name}"
            )));
        }
        env.push(format!("{env_name}={env_value}"));
    }

    let mounts = if request.with_workspace {
        Some(vec![Mount {
            target: Some("/workspace".to_string()),
            source: Some(request.staging_path.to_string()),
            typ: Some(MountTypeEnum::BIND),
            bind_options: Some(MountBindOptions {
                propagation: Some(MountBindOptionsPropagationEnum::RSLAVE),
                ..Default::default()
            }),
            ..Default::default()
        }])
    } else {
        None
    };

    // A container-local tmpfs for secrets delivery.  The tmpfs is RAM-backed
    // and destroyed when the container exits (auto_remove: true), so no
    // host-side cleanup is ever needed.
    //
    // Options:
    //   rw         - the daemon writes into it via put-archive before start
    //   noexec     - no executables in the secrets dir
    //   nosuid     - belt-and-suspenders
    //   uid=…,gid=… - the directory is owned by the plugin uid/gid
    //   mode=0500  - plugin uid can traverse + read; others cannot
    //
    // `readonly_rootfs: true` does not affect a tmpfs mount — the tmpfs is its
    // own writable mount regardless of the rootfs setting.
    let tmpfs = if !request.secrets.is_empty() {
        let mut map = HashMap::new();
        map.insert(
            SECRETS_TMPFS_PATH.to_string(),
            format!(
                "rw,noexec,nosuid,uid={},gid={},mode=0500",
                request.plugin_uid, request.plugin_gid
            ),
        );
        Some(map)
    } else {
        None
    };

    let memory = parse_memory_limit(request.memory_limit)?;
    let nano_cpus = parse_cpu_limit(request.cpu_limit)?;

    let host_config = HostConfig {
        auto_remove: Some(true),
        network_mode: Some(request.network.to_string()),
        cap_drop: Some(vec!["ALL".to_string()]),
        security_opt: Some(vec!["no-new-privileges".to_string()]),
        pids_limit: Some(request.pids_limit as i64),
        memory: Some(memory),
        nano_cpus: Some(nano_cpus),
        readonly_rootfs: Some(request.read_only_rootfs),
        mounts,
        tmpfs,
        ..Default::default()
    };

    let mut endpoints = HashMap::new();
    endpoints.insert(
        request.network.to_string(),
        EndpointSettings {
            aliases: Some(vec![request.name.to_string()]),
            ..Default::default()
        },
    );

    Ok(ContainerCreateBody {
        image: Some(request.image.to_string()),
        user: Some(format!("{}:{}", request.plugin_uid, request.plugin_gid)),
        env: Some(env),
        labels: if labels.is_empty() {
            None
        } else {
            Some(labels)
        },
        host_config: Some(host_config),
        networking_config: Some(NetworkingConfig {
            endpoints_config: Some(endpoints),
        }),
        ..Default::default()
    })
}

fn parse_memory_limit(value: &str) -> Result<i64, LauncherError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(LauncherError::Internal("memory limit is empty".to_string()));
    }

    let split_at = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(trimmed.len());
    let (num_part, suffix_part) = trimmed.split_at(split_at);

    let number = num_part
        .parse::<f64>()
        .map_err(|e| LauncherError::Internal(format!("invalid memory limit {value:?}: {e}")))?;

    let multiplier = match suffix_part.to_ascii_lowercase().as_str() {
        "" | "b" => 1f64,
        "k" | "kb" => 1024f64,
        "m" | "mb" => 1024f64.powi(2),
        "g" | "gb" => 1024f64.powi(3),
        "t" | "tb" => 1024f64.powi(4),
        "p" | "pb" => 1024f64.powi(5),
        "e" | "eb" => 1024f64.powi(6),
        other => {
            return Err(LauncherError::Internal(format!(
                "invalid memory limit suffix {other:?} in {value:?}"
            )));
        }
    };

    let bytes = (number * multiplier).round();
    if !bytes.is_finite() || bytes <= 0.0 || bytes > i64::MAX as f64 {
        return Err(LauncherError::Internal(format!(
            "invalid memory limit {value:?}"
        )));
    }

    let bytes_i64 = format!("{bytes:.0}")
        .parse::<i64>()
        .map_err(|_| LauncherError::Internal(format!("invalid memory limit {value:?}")))?;
    Ok(bytes_i64)
}

fn parse_cpu_limit(value: &str) -> Result<i64, LauncherError> {
    let cpus = value
        .trim()
        .parse::<f64>()
        .map_err(|e| LauncherError::Internal(format!("invalid cpu limit {value:?}: {e}")))?;

    if !cpus.is_finite() || cpus <= 0.0 {
        return Err(LauncherError::Internal(format!(
            "invalid cpu limit {value:?}"
        )));
    }

    let nano = (cpus * 1_000_000_000f64).round();
    if nano > i64::MAX as f64 {
        return Err(LauncherError::Internal(format!(
            "invalid cpu limit {value:?}"
        )));
    }

    let nano_i64 = format!("{nano:.0}")
        .parse::<i64>()
        .map_err(|_| LauncherError::Internal(format!("invalid cpu limit {value:?}")))?;
    Ok(nano_i64)
}

/// Build an in-memory POSIX ustar tar archive containing one file per secret.
///
/// Each file entry has:
/// - Name:  `<sanitized_name>` lowercased  (e.g. `github_com_pat`)
/// - Mode:  `0400` — readable only by the owning uid
/// - Owner: `uid` / `gid` of the plugin process
/// - Data:  the plaintext secret value bytes
///
/// The archive is intended to be extracted into `SECRETS_TMPFS_PATH` inside
/// the container via the Docker daemon put-archive API, so tar entries contain
/// only bare filenames (no directory prefix).
fn build_secrets_tar(secrets: &[(String, String)], uid: u32, gid: u32) -> Vec<u8> {
    let mut archive: Vec<u8> = Vec::new();
    for (name, value) in secrets {
        let file_name = name.to_ascii_lowercase();
        append_tar_entry(&mut archive, &file_name, value.as_bytes(), 0o400, uid, gid);
    }
    // POSIX tar: two consecutive 512-byte zero blocks mark end-of-archive.
    archive.extend_from_slice(&[0u8; 1024]);
    archive
}

/// Append one POSIX ustar file entry to a tar byte buffer.
///
/// The header is a standard 512-byte block (ustar format):
/// offset  len  field
///    0   100   name (file name, NUL-terminated)
///  100     8   mode (octal ASCII, NUL-terminated)
///  108     8   uid  (octal ASCII, NUL-terminated)
///  116     8   gid  (octal ASCII, NUL-terminated)
///  124    12   size (octal ASCII, NUL-terminated)
///  136    12   mtime (octal ASCII, NUL-terminated)  — zeroed (epoch)
///  148     8   checksum (ASCII)
///  156     1   typeflag ('0' = regular file)
///  157   100   linkname (empty)
///  257     6   magic  ("ustar\0")
///  263     2   version ("00")
///  ...     rest zeroed
fn append_tar_entry(archive: &mut Vec<u8>, name: &str, data: &[u8], mode: u32, uid: u32, gid: u32) {
    let mut header = [0u8; 512];

    // name (offset 0, len 100): truncate silently; keys are sanitized short names
    let name_bytes = name.as_bytes();
    let copy_len = name_bytes.len().min(99);
    header[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

    // mode (offset 100, len 8): e.g. "0000400\0"
    write_octal(&mut header[100..108], mode as u64, 7);
    // uid (offset 108, len 8)
    write_octal(&mut header[108..116], uid as u64, 7);
    // gid (offset 116, len 8)
    write_octal(&mut header[116..124], gid as u64, 7);
    // size (offset 124, len 12)
    write_octal(&mut header[124..136], data.len() as u64, 11);
    // mtime (offset 136, len 12): epoch
    write_octal(&mut header[136..148], 0, 11);
    // typeflag (offset 156): '0' = regular file
    header[156] = b'0';
    // ustar magic (offset 257, len 6)
    header[257..263].copy_from_slice(b"ustar\0");
    // ustar version (offset 263, len 2)
    header[263..265].copy_from_slice(b"00");

    // Checksum: sum of all header bytes (treating checksum field as spaces)
    for b in &mut header[148..156] {
        *b = b' ';
    }
    let checksum: u32 = header.iter().map(|&b| b as u32).sum();
    // Write checksum as 6 octal digits + NUL + space (POSIX convention)
    let cs_str = format!("{checksum:06o}\0 ");
    header[148..156].copy_from_slice(cs_str.as_bytes());

    archive.extend_from_slice(&header);

    // Data blocks (padded to 512-byte boundary)
    archive.extend_from_slice(data);
    let pad = (512 - (data.len() % 512)) % 512;
    archive.extend(std::iter::repeat_n(0u8, pad));
}

/// Write `value` as a NUL-terminated octal string into `buf` with `digits` significant digits.
fn write_octal(buf: &mut [u8], value: u64, digits: usize) {
    let s = format!("{value:0>digits$o}\0");
    let copy_len = s.len().min(buf.len());
    buf[..copy_len].copy_from_slice(&s.as_bytes()[..copy_len]);
}

pub async fn teardown(
    name: &str,
    staging_path: &str,
    validators: &Validators,
) -> Result<(), LauncherError> {
    let docker = connect_docker()?;
    teardown_impl(name, staging_path, validators, &docker, run_command).await
}

async fn teardown_impl<D: DockerApi + ?Sized>(
    name: &str,
    staging_path: &str,
    validators: &Validators,
    docker: &D,
    mut run: impl FnMut(&[String]) -> Result<CommandOutput, String>,
) -> Result<(), LauncherError> {
    let safe_staging = validators.safe_staging_path(staging_path)?;

    match docker.remove_container_force(name).await {
        Ok(()) => {}
        Err(e) if is_not_found(&e) => {}
        Err(e) => {
            log_info(&format!("docker rm -f {name} failed (non-fatal): {e}"));
        }
    }

    for _ in 0..2 {
        let umount =
            run(&["umount".to_string(), safe_staging.clone()]).map_err(LauncherError::Internal)?;
        if umount.returncode != 0 {
            if is_not_mounted_or_einval(&umount.stderr) {
                break;
            }

            log_info(&format!(
                "umount {safe_staging} failed (non-fatal): {}",
                umount.stderr.trim()
            ));
            break;
        }
    }

    let _ = fs::remove_dir(&safe_staging);

    log_info(&format!(
        "teardown complete: name={name} staging={safe_staging}"
    ));
    Ok(())
}

fn is_not_found(error: &BollardError) -> bool {
    matches!(
        error,
        BollardError::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::{ContainerState, NetworkSettings};
    use futures_util::stream;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Type alias to satisfy `clippy::type_complexity` for the uploaded-tars capture buffer.
    type UploadedTars = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    #[derive(Default, Clone)]
    struct FakeDocker {
        inspect_results: Arc<Mutex<VecDeque<Result<ContainerInspectResponse, BollardError>>>>,
        create_results: Arc<Mutex<VecDeque<Result<(), BollardError>>>>,
        start_results: Arc<Mutex<VecDeque<Result<(), BollardError>>>>,
        remove_results: Arc<Mutex<VecDeque<Result<(), BollardError>>>>,
        upload_results: Arc<Mutex<VecDeque<Result<(), BollardError>>>>,
        created_configs: Arc<Mutex<Vec<ContainerCreateBody>>>,
        /// Captures (path, tar_bytes) pairs from `upload_to_container` calls.
        uploaded_tars: UploadedTars,
    }

    impl FakeDocker {
        fn with_inspect(
            self,
            results: Vec<Result<ContainerInspectResponse, BollardError>>,
        ) -> Self {
            *self.inspect_results.lock().expect("inspect lock") = VecDeque::from(results);
            self
        }

        fn with_create(self, results: Vec<Result<(), BollardError>>) -> Self {
            *self.create_results.lock().expect("create lock") = VecDeque::from(results);
            self
        }

        fn with_start(self, results: Vec<Result<(), BollardError>>) -> Self {
            *self.start_results.lock().expect("start lock") = VecDeque::from(results);
            self
        }

        fn with_remove(self, results: Vec<Result<(), BollardError>>) -> Self {
            *self.remove_results.lock().expect("remove lock") = VecDeque::from(results);
            self
        }

        fn with_upload(self, results: Vec<Result<(), BollardError>>) -> Self {
            *self.upload_results.lock().expect("upload lock") = VecDeque::from(results);
            self
        }

        fn uploaded_tars(&self) -> Vec<(String, Vec<u8>)> {
            self.uploaded_tars
                .lock()
                .expect("uploaded_tars lock")
                .clone()
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
                    .expect("missing inspect result")
            }
            .boxed()
        }

        fn create_container<'a>(
            &'a self,
            _name: &'a str,
            config: ContainerCreateBody,
        ) -> BoxFuture<'a, Result<(), BollardError>> {
            async move {
                self.created_configs
                    .lock()
                    .expect("created lock")
                    .push(config);
                self.create_results
                    .lock()
                    .expect("create lock")
                    .pop_front()
                    .unwrap_or(Ok(()))
            }
            .boxed()
        }

        fn start_container<'a>(
            &'a self,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<(), BollardError>> {
            async move {
                self.start_results
                    .lock()
                    .expect("start lock")
                    .pop_front()
                    .unwrap_or(Ok(()))
            }
            .boxed()
        }

        fn remove_container_force<'a>(
            &'a self,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<(), BollardError>> {
            async move {
                self.remove_results
                    .lock()
                    .expect("remove lock")
                    .pop_front()
                    .unwrap_or(Ok(()))
            }
            .boxed()
        }

        fn upload_to_container<'a>(
            &'a self,
            _name: &'a str,
            path: &'a str,
            tar: Vec<u8>,
        ) -> BoxFuture<'a, Result<(), BollardError>> {
            let path = path.to_string();
            async move {
                self.uploaded_tars
                    .lock()
                    .expect("uploaded_tars lock")
                    .push((path, tar));
                self.upload_results
                    .lock()
                    .expect("upload lock")
                    .pop_front()
                    .unwrap_or(Ok(()))
            }
            .boxed()
        }

        fn events(&self, _options: Option<EventsOptions>) -> DockerEventStream {
            Box::pin(stream::iter(
                Vec::<Result<EventMessage, BollardError>>::new(),
            ))
        }
    }

    fn minimal_launch<'a>(
        name: &'a str,
        env: &'a [(String, String)],
        labels: &'a [(String, String)],
    ) -> ContainerLaunch<'a> {
        ContainerLaunch {
            name,
            image: "botwork/mcp-echo:local",
            network: "botwork",
            staging_path: "/var/lib/botwork/tenants/acme/staging/aabbccddeeff",
            with_workspace: false,
            plugin_uid: 1000,
            plugin_gid: 1000,
            pids_limit: 256,
            cpu_limit: "1.0",
            memory_limit: "512m",
            read_only_rootfs: true,
            env,
            labels,
            secrets: &[],
        }
    }

    fn inspect_running(ip: &str, network: &str) -> ContainerInspectResponse {
        let mut networks = HashMap::new();
        networks.insert(
            network.to_string(),
            EndpointSettings {
                ip_address: Some(ip.to_string()),
                ..Default::default()
            },
        );

        ContainerInspectResponse {
            state: Some(ContainerState {
                status: Some(ContainerStateStatusEnum::RUNNING),
                running: Some(true),
                ..Default::default()
            }),
            network_settings: Some(NetworkSettings {
                networks: Some(networks),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn parse_container_ip_returns_valid_ipv4() {
        let ip = parse_container_ip("192.168.1.100\n", "mcp_session_x", "botwork-plugin")
            .expect("valid IPv4");
        assert_eq!(ip, "192.168.1.100");
    }

    #[test]
    fn parse_container_ip_fails_on_empty_output() {
        let err = parse_container_ip("", "mcp_session_x", "botwork-plugin").unwrap_err();
        assert!(matches!(err, LauncherError::Internal(_)));
    }

    #[test]
    fn parse_container_ip_fails_on_non_ipv4_address() {
        let err = parse_container_ip("::1", "mcp_session_x", "botwork-plugin").unwrap_err();
        assert!(matches!(err, LauncherError::Internal(_)));
    }

    #[test]
    fn build_container_config_includes_sandbox_flags_and_all_env() {
        let env = vec![("FOO".to_string(), "plain".to_string())];
        let secrets = vec![("TOKEN".to_string(), "s3cr3t".to_string())];
        let labels = vec![("io.botworkz.tenant".to_string(), "acme".to_string())];
        let mut launch = minimal_launch("mcp_session_aabbccddeeff", &env, &labels);
        launch.with_workspace = true;
        launch.secrets = &secrets;

        let config = build_container_config(&launch).expect("config");
        let host = config.host_config.expect("host config");

        assert_eq!(host.auto_remove, Some(true));
        assert_eq!(host.cap_drop, Some(vec!["ALL".to_string()]));
        assert_eq!(
            host.security_opt,
            Some(vec!["no-new-privileges".to_string()])
        );
        assert_eq!(host.pids_limit, Some(256));
        assert_eq!(host.memory, Some(536_870_912));
        assert_eq!(host.nano_cpus, Some(1_000_000_000));
        assert_eq!(host.readonly_rootfs, Some(true));

        let env_list = config.env.expect("env");
        // Non-secret env is present verbatim.
        assert!(
            env_list.contains(&"FOO=plain".to_string()),
            "expected FOO=plain in env: {env_list:?}"
        );
        // The *_FILE pointer for the secret must be present.
        let expected_file_ptr = format!("BOTWORK_SECRET_TOKEN_FILE={SECRETS_TMPFS_PATH}/token");
        assert!(
            env_list.contains(&expected_file_ptr),
            "expected {expected_file_ptr} in env: {env_list:?}"
        );
        // The plaintext secret value must NOT appear anywhere in the env list.
        for entry in &env_list {
            assert!(
                !entry.contains("s3cr3t"),
                "plaintext secret value must not appear in env: {entry}"
            );
        }

        // Workspace bind mount is still present.
        let mounts = host.mounts.expect("mounts");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].target.as_deref(), Some("/workspace"));
        assert_eq!(mounts[0].source.as_deref(), Some(launch.staging_path));
        assert_eq!(mounts[0].typ, Some(MountTypeEnum::BIND));
        assert_eq!(
            mounts[0].bind_options.as_ref().and_then(|b| b.propagation),
            Some(MountBindOptionsPropagationEnum::RSLAVE)
        );

        // Secrets tmpfs must be configured.
        let tmpfs = host.tmpfs.expect("tmpfs");
        let tmpfs_opts = tmpfs.get(SECRETS_TMPFS_PATH).expect("secrets tmpfs entry");
        assert!(
            tmpfs_opts.contains("noexec"),
            "tmpfs options must contain noexec: {tmpfs_opts}"
        );
        assert!(
            tmpfs_opts.contains("nosuid"),
            "tmpfs options must contain nosuid: {tmpfs_opts}"
        );
        assert!(
            tmpfs_opts.contains("mode=0500"),
            "tmpfs options must contain mode=0500: {tmpfs_opts}"
        );
        assert!(
            tmpfs_opts.contains(&format!("uid={}", launch.plugin_uid)),
            "tmpfs options must contain uid: {tmpfs_opts}"
        );
        assert!(
            tmpfs_opts.contains(&format!("gid={}", launch.plugin_gid)),
            "tmpfs options must contain gid: {tmpfs_opts}"
        );

        assert_eq!(
            config
                .networking_config
                .as_ref()
                .and_then(|n| n.endpoints_config.as_ref())
                .and_then(|m| m.get(launch.network))
                .and_then(|e| e.aliases.as_ref())
                .cloned(),
            Some(vec![launch.name.to_string()])
        );
    }

    #[tokio::test]
    async fn ensure_container_returns_already_running_when_running() {
        let validators =
            Validators::new(r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$").expect("validators");
        let launch = minimal_launch("mcp_session_aabbccddeeff", &[], &[]);
        let fake =
            FakeDocker::default().with_inspect(vec![Ok(inspect_running("10.0.0.8", "botwork"))]);

        let outcome = ensure_container_impl(&launch, &validators, &fake)
            .await
            .expect("outcome");

        assert_eq!(outcome.status, "already_running");
        assert_eq!(outcome.container_ip, "10.0.0.8");
    }

    #[tokio::test]
    async fn ensure_container_starts_when_missing() {
        let validators =
            Validators::new(r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$").expect("validators");
        let launch = minimal_launch("mcp_session_aabbccddeeff", &[], &[]);

        let fake = FakeDocker::default()
            .with_inspect(vec![
                Err(BollardError::DockerResponseServerError {
                    status_code: 404,
                    message: "not found".to_string(),
                }),
                Ok(inspect_running("172.20.0.5", "botwork")),
            ])
            .with_create(vec![Ok(())])
            .with_start(vec![Ok(())]);

        let outcome = ensure_container_impl(&launch, &validators, &fake)
            .await
            .expect("outcome");

        assert_eq!(outcome.status, "started");
        assert_eq!(outcome.container_ip, "172.20.0.5");
    }

    #[tokio::test]
    async fn ensure_container_returns_internal_when_inspect_fails() {
        let validators =
            Validators::new(r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$").expect("validators");
        let launch = minimal_launch("mcp_session_aabbccddeeff", &[], &[]);
        let fake = FakeDocker::default().with_inspect(vec![Err(
            BollardError::DockerResponseServerError {
                status_code: 500,
                message: "boom".to_string(),
            },
        )]);

        let err = ensure_container_impl(&launch, &validators, &fake)
            .await
            .expect_err("expected failure");

        assert!(matches!(err, LauncherError::Internal(_)));
    }

    #[tokio::test]
    async fn ensure_container_returns_internal_when_start_fails() {
        let validators =
            Validators::new(r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$").expect("validators");
        let launch = minimal_launch("mcp_session_aabbccddeeff", &[], &[]);
        let fake = FakeDocker::default()
            .with_inspect(vec![Err(BollardError::DockerResponseServerError {
                status_code: 404,
                message: "not found".to_string(),
            })])
            .with_create(vec![Ok(())])
            .with_start(vec![Err(BollardError::DockerResponseServerError {
                status_code: 500,
                message: "cannot start".to_string(),
            })]);

        let err = ensure_container_impl(&launch, &validators, &fake)
            .await
            .expect_err("expected failure");

        assert!(matches!(err, LauncherError::Internal(_)));
    }

    #[tokio::test]
    async fn teardown_succeeds_when_container_already_removed() {
        let validators =
            Validators::new(r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$").expect("validators");
        let fake =
            FakeDocker::default().with_remove(vec![Err(BollardError::DockerResponseServerError {
                status_code: 404,
                message: "gone".to_string(),
            })]);

        let result = teardown_impl(
            "mcp_session_aabbccddeeff",
            "/var/lib/botwork/tenants/acme/staging/aabbccddeeff",
            &validators,
            &fake,
            |_| {
                Ok(CommandOutput {
                    returncode: 1,
                    stdout: String::new(),
                    stderr: "not mounted".to_string(),
                })
            },
        )
        .await;

        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[tokio::test]
    async fn teardown_rejects_invalid_staging_path() {
        let validators =
            Validators::new(r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$").expect("validators");
        let fake = FakeDocker::default();

        let err = teardown_impl(
            "mcp_session_aabbccddeeff",
            "/invalid/path",
            &validators,
            &fake,
            |_| unreachable!(),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, LauncherError::BadRequest(_)));
    }

    // --------------- secrets delivery tests ---------------

    /// Helper: extract the content of a named file from a tar archive.
    fn tar_file_content(archive: &[u8], target_name: &str) -> Option<Vec<u8>> {
        let mut pos = 0;
        while pos + 512 <= archive.len() {
            let header = &archive[pos..pos + 512];
            // Check for end-of-archive (two zero blocks)
            if header.iter().all(|&b| b == 0) {
                return None;
            }
            // Name is null-terminated in first 100 bytes
            let name_end = header[..100].iter().position(|&b| b == 0).unwrap_or(100);
            let name = std::str::from_utf8(&header[..name_end]).unwrap_or("");
            // Size is octal in bytes 124..136
            let size_str = std::str::from_utf8(&header[124..136])
                .unwrap_or("0")
                .trim_matches(|c: char| !c.is_ascii_digit());
            let size = u64::from_str_radix(size_str.trim(), 8).unwrap_or(0) as usize;
            pos += 512;
            if name == target_name {
                return Some(archive[pos..pos + size].to_vec());
            }
            // Skip data blocks (padded to 512)
            let padded = size.div_ceil(512) * 512;
            pos += padded;
        }
        None
    }

    /// Helper: return the mode field (octal) from a named tar entry's header.
    fn tar_file_mode(archive: &[u8], target_name: &str) -> Option<u32> {
        let mut pos = 0;
        while pos + 512 <= archive.len() {
            let header = &archive[pos..pos + 512];
            if header.iter().all(|&b| b == 0) {
                return None;
            }
            let name_end = header[..100].iter().position(|&b| b == 0).unwrap_or(100);
            let name = std::str::from_utf8(&header[..name_end]).unwrap_or("");
            let mode_str = std::str::from_utf8(&header[100..108])
                .unwrap_or("0")
                .trim_matches(|c: char| !c.is_ascii_digit());
            let size_str = std::str::from_utf8(&header[124..136])
                .unwrap_or("0")
                .trim_matches(|c: char| !c.is_ascii_digit());
            let size = u64::from_str_radix(size_str.trim(), 8).unwrap_or(0) as usize;
            if name == target_name {
                let mode = u32::from_str_radix(mode_str.trim(), 8).unwrap_or(0);
                return Some(mode);
            }
            pos += 512;
            let padded = size.div_ceil(512) * 512;
            pos += padded;
        }
        None
    }

    #[test]
    fn build_secrets_tar_produces_readable_entry() {
        let secrets = vec![("GITHUB_COM_PAT".to_string(), "ghp_test_value".to_string())];
        let tar = build_secrets_tar(&secrets, 1000, 1000);

        // The file should be named by the lowercased key.
        let content =
            tar_file_content(&tar, "github_com_pat").expect("tar must contain github_com_pat");
        assert_eq!(content, b"ghp_test_value");

        let mode = tar_file_mode(&tar, "github_com_pat").expect("mode");
        assert_eq!(mode, 0o400, "file mode must be 0400");
    }

    #[tokio::test]
    async fn ensure_container_uploads_secrets_before_start() {
        let validators =
            Validators::new(r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$").expect("validators");
        let secrets = vec![("GITHUB_COM_PAT".to_string(), "ghp_abc".to_string())];
        let mut launch = minimal_launch("mcp_session_aabbccddeeff", &[], &[]);
        launch.secrets = &secrets;

        let fake = FakeDocker::default()
            .with_inspect(vec![
                Err(BollardError::DockerResponseServerError {
                    status_code: 404,
                    message: "not found".to_string(),
                }),
                Ok(inspect_running("172.20.0.5", "botwork")),
            ])
            .with_create(vec![Ok(())])
            .with_start(vec![Ok(())]);

        let outcome = ensure_container_impl(&launch, &validators, &fake)
            .await
            .expect("outcome");
        assert_eq!(outcome.status, "started");

        // Verify the tar was uploaded to the correct path.
        let uploads = fake.uploaded_tars();
        assert_eq!(uploads.len(), 1, "expected exactly one upload");
        let (path, tar) = &uploads[0];
        assert_eq!(path, SECRETS_TMPFS_PATH);

        // Verify the tar contains the secret file with correct content.
        let content =
            tar_file_content(tar, "github_com_pat").expect("tar must contain github_com_pat");
        assert_eq!(content, b"ghp_abc");

        // Verify env contains the *_FILE pointer, not the plaintext value.
        let configs = fake.created_configs.lock().expect("lock");
        let env = configs[0].env.as_ref().expect("env");
        let expected_ptr =
            format!("BOTWORK_SECRET_GITHUB_COM_PAT_FILE={SECRETS_TMPFS_PATH}/github_com_pat");
        assert!(
            env.contains(&expected_ptr),
            "env must contain FILE pointer: {env:?}"
        );
        for entry in env {
            assert!(
                !entry.contains("ghp_abc"),
                "plaintext secret value must not appear in env: {entry}"
            );
        }
    }

    #[tokio::test]
    async fn ensure_container_fails_closed_when_upload_fails() {
        let validators =
            Validators::new(r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$").expect("validators");
        let secrets = vec![("TOKEN".to_string(), "secret".to_string())];
        let mut launch = minimal_launch("mcp_session_aabbccddeeff", &[], &[]);
        launch.secrets = &secrets;

        let fake = FakeDocker::default()
            .with_inspect(vec![Err(BollardError::DockerResponseServerError {
                status_code: 404,
                message: "not found".to_string(),
            })])
            .with_create(vec![Ok(())])
            .with_upload(vec![Err(BollardError::DockerResponseServerError {
                status_code: 500,
                message: "upload failed".to_string(),
            })]);

        let err = ensure_container_impl(&launch, &validators, &fake)
            .await
            .expect_err("expected failure");

        // Must fail with Internal error (fail-closed).
        assert!(
            matches!(err, LauncherError::Internal(_)),
            "expected Internal error, got {err:?}"
        );
        // The container must NOT have been started.
        assert_eq!(
            fake.start_results.lock().expect("lock").len(),
            0,
            "start_container must not be called when upload fails"
        );
    }

    // --------------- parse_memory_limit ---------------

    #[test]
    fn parse_memory_limit_valid_m() {
        assert_eq!(parse_memory_limit("512m").expect("valid"), 536_870_912);
    }

    #[test]
    fn parse_memory_limit_valid_g() {
        assert_eq!(parse_memory_limit("1g").expect("valid"), 1_073_741_824);
    }

    #[test]
    fn parse_memory_limit_valid_k() {
        assert_eq!(parse_memory_limit("1024k").expect("valid"), 1_048_576);
    }

    #[test]
    fn parse_memory_limit_valid_b_suffix() {
        assert_eq!(parse_memory_limit("1b").expect("valid"), 1);
    }

    #[test]
    fn parse_memory_limit_valid_no_suffix() {
        assert_eq!(parse_memory_limit("1").expect("valid"), 1);
    }

    #[test]
    fn parse_memory_limit_valid_two_char_suffixes() {
        assert_eq!(parse_memory_limit("1kb").expect("kb"), 1_024);
        assert_eq!(parse_memory_limit("1mb").expect("mb"), 1_048_576);
        assert_eq!(parse_memory_limit("1gb").expect("gb"), 1_073_741_824);
        assert_eq!(parse_memory_limit("1tb").expect("tb"), 1_099_511_627_776);
        assert_eq!(
            parse_memory_limit("1pb").expect("pb"),
            1_125_899_906_842_624
        );
    }

    #[test]
    fn parse_memory_limit_case_insensitive() {
        assert_eq!(
            parse_memory_limit("512M").expect("uppercase M"),
            536_870_912
        );
    }

    #[test]
    fn parse_memory_limit_fails_empty() {
        assert!(matches!(
            parse_memory_limit("").unwrap_err(),
            LauncherError::Internal(_)
        ));
    }

    #[test]
    fn parse_memory_limit_fails_bad_suffix() {
        assert!(matches!(
            parse_memory_limit("512x").unwrap_err(),
            LauncherError::Internal(_)
        ));
    }

    #[test]
    fn parse_memory_limit_fails_non_numeric() {
        assert!(matches!(
            parse_memory_limit("abc").unwrap_err(),
            LauncherError::Internal(_)
        ));
    }

    #[test]
    fn parse_memory_limit_fails_zero() {
        assert!(matches!(
            parse_memory_limit("0").unwrap_err(),
            LauncherError::Internal(_)
        ));
    }

    #[test]
    fn parse_memory_limit_fails_negative() {
        assert!(matches!(
            parse_memory_limit("-5m").unwrap_err(),
            LauncherError::Internal(_)
        ));
    }

    #[test]
    fn parse_memory_limit_fails_overflow() {
        // 9_999_999_999 GiB exceeds i64::MAX
        assert!(matches!(
            parse_memory_limit("9999999999g").unwrap_err(),
            LauncherError::Internal(_)
        ));
    }

    // --------------- parse_cpu_limit ---------------

    #[test]
    fn parse_cpu_limit_valid_one() {
        assert_eq!(parse_cpu_limit("1.0").expect("valid"), 1_000_000_000);
    }

    #[test]
    fn parse_cpu_limit_valid_half() {
        assert_eq!(parse_cpu_limit("0.5").expect("valid"), 500_000_000);
    }

    #[test]
    fn parse_cpu_limit_valid_integer() {
        assert_eq!(parse_cpu_limit("2").expect("valid"), 2_000_000_000);
    }

    #[test]
    fn parse_cpu_limit_fails_non_numeric() {
        assert!(matches!(
            parse_cpu_limit("abc").unwrap_err(),
            LauncherError::Internal(_)
        ));
    }

    #[test]
    fn parse_cpu_limit_fails_zero() {
        assert!(matches!(
            parse_cpu_limit("0").unwrap_err(),
            LauncherError::Internal(_)
        ));
    }

    #[test]
    fn parse_cpu_limit_fails_negative() {
        assert!(matches!(
            parse_cpu_limit("-1").unwrap_err(),
            LauncherError::Internal(_)
        ));
    }

    #[test]
    fn parse_cpu_limit_fails_overflow() {
        // 1_000_000_000_000 CPUs * 1_000_000_000 nano/CPU exceeds i64::MAX
        assert!(matches!(
            parse_cpu_limit("1e12").unwrap_err(),
            LauncherError::Internal(_)
        ));
    }

    // --------------- inspect_container_ip missing-network paths ---------------

    #[test]
    fn inspect_container_ip_fails_when_no_network_settings() {
        let inspect = ContainerInspectResponse {
            network_settings: None,
            ..Default::default()
        };
        let err = inspect_container_ip(&inspect, "botwork", "test-container").unwrap_err();
        assert!(matches!(err, LauncherError::Internal(_)));
    }

    #[test]
    fn inspect_container_ip_fails_when_network_key_absent() {
        let mut networks = HashMap::new();
        networks.insert(
            "other-net".to_string(),
            EndpointSettings {
                ip_address: Some("10.0.0.1".to_string()),
                ..Default::default()
            },
        );
        let inspect = ContainerInspectResponse {
            network_settings: Some(NetworkSettings {
                networks: Some(networks),
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = inspect_container_ip(&inspect, "botwork", "test-container").unwrap_err();
        assert!(matches!(err, LauncherError::Internal(_)));
    }

    // --------------- is_running edge cases ---------------

    #[test]
    fn is_running_returns_true_when_running_field_true_and_status_none() {
        let inspect = ContainerInspectResponse {
            state: Some(ContainerState {
                status: None,
                running: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(is_running(&inspect));
    }

    #[test]
    fn is_running_returns_false_when_state_none() {
        let inspect = ContainerInspectResponse {
            state: None,
            ..Default::default()
        };
        assert!(!is_running(&inspect));
    }

    // --------------- stopped-container path in ensure_container_impl ---------------

    fn inspect_stopped() -> ContainerInspectResponse {
        ContainerInspectResponse {
            state: Some(ContainerState {
                status: Some(ContainerStateStatusEnum::EXITED),
                running: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn ensure_container_removes_stopped_and_starts_fresh() {
        let validators =
            Validators::new(r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$").expect("validators");
        let launch = minimal_launch("mcp_session_aabbccddeeff", &[], &[]);

        let fake = FakeDocker::default()
            .with_inspect(vec![
                Ok(inspect_stopped()),
                Ok(inspect_running("10.0.0.9", "botwork")),
            ])
            .with_remove(vec![Ok(())])
            .with_create(vec![Ok(())])
            .with_start(vec![Ok(())]);

        let outcome = ensure_container_impl(&launch, &validators, &fake)
            .await
            .expect("outcome");

        assert_eq!(outcome.status, "started");
        assert_eq!(outcome.container_ip, "10.0.0.9");
    }

    // --------------- teardown non-404 server error is non-fatal ---------------

    #[tokio::test]
    async fn teardown_succeeds_when_remove_returns_server_error() {
        let validators =
            Validators::new(r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$").expect("validators");
        let fake =
            FakeDocker::default().with_remove(vec![Err(BollardError::DockerResponseServerError {
                status_code: 500,
                message: "internal server error".to_string(),
            })]);

        let result = teardown_impl(
            "mcp_session_aabbccddeeff",
            "/var/lib/botwork/tenants/acme/staging/aabbccddeeff",
            &validators,
            &fake,
            |_| {
                Ok(CommandOutput {
                    returncode: 1,
                    stdout: String::new(),
                    stderr: "not mounted".to_string(),
                })
            },
        )
        .await;

        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }
}
