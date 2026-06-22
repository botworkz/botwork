//! Thin wrappers around the docker CLI used by the broker's hot path.
//!
//! Round-3 cutover (RFE #105 PR2) shrank `session_registry.rs` down to
//! these two helpers; the rest of that module's docker-CLI surface
//! lives in `recovery.rs` (cold-start only). This file is just the
//! per-request hook that `evict_dead_session` uses to confirm a
//! container is still up before declaring a session dead.

use crate::log_info;

/// Checks whether a specific container is currently running.
///
/// Returns `Some(true)` if running, `Some(false)` if the container
/// exists but is not running OR does not exist, `None` when the
/// docker CLI is unavailable. The "unknown" answer is mapped to
/// "assume alive" upstream so a transient docker hiccup doesn't
/// cause a false-positive eviction.
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
            // Container not known to docker — same outcome as
            // "exists but stopped". Caller treats both as "evict".
            Some(false)
        }
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Some(stdout == "true")
        }
    }
}
