use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::cmd::{log_info, run_command};
use crate::error::LauncherError;
use crate::validate::Validators;

pub fn setup_staging_dir(
    staging_path: &str,
    validators: &Validators,
    plugin_uid: u32,
    plugin_gid: u32,
) -> Result<(), LauncherError> {
    let safe_staging = validators.safe_staging_path(staging_path)?;

    fs::create_dir_all(&safe_staging).map_err(|err| {
        LauncherError::Internal(format!(
            "failed to create staging dir {safe_staging}: {err}"
        ))
    })?;
    fs::set_permissions(&safe_staging, fs::Permissions::from_mode(0o700)).map_err(|err| {
        LauncherError::Internal(format!(
            "failed to set permissions on {safe_staging}: {err}"
        ))
    })?;
    chown_path(&safe_staging, plugin_uid, plugin_gid)?;

    let bind = run_command(&[
        "mount".to_string(),
        "--bind".to_string(),
        safe_staging.clone(),
        safe_staging.clone(),
    ])
    .map_err(LauncherError::Internal)?;
    if bind.returncode != 0 {
        return Err(LauncherError::Internal(fallback_message(
            &bind.stderr,
            format!("failed to bind-mount staging dir {safe_staging}"),
        )));
    }

    let shared = run_command(&[
        "mount".to_string(),
        "--make-rshared".to_string(),
        safe_staging.clone(),
    ])
    .map_err(LauncherError::Internal)?;
    if shared.returncode != 0 {
        return Err(LauncherError::Internal(fallback_message(
            &shared.stderr,
            format!("failed to make staging dir rshared: {safe_staging}"),
        )));
    }

    log_info(&format!("staging dir ready: {safe_staging}"));
    Ok(())
}

pub fn bind_agent(
    staging_path: &str,
    agent_dir: &str,
    validators: &Validators,
    plugin_uid: u32,
    plugin_gid: u32,
) -> Result<(), LauncherError> {
    let safe_staging = validators.safe_staging_path(staging_path)?;
    let safe_agent = validators.safe_agent_dir(agent_dir)?;

    fs::create_dir_all(&safe_agent).map_err(|err| {
        LauncherError::Internal(format!("failed to create agent dir {safe_agent}: {err}"))
    })?;
    fs::set_permissions(&safe_agent, fs::Permissions::from_mode(0o700)).map_err(|err| {
        LauncherError::Internal(format!("failed to set permissions on {safe_agent}: {err}"))
    })?;
    chown_path(&safe_agent, plugin_uid, plugin_gid)?;

    let staging_stat = fs::metadata(&safe_staging).map_err(|err| {
        LauncherError::Internal(format!("failed to stat staging_path {safe_staging}: {err}"))
    })?;
    let agent_stat = fs::metadata(&safe_agent).map_err(|err| {
        LauncherError::Internal(format!("failed to stat agent_dir {safe_agent}: {err}"))
    })?;

    use std::os::unix::fs::MetadataExt;
    let staging_identity = (staging_stat.dev(), staging_stat.ino());
    let agent_identity = (agent_stat.dev(), agent_stat.ino());

    if staging_identity == agent_identity {
        log_info(&format!(
            "bind-agent: already bound {safe_staging} -> {safe_agent}"
        ));
        return Ok(());
    }

    let agents_parent = std::path::Path::new(&safe_agent).parent().ok_or_else(|| {
        LauncherError::Internal(format!("failed to determine parent for {safe_agent}"))
    })?;

    let entries = fs::read_dir(agents_parent).map_err(|err| {
        LauncherError::Internal(format!(
            "failed to list sibling agents in {}: {err}",
            agents_parent.to_string_lossy()
        ))
    })?;

    for entry in entries {
        let entry = entry.map_err(|err| {
            LauncherError::Internal(format!("failed to read sibling agent entry: {err}"))
        })?;
        let sibling_dir = entry.path();
        if sibling_dir == std::path::Path::new(&safe_agent) {
            continue;
        }

        let sibling_meta = match fs::metadata(&sibling_dir) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(LauncherError::Internal(format!(
                    "failed to stat sibling agent {}: {err}",
                    sibling_dir.to_string_lossy()
                )))
            }
        };

        let sibling_identity = (sibling_meta.dev(), sibling_meta.ino());
        if sibling_identity == staging_identity {
            return Err(LauncherError::Conflict(format!(
                "staging_path {safe_staging} already bound to {}, cannot rebind to {safe_agent}",
                sibling_dir.to_string_lossy()
            )));
        }
    }

    let bind = run_command(&[
        "mount".to_string(),
        "--bind".to_string(),
        safe_agent.clone(),
        safe_staging.clone(),
    ])
    .map_err(LauncherError::Internal)?;
    if bind.returncode != 0 {
        return Err(LauncherError::Internal(fallback_message(
            &bind.stderr,
            format!("failed to bind {safe_agent} onto {safe_staging}"),
        )));
    }

    log_info(&format!("bound agent dir {safe_agent} -> {safe_staging}"));
    Ok(())
}

pub(crate) fn is_not_mounted_or_einval(stderr: &str) -> bool {
    let stderr = stderr.to_lowercase();
    stderr.contains("not mounted")
        || stderr.contains("invalid argument")
        || stderr.contains("einval")
}

pub(crate) fn fallback_message(stderr: &str, fallback: String) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        fallback
    } else {
        trimmed.to_string()
    }
}

fn chown_path(path: &str, uid: u32, gid: u32) -> Result<(), LauncherError> {
    // `std::os::unix::fs::chown` is stable since 1.73 and wraps
    // `chown(2)` so we don't need an `unsafe` libc call here. `None`
    // would mean "don't touch this id"; we want both set.
    std::os::unix::fs::chown(Path::new(path), Some(uid), Some(gid)).map_err(|err| {
        LauncherError::Internal(format!("failed to chown {path} to {uid}:{gid}: {err}"))
    })
}

#[cfg(test)]
mod tests {
    use super::is_not_mounted_or_einval;

    #[test]
    fn umount_stderr_classification_matches_python_behavior() {
        assert!(is_not_mounted_or_einval("umount: /path: not mounted"));
        assert!(is_not_mounted_or_einval("umount: /path: Invalid argument"));
        assert!(is_not_mounted_or_einval("EInVaL"));
        assert!(!is_not_mounted_or_einval("permission denied"));
    }
}
