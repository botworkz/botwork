pub mod docker;
pub mod registry;
pub mod render;

use std::path::Path;

use thiserror::Error;

use crate::ps::docker::DockerError;
use crate::ps::registry::RegistryError;

const DEFAULT_REGISTRY_PATH: &str = "/var/lib/botwork/sessions.json";
const NO_REGISTRY_MESSAGE: &str =
    "No session registry at {path} (broker not running or never bound a session).";

pub fn run(args: &[String]) -> Result<(), PsError> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_usage();
        return Ok(());
    }

    if !args.is_empty() {
        return Err(PsError::InvalidUsage);
    }

    let registry_path = std::env::var("BOTWORK_SESSION_REGISTRY_PATH")
        .unwrap_or_else(|_| DEFAULT_REGISTRY_PATH.to_string());

    let registry = match registry::load_registry(Path::new(&registry_path)) {
        Ok(data) => data,
        Err(RegistryError::Io(_)) => {
            eprintln!("{}", NO_REGISTRY_MESSAGE.replace("{path}", &registry_path));
            return Ok(());
        }
        Err(err) => return Err(PsError::Registry(err)),
    };

    let running = docker::list_running_sessions()?;
    let mut rows = Vec::with_capacity(running.len());

    for container in running {
        let (agent, image) = match registry.sessions.get(&container.name) {
            Some(entry) => (
                entry
                    .agent_id
                    .clone()
                    .unwrap_or_else(|| "(unbound)".to_string()),
                entry.image.clone(),
            ),
            None => ("(unregistered)".to_string(), "?".to_string()),
        };

        rows.push(render::TableRow {
            id: container.id,
            container: container.name,
            agent,
            image,
            age: container.age,
        });
    }

    print!("{}", render::render_table(&rows));
    Ok(())
}

fn print_usage() {
    println!("Usage: botwork-tools ps");
}

#[derive(Debug, Error)]
pub enum PsError {
    #[error("usage: botwork-tools ps")]
    InvalidUsage,
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Docker(#[from] DockerError),
}

impl PsError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::InvalidUsage => 2,
            Self::Registry(_) => 1,
            Self::Docker(err) => err.exit_code(),
        }
    }
}
