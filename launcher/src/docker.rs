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
    RemoveContainerOptionsBuilder,
};
use bollard::Docker;
use futures_util::future::{BoxFuture, FutureExt};
use futures_util::Stream;

use crate::cmd::{log_info, run_command, CommandOutput};
use crate::error::LauncherError;
use crate::mount::{is_not_mounted_or_einval, setup_staging_dir};
use crate::validate::{valid_env_name, Validators};

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

    fn events(&self, options: Option<EventsOptions>) -> DockerEventStream;
}

/// Production implementation — connects over the local docker socket.
/// NOT covered by offline unit tests.
#[cfg(not(tarpaulin_include))]
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

    fn events(&self, options: Option<EventsOptions>) -> DockerEventStream {
        Box::pin(Docker::events(self, options))
    }
}

/// Connect to the local docker socket.
/// NOT covered by offline unit tests.
#[cfg(not(tarpaulin_include))]
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

    let env = request
        .env
        .iter()
        .map(|(name, value)| {
            if !valid_env_name(name) {
                return Err(LauncherError::Internal(format!("invalid env name: {name}")));
            }
            Ok(format!("{name}={value}"))
        })
        .collect::<Result<Vec<_>, LauncherError>>()?;

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

    #[derive(Default, Clone)]
    struct FakeDocker {
        inspect_results: Arc<Mutex<VecDeque<Result<ContainerInspectResponse, BollardError>>>>,
        create_results: Arc<Mutex<VecDeque<Result<(), BollardError>>>>,
        start_results: Arc<Mutex<VecDeque<Result<(), BollardError>>>>,
        remove_results: Arc<Mutex<VecDeque<Result<(), BollardError>>>>,
        created_configs: Arc<Mutex<Vec<ContainerCreateBody>>>,
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
        let env = vec![
            ("FOO".to_string(), "plain".to_string()),
            ("BOTWORK_SECRET_TOKEN".to_string(), "s3cr3t".to_string()),
        ];
        let labels = vec![("io.botworkz.tenant".to_string(), "acme".to_string())];
        let mut launch = minimal_launch("mcp_session_aabbccddeeff", &env, &labels);
        launch.with_workspace = true;

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

        assert_eq!(
            config.env,
            Some(vec![
                "FOO=plain".to_string(),
                "BOTWORK_SECRET_TOKEN=s3cr3t".to_string()
            ])
        );

        let mounts = host.mounts.expect("mounts");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].target.as_deref(), Some("/workspace"));
        assert_eq!(mounts[0].source.as_deref(), Some(launch.staging_path));
        assert_eq!(mounts[0].typ, Some(MountTypeEnum::BIND));
        assert_eq!(
            mounts[0].bind_options.as_ref().and_then(|b| b.propagation),
            Some(MountBindOptionsPropagationEnum::RSLAVE)
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
