use std::fs;

use crate::cmd::{log_info, run_command, run_command_with_stdin};
use crate::error::LauncherError;
use crate::mount::{fallback_message, is_not_mounted_or_einval, setup_staging_dir};
use crate::validate::{is_sensitive_env, valid_env_name, Validators};

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
}

/// Outcome of `ensure_container`: lifecycle status plus the container's
/// IPv4 address on its docker network.
///
/// `status` is the same machine-readable shape session-broker already
/// consumes today (`"already_running"` or `"started"`); `container_ip` is
/// new in 0.1.5 -- session-broker forwards it to control-plane
/// (botwork #81) as part of the per-session hard-gate POST.
///
/// `container_ip` is **always populated on success** by an inspect call
/// chained after `docker run`. We treat a missing/unparseable IP as a
/// launch failure (the launcher never returns 200 with an unknown
/// address): every downstream that uses this shape requires it, and a
/// silently-empty IP would surface as a confusing control-plane 400
/// hours later instead of an immediate launcher 500.
pub struct LaunchOutcome {
    pub status: &'static str,
    pub container_ip: String,
}

pub fn ensure_container(
    request: &ContainerLaunch<'_>,
    validators: &Validators,
) -> Result<LaunchOutcome, LauncherError> {
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
            // Reuse the existing container's address rather than tear it
            // down and re-spawn; downstream consumers (control-plane gate)
            // require an IP either way. An unparseable inspect here is a
            // launch failure -- same posture as a fresh spawn.
            let container_ip = inspect_container_ip(request.network, request.name)?;
            log_info(&format!(
                "{} already running (ip={container_ip})",
                request.name
            ));
            return Ok(LaunchOutcome {
                status: "already_running",
                container_ip,
            });
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
    let stdin_bytes = sensitive_env_stdin(request.env);

    let launch = if stdin_bytes.is_empty() {
        run_command(&run_cmd).map_err(LauncherError::Internal)?
    } else {
        run_command_with_stdin(&run_cmd, &stdin_bytes).map_err(LauncherError::Internal)?
    };
    if launch.returncode != 0 {
        log_docker_failure("run", &launch);
        return Err(LauncherError::Internal(fallback_message(
            &launch.stderr,
            format!("failed to start {}", request.name),
        )));
    }

    // `docker run` exits as soon as the container is started; the IPAM
    // address is assigned synchronously by docker's userland networking
    // layer, so the immediate inspect call below is race-free in
    // practice. If a future driver makes this async we'll need to retry
    // -- but failing closed on missing IP is still the right posture.
    let container_ip = inspect_container_ip(request.network, request.name)?;

    log_info(&format!(
        "started {} with image={} network={} staging={} ip={}",
        request.name, request.image, request.network, request.staging_path, container_ip
    ));
    Ok(LaunchOutcome {
        status: "started",
        container_ip,
    })
}

