use std::fs;

use crate::cmd::{log_info, run_command};
use crate::error::LauncherError;
use crate::mount::{fallback_message, is_not_mounted_or_einval, setup_staging_dir};
use crate::validate::valid_env_name;
use crate::validate::Validators;

pub struct ContainerLaunch<'a> {
    pub name: &'a str,
    pub image: &'a str,
    pub network: &'a str,
    pub staging_path: &'a str,
    pub with_workspace: bool,
    pub plugin_uid: u32,
    pub plugin_gid: u32,
    pub pids_limit: u32,
    pub memory_limit: &'a str,
    pub read_only_rootfs: bool,
    pub env: &'a [(String, String)],
}

pub fn ensure_container(
    request: &ContainerLaunch<'_>,
    validators: &Validators,
) -> Result<&'static str, LauncherError> {
    let inspect = run_command(&[
        "docker".to_string(),
        "inspect".to_string(),
        "--format".to_string(),
        "{{.State.Status}}".to_string(),
        request.name.to_string(),
    ])
    .map_err(LauncherError::Internal)?;

    if inspect.returncode == 0 {
        let status = inspect.stdout.trim();
        if status == "running" {
            log_info(&format!("{} already running", request.name));
            return Ok("already_running");
        }

        let remove = run_command(&[
            "docker".to_string(),
            "rm".to_string(),
            "-f".to_string(),
            request.name.to_string(),
        ])
        .map_err(LauncherError::Internal)?;
        if remove.returncode != 0 {
            log_docker_failure("rm", &remove);
            return Err(LauncherError::Internal(fallback_message(
                &remove.stderr,
                format!("failed to remove {}", request.name),
            )));
        }
    } else if !is_no_such_object(&inspect.stderr) {
        log_docker_failure("inspect", &inspect);
        return Err(LauncherError::Internal(fallback_message(
            &inspect.stderr,
            format!("failed to inspect {}", request.name),
        )));
    }

    if request.with_workspace {
        setup_staging_dir(
            request.staging_path,
            validators,
            request.plugin_uid,
            request.plugin_gid,
        )?;
    }

    let run_cmd = docker_run_args(request);

    let launch = run_command(&run_cmd).map_err(LauncherError::Internal)?;
    if launch.returncode != 0 {
        log_docker_failure("run", &launch);
        return Err(LauncherError::Internal(fallback_message(
            &launch.stderr,
            format!("failed to start {}", request.name),
        )));
    }

    log_info(&format!(
        "started {} with image={} network={} staging={}",
        request.name, request.image, request.network, request.staging_path
    ));
    Ok("started")
}

