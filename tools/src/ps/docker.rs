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