/// Asks docker for the container's IPv4 address on `network`.
///
/// Returns `LauncherError::Internal` when the inspect exits non-zero, when
/// the IP is empty (container not attached to that network), or when it
/// doesn't parse as IPv4. The launcher never returns 200 with an unknown
/// IP -- see `LaunchOutcome` for the rationale.
fn inspect_container_ip(network: &str, name: &str) -> Result<String, LauncherError> {
    let format = inspect_ip_format(network)?;
    let inspect = run_command(&[
        "docker".to_string(),
        "inspect".to_string(),
        "--format".to_string(),
        format,
        name.to_string(),
    ])
    .map_err(LauncherError::Internal)?;

    if inspect.returncode != 0 {
        log_docker_failure("inspect-ip", &inspect);
        return Err(LauncherError::Internal(fallback_message(
            &inspect.stderr,
            format!("failed to inspect IP for {name} on {network}"),
        )));
    }

    let ip = inspect.stdout.trim().to_string();
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

/// Builds the `--format` string passed to `docker inspect` to read the
/// container's IPv4 on `network`.
///
/// Go `text/template` parses identifiers as
/// `[a-zA-Z_][a-zA-Z0-9_]*`, with `-` interpreted as the subtraction
/// operator. That means the dotted shape
/// `{{.NetworkSettings.Networks.<net>.IPAddress}}` only works for
/// networks whose names don't contain a hyphen. The default in
/// production is `botwork-plugin`, which trips this immediately:
///
/// ```text
/// template parsing error: template: :1: bad character U+002D '-'
/// ```
///
/// The fix is to look up the network via the template `index` builtin,
/// which takes the map key as a string literal and so escapes the
/// identifier grammar entirely:
///
/// ```text
/// {{(index .NetworkSettings.Networks "botwork-plugin").IPAddress}}
/// ```
///
/// `network` is still validated against a conservative character class
/// because it is interpolated into the format string -- a `"` in the
/// name would break out of the string literal and let an attacker
/// inject arbitrary template actions.
fn inspect_ip_format(network: &str) -> Result<String, LauncherError> {
    if !network
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        // Defence in depth: validators already accept the launcher's
        // configured default network, but ensure_container's caller could
        // be passing a payload-supplied value too. Refuse anything that
        // could escape the Go template grammar (in particular `"`,
        // which would close the string literal below).
        return Err(LauncherError::Internal(format!(
            "invalid network name '{network}' for inspect"
        )));
    }

    Ok(format!(
        r#"{{{{(index .NetworkSettings.Networks "{network}").IPAddress}}}}"#
    ))
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
        "--cpus".to_string(),
        request.cpu_limit.to_string(),
    ];

    for (name, value) in request.env {
        debug_assert!(valid_env_name(name), "invalid env name: {}", name);
        if is_sensitive_env(name) {
            // Sensitive vars are routed via stdin (--env-file /dev/stdin); never on argv.
            continue;
        }
        run_cmd.push("-e".to_string());
        run_cmd.push(format!("{name}={value}"));
    }

    if request.env.iter().any(|(name, _)| is_sensitive_env(name)) {
        // Docker reads the env-file from its own stdin, which the launcher pipes.
        run_cmd.push("--env-file".to_string());
        run_cmd.push("/dev/stdin".to_string());
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

/// Builds the `--env-file` content for sensitive env vars: one `NAME=VALUE\n`
/// line per entry.  Returns an empty `Vec` when there are no sensitive vars,
/// in which case the caller should use regular `run_command` (no stdin pipe).
fn sensitive_env_stdin(env: &[(String, String)]) -> Vec<u8> {
    let mut content = Vec::new();
    for (name, value) in env {
        if is_sensitive_env(name) {
            content.extend_from_slice(name.as_bytes());
            content.push(b'=');
            content.extend_from_slice(value.as_bytes());
            content.push(b'\n');
        }
    }
    content
}

#[cfg(test)]
mod tests {
    use super::{
        docker_run_args, inspect_ip_format, is_no_such_container, is_no_such_object,
        sensitive_env_stdin, ContainerLaunch,
    };

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
            cpu_limit: "1.0",
            memory_limit: "512m",
            read_only_rootfs: true,
            env: &[],
        });

        assert!(args.contains(&"--cap-drop=ALL".to_string()));
        assert!(args.contains(&"--security-opt=no-new-privileges".to_string()));
        assert!(args.windows(2).any(|pair| pair == ["--pids-limit", "256"]));
        assert!(args.windows(2).any(|pair| pair == ["--memory", "512m"]));
        assert!(args.windows(2).any(|pair| pair == ["--cpus", "1.0"]));
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
            cpu_limit: "1.0",
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
            cpu_limit: "1.0",
            memory_limit: "512m",
            read_only_rootfs: false,
            env: &env,
        });

        assert!(args
            .windows(2)
            .any(|pair| pair == ["-e", "FOO=value with spaces and =equals="]));
    }

    #[test]
    fn sensitive_env_not_on_argv_and_env_file_flag_added() {
        let env = vec![
            ("FOO".to_string(), "plain".to_string()),
            ("BOTWORK_SECRET_TOKEN".to_string(), "s3cr3t".to_string()),
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
            cpu_limit: "1.0",
            memory_limit: "512m",
            read_only_rootfs: false,
            env: &env,
        });

        // Secret value must never appear anywhere in argv.
        assert!(
            !args.iter().any(|a| a.contains("s3cr3t")),
            "secret value leaked onto argv"
        );
        // Secret name must not appear as -e NAME=VALUE.
        assert!(
            !args
                .windows(2)
                .any(|pair| pair[0] == "-e" && pair[1].starts_with("BOTWORK_SECRET_")),
            "secret name leaked via -e onto argv"
        );
        // Plain env must still be on argv.
        assert!(
            args.windows(2).any(|pair| pair == ["-e", "FOO=plain"]),
            "plain env missing from argv"
        );
        // --env-file /dev/stdin must be present.
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--env-file", "/dev/stdin"]),
            "--env-file /dev/stdin missing"
        );
    }

    #[test]
    fn sensitive_env_stdin_content_is_correct() {
        let env = vec![
            ("FOO".to_string(), "plain".to_string()),
            ("BOTWORK_SECRET_A".to_string(), "alpha".to_string()),
            ("BOTWORK_SECRET_B".to_string(), "beta=value".to_string()),
        ];
        let content = sensitive_env_stdin(&env);
        let text = std::str::from_utf8(&content).expect("utf8");

        // Only sensitive vars should appear in the stdin content.
        assert!(!text.contains("FOO"), "plain env leaked into stdin content");
        assert!(text.contains("BOTWORK_SECRET_A=alpha\n"));
        assert!(text.contains("BOTWORK_SECRET_B=beta=value\n"));
    }

    #[test]
    fn no_env_file_flag_when_no_sensitive_env() {
        let env = vec![("FOO".to_string(), "bar".to_string())];
        let args = docker_run_args(&ContainerLaunch {
            name: "mcp_session_aabbccddeeff",
            image: "botwork/mcp-echo:local",
            network: "botwork",
            staging_path: "/var/lib/botwork/tenants/acme/staging/aabbccddeeff",
            with_workspace: false,
            plugin_uid: 1000,
            plugin_gid: 1000,
            pids_limit: 256,
            cpu_limit: "1.0",
            memory_limit: "512m",
            read_only_rootfs: false,
            env: &env,
        });

        assert!(
            !args.contains(&"--env-file".to_string()),
            "--env-file must not appear when there is no sensitive env"
        );
        let stdin = sensitive_env_stdin(&env);
        assert!(
            stdin.is_empty(),
            "stdin bytes must be empty for non-sensitive env"
        );
    }

    // ── inspect_ip_format ──────────────────────────────────────────────────
    //
    // The shape of the `--format` string passed to `docker inspect` is wire
    // contract between launcher and the docker CLI. Go `text/template`
    // parses identifiers as `[a-zA-Z_][a-zA-Z0-9_]*` and reads `-` as the
    // subtraction operator, so the dotted form
    // `{{.NetworkSettings.Networks.<net>.IPAddress}}` blows up at parse
    // time for any network with a hyphen -- including our production
    // `botwork-plugin`. We use the `index` builtin (which takes the key
    // as a quoted string literal) to escape the identifier grammar.

    #[test]
    fn inspect_ip_format_uses_index_for_hyphenated_network() {
        // Production case: this is the format string that blew up
        // before the fix.
        let format = inspect_ip_format("botwork-plugin").expect("valid network");
        assert_eq!(
            format,
            r#"{{(index .NetworkSettings.Networks "botwork-plugin").IPAddress}}"#
        );
    }

    #[test]
    fn inspect_ip_format_uses_index_for_simple_network() {
        // Identifier-safe names go through the same `index` shape rather
        // than the dotted form so both code paths use one template
        // grammar -- no second variant to drift.
        let format = inspect_ip_format("bridge").expect("valid network");
        assert_eq!(
            format,
            r#"{{(index .NetworkSettings.Networks "bridge").IPAddress}}"#
        );
    }

    #[test]
    fn inspect_ip_format_rejects_double_quote() {
        // `"` would close the string literal in the `index ...` call and
        // let a payload inject arbitrary template actions. The validator
        // accepts only a conservative character class; everything else
        // (including `"`, `{`, `}`, spaces) is refused at this layer.
        let err = inspect_ip_format(r#"x"y"#).expect_err("must reject quote");
        let crate::error::LauncherError::Internal(msg) = err else {
            panic!("expected Internal, got: {err:?}");
        };
        assert!(msg.contains("invalid network name"), "{msg}");
    }

    #[test]
    fn inspect_ip_format_rejects_curly_braces() {
        // Same posture as the quote case -- defence in depth against any
        // future caller that lets a network name slip through.
        for bad in ["{x", "x}", "x{y}", " "] {
            assert!(
                inspect_ip_format(bad).is_err(),
                "must reject network name {bad:?}"
            );
        }
    }

    #[test]
    fn inspect_ip_format_accepts_alphanumeric_dot_dash_underscore() {
        // Mirror the validator's accepted class so a future tightening
        // of the validator is caught here too.
        for ok in ["a", "ABC", "x1", "x-y", "x_y", "x.y", "x-y_z.0"] {
            assert!(
                inspect_ip_format(ok).is_ok(),
                "must accept network name {ok:?}"
            );
        }
    }
}
