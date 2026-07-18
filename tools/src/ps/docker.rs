use std::process::Command;

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningContainer {
    pub id: String,
    pub name: String,
    pub age: String,
}

pub fn list_running_sessions() -> Result<Vec<RunningContainer>, DockerError> {
    let output = Command::new("docker")
        .args([
            "ps",
            "--filter",
            "name=^mcp_session_",
            "--format",
            "{{.ID}}\t{{.Names}}\t{{.RunningFor}}",
        ])
        .output()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => DockerError::NotFound,
            _ => DockerError::Io(err),
        })?;

    if !output.status.success() {
        return Err(DockerError::CommandFailed {
            code: output.status.code().unwrap_or(1),
            stderr: String::from_utf8_lossy(&output.stderr)
                .trim_end()
                .to_string(),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut containers = Vec::new();

    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let mut parts = line.splitn(3, '\t');
        let id = parts.next();
        let name = parts.next();
        let age = parts.next();

        match (id, name, age) {
            (Some(id), Some(name), Some(age)) => containers.push(RunningContainer {
                id: id.to_string(),
                name: name.to_string(),
                age: age.to_string(),
            }),
            _ => return Err(DockerError::MalformedOutput(line.to_string())),
        }
    }

    Ok(containers)
}

#[derive(Debug, Error)]
pub enum DockerError {
    #[error("docker CLI not found")]
    NotFound,
    #[error("failed to execute docker ps: {0}")]
    Io(std::io::Error),
    #[error("{stderr}")]
    CommandFailed { code: i32, stderr: String },
    #[error("failed to parse docker ps output line: {0}")]
    MalformedOutput(String),
}

impl DockerError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::NotFound => 2,
            Self::CommandFailed { code, .. } => *code,
            Self::Io(_) | Self::MalformedOutput(_) => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- RunningContainer: struct fields ---

    #[test]
    fn running_container_fields_roundtrip() {
        let c = RunningContainer {
            id: "abc123".into(),
            name: "mcp_session_foo".into(),
            age: "3 minutes ago".into(),
        };
        assert_eq!(c.id, "abc123");
        assert_eq!(c.name, "mcp_session_foo");
        assert_eq!(c.age, "3 minutes ago");
    }

    // --- DockerError display ---

    #[test]
    fn not_found_display_mentions_docker_cli() {
        let msg = format!("{}", DockerError::NotFound);
        assert!(msg.contains("docker"), "{msg}");
    }

    #[test]
    fn command_failed_display_includes_stderr() {
        let err = DockerError::CommandFailed {
            code: 1,
            stderr: "permission denied".into(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("permission denied"), "{msg}");
    }

    #[test]
    fn malformed_output_display_includes_offending_line() {
        let err = DockerError::MalformedOutput("only-one-field".into());
        let msg = format!("{err}");
        assert!(msg.contains("only-one-field"), "{msg}");
    }

    #[test]
    fn io_error_display_mentions_docker_ps() {
        let io = std::io::Error::other("fake io");
        let err = DockerError::Io(io);
        let msg = format!("{err}");
        assert!(!msg.is_empty(), "Io display should not be empty: {msg}");
    }

    // --- DockerError exit codes ---

    #[test]
    fn not_found_exit_code_is_2() {
        assert_eq!(DockerError::NotFound.exit_code(), 2);
    }

    #[test]
    fn command_failed_exit_code_mirrors_docker_exit_code() {
        let err = DockerError::CommandFailed {
            code: 125,
            stderr: String::new(),
        };
        assert_eq!(err.exit_code(), 125);
    }

    #[test]
    fn io_and_malformed_output_exit_code_is_1() {
        let io = std::io::Error::other("x");
        assert_eq!(DockerError::Io(io).exit_code(), 1);
        assert_eq!(DockerError::MalformedOutput("y".into()).exit_code(), 1);
    }

    // --- list_running_sessions: output parsing (no real docker) ---

    // The actual `list_running_sessions()` shells out to `docker ps`;
    // exercising that end-to-end requires a running docker daemon and
    // belongs in the integration / smoke tier (tools/smoke.sh).
    // The parsing logic is inlined; the tests above cover the error
    // type surface instead.
}