pub fn teardown(
    name: &str,
    staging_path: &str,
    validators: &Validators,
) -> Result<(), LauncherError> {
    let safe_staging = validators.safe_staging_path(staging_path)?;

    let rm = run_command(&[
        "docker".to_string(),
        "rm".to_string(),
        "-f".to_string(),
        name.to_string(),
    ])
    .map_err(LauncherError::Internal)?;

    if rm.returncode != 0 && !is_no_such_container(&rm.stderr) {
        log_info(&format!(
            "docker rm -f {name} failed (non-fatal): {}",
            rm.stderr.trim()
        ));
    }

    for _ in 0..2 {
        let umount = run_command(&["umount".to_string(), safe_staging.clone()])
            .map_err(LauncherError::Internal)?;
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

pub(crate) fn is_no_such_object(stderr: &str) -> bool {
    stderr.to_lowercase().contains("no such object")
}

pub(crate) fn is_no_such_container(stderr: &str) -> bool {
    stderr.to_lowercase().contains("no such container")
}

fn log_docker_failure(subcommand: &str, output: &crate::cmd::CommandOutput) {
    log_info(&format!(
        "docker {subcommand} failed: rc={} stderr={}",
        output.returncode,
        output.stderr.trim()
    ));
}

fn docker_run_args(request: &ContainerLaunch<'_>) -> Vec<String> {
    let mut run_cmd = vec![
        "docker".to_string(),
        "run".to_string(),
        "-d".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        request.name.to_string(),
        "--network".to_string(),
        request.network.to_string(),
        "--network-alias".to_string(),
        request.name.to_string(),
        // This container runs untrusted plugin code; these flags are defence-in-depth — do not loosen.
        "--cap-drop=ALL".to_string(),
        "--security-opt=no-new-privileges".to_string(),
        "--pids-limit".to_string(),
        request.pids_limit.to_string(),
        "--memory".to_string(),
        request.memory_limit.to_string(),
    ];

    for (name, value) in request.env {
        debug_assert!(valid_env_name(name), "invalid env name: {name}");
        run_cmd.push("-e".to_string());
        run_cmd.push(format!("{name}={value}"));
    }

    if request.read_only_rootfs {
        // Rootfs writes are disabled only when explicitly requested because some plugins still need
        // writable runtime paths under /tmp or similar until they are cleaned up.
        run_cmd.push("--read-only".to_string());
    }

    if request.with_workspace {
        run_cmd.push("-v".to_string());
        run_cmd.push(format!("{}:/workspace:rslave", request.staging_path));
    }

    run_cmd.push("--user".to_string());
    run_cmd.push(format!("{}:{}", request.plugin_uid, request.plugin_gid));
    run_cmd.push(request.image.to_string());
    run_cmd
}

#[cfg(test)]
mod tests {
    use super::{docker_run_args, is_no_such_container, is_no_such_object, ContainerLaunch};

    #[test]
    fn docker_stderr_classification_matches_python_behavior() {
        assert!(is_no_such_object(
            "Error: No Such Object: mcp_session_aabbccddeeff"
        ));
        assert!(is_no_such_container(
            "Error response from daemon: No such container: abc"
        ));
        assert!(!is_no_such_object("permission denied"));
        assert!(!is_no_such_container("cannot connect to docker daemon"));
    }

    #[test]
    fn docker_run_args_include_sandbox_flags() {
        let args = docker_run_args(&ContainerLaunch {
            name: "mcp_session_aabbccddeeff",
            image: "botwork/mcp-echo:local",
            network: "botwork",
            staging_path: "/var/lib/botwork/tenants/acme/staging/aabbccddeeff",
            with_workspace: true,
            plugin_uid: 1000,
            plugin_gid: 1000,
            pids_limit: 256,
            memory_limit: "512m",
            read_only_rootfs: true,
            env: &[],
        });

        assert!(args.contains(&"--cap-drop=ALL".to_string()));
        assert!(args.contains(&"--security-opt=no-new-privileges".to_string()));
        assert!(args.windows(2).any(|pair| pair == ["--pids-limit", "256"]));
        assert!(args.windows(2).any(|pair| pair == ["--memory", "512m"]));
        assert!(!args.contains(&"-e".to_string()));
        assert!(args.contains(&"--read-only".to_string()));
        assert!(args.windows(2).any(|pair| pair == ["--user", "1000:1000"]));
        assert!(args.contains(
            &"/var/lib/botwork/tenants/acme/staging/aabbccddeeff:/workspace:rslave".to_string()
        ));
    }

    #[test]
    fn docker_run_args_includes_env_in_declaration_order() {
        let env = vec![
            ("FOO".to_string(), "one".to_string()),
            ("BAR".to_string(), "two".to_string()),
        ];
        let args = docker_run_args(&ContainerLaunch {
            name: "mcp_session_aabbccddeeff",
            image: "botwork/mcp-echo:local",
            network: "botwork",
            staging_path: "/var/lib/botwork/tenants/acme/staging/aabbccddeeff",
            with_workspace: false,
            plugin_uid: 1000,
            plugin_gid: 1000,
            pids_limit: 256,
            memory_limit: "512m",
            read_only_rootfs: false,
            env: &env,
        });

        let foo = args
            .windows(2)
            .position(|pair| pair == ["-e", "FOO=one"])
            .expect("FOO env");
        let bar = args
            .windows(2)
            .position(|pair| pair == ["-e", "BAR=two"])
            .expect("BAR env");
        let user = args
            .windows(2)
            .position(|pair| pair == ["--user", "1000:1000"])
            .expect("user flag");

        assert!(foo < bar);
        assert!(bar < user);
    }

    #[test]
    fn docker_run_args_env_values_with_special_chars_pass_through() {
        let env = vec![(
            "FOO".to_string(),
            "value with spaces and =equals=".to_string(),
        )];
        let args = docker_run_args(&ContainerLaunch {
            name: "mcp_session_aabbccddeeff",
            image: "botwork/mcp-echo:local",
            network: "botwork",
            staging_path: "/var/lib/botwork/tenants/acme/staging/aabbccddeeff",
            with_workspace: false,
            plugin_uid: 1000,
            plugin_gid: 1000,
            pids_limit: 256,
            memory_limit: "512m",
            read_only_rootfs: false,
            env: &env,
        });

        assert!(args
            .windows(2)
            .any(|pair| pair == ["-e", "FOO=value with spaces and =equals="]));
    }
}
