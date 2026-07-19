//! Bollard-based Docker API used by the broker's hot path and recovery.
//!
//! The `DockerApi` trait provides a seam for offline testing.  The only
//! production-unreachable parts are `impl DockerApi for Docker` (the thin
//! bollard wrapper) and `connect_docker` (requires the docker socket).

use std::collections::HashMap;

use bollard::errors::Error as BollardError;
use bollard::models::{ContainerInspectResponse, ContainerStateStatusEnum, ContainerSummary};
use bollard::query_parameters::{
    InspectContainerOptionsBuilder, ListContainersOptionsBuilder, RemoveContainerOptionsBuilder,
};
use bollard::Docker;
use futures_util::future::BoxFuture;
use futures_util::FutureExt;

use crate::log_info;

// ---------------------------------------------------------------------------
// DockerApi seam
// ---------------------------------------------------------------------------

pub(crate) trait DockerApi {
    fn inspect_container<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<ContainerInspectResponse, BollardError>>;

    fn list_containers<'a>(
        &'a self,
        filters: HashMap<String, Vec<String>>,
    ) -> BoxFuture<'a, Result<Vec<ContainerSummary>, BollardError>>;

    fn remove_container<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), BollardError>>;
}

/// Production implementation — connects over the local docker socket.
/// NOT covered by offline unit tests.
impl DockerApi for Docker {
    fn inspect_container<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<ContainerInspectResponse, BollardError>> {
        let options = Some(InspectContainerOptionsBuilder::new().size(false).build());
        Docker::inspect_container(self, name, options).boxed()
    }

    fn list_containers<'a>(
        &'a self,
        filters: HashMap<String, Vec<String>>,
    ) -> BoxFuture<'a, Result<Vec<ContainerSummary>, BollardError>> {
        let options = Some(
            ListContainersOptionsBuilder::new()
                .filters(&filters)
                .build(),
        );
        Docker::list_containers(self, options).boxed()
    }

    fn remove_container<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), BollardError>> {
        let options = Some(RemoveContainerOptionsBuilder::new().force(true).build());
        Docker::remove_container(self, name, options).boxed()
    }
}

/// Connect to the local docker socket.
/// NOT covered by offline unit tests.
pub(crate) fn connect_docker() -> Result<Docker, BollardError> {
    Docker::connect_with_local_defaults()
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Checks whether a specific container is currently running.
///
/// Returns `Some(true)` if running, `Some(false)` if the container
/// exists but is not running OR does not exist, `None` when the
/// docker socket is unavailable or an unexpected error occurs. The
/// "unknown" answer is mapped to "assume alive" upstream so a
/// transient docker hiccup doesn't cause a false-positive eviction.
pub async fn is_container_running(name: &str) -> Option<bool> {
    match connect_docker() {
        Err(e) => {
            log_info(&format!(
                "docker socket unavailable; skipping liveness check: {e}"
            ));
            None
        }
        Ok(docker) => is_container_running_impl(name, &docker).await,
    }
}

async fn is_container_running_impl<D: DockerApi + ?Sized>(name: &str, docker: &D) -> Option<bool> {
    match docker.inspect_container(name).await {
        Ok(inspect) => Some(is_running(&inspect)),
        Err(BollardError::DockerResponseServerError {
            status_code: 404, ..
        }) => Some(false),
        Err(e) => {
            log_info(&format!("docker inspect failed for {name}: {e}"));
            None
        }
    }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::ContainerState;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    // FakeDocker returns pre-canned responses for offline testing.
    // The production `impl DockerApi for Docker` and `connect_docker` are
    // the only offline-unreachable parts.
    #[derive(Default, Clone)]
    struct FakeDocker {
        inspect_results: Arc<Mutex<VecDeque<Result<ContainerInspectResponse, BollardError>>>>,
    }

    impl FakeDocker {
        fn with_inspect(
            self,
            results: Vec<Result<ContainerInspectResponse, BollardError>>,
        ) -> Self {
            *self.inspect_results.lock().expect("inspect lock") = VecDeque::from(results);
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

        fn list_containers<'a>(
            &'a self,
            _filters: HashMap<String, Vec<String>>,
        ) -> BoxFuture<'a, Result<Vec<ContainerSummary>, BollardError>> {
            unimplemented!("list_containers not used in docker.rs tests")
        }

        fn remove_container<'a>(
            &'a self,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<(), BollardError>> {
            unimplemented!("remove_container not used in docker.rs tests")
        }
    }

    fn running_inspect() -> ContainerInspectResponse {
        ContainerInspectResponse {
            state: Some(ContainerState {
                status: Some(ContainerStateStatusEnum::RUNNING),
                running: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn stopped_inspect() -> ContainerInspectResponse {
        ContainerInspectResponse {
            state: Some(ContainerState {
                status: Some(ContainerStateStatusEnum::EXITED),
                running: Some(false),
                ..Default::default()
            }),
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

    #[tokio::test]
    async fn running_container_returns_some_true() {
        let docker = FakeDocker::default().with_inspect(vec![Ok(running_inspect())]);
        let result = is_container_running_impl("mcp_session_abc", &docker).await;
        assert_eq!(result, Some(true));
    }

    #[tokio::test]
    async fn stopped_container_returns_some_false() {
        let docker = FakeDocker::default().with_inspect(vec![Ok(stopped_inspect())]);
        let result = is_container_running_impl("mcp_session_abc", &docker).await;
        assert_eq!(result, Some(false));
    }

    #[tokio::test]
    async fn not_found_returns_some_false() {
        let docker = FakeDocker::default().with_inspect(vec![Err(err_404())]);
        let result = is_container_running_impl("mcp_session_abc", &docker).await;
        assert_eq!(result, Some(false));
    }

    #[tokio::test]
    async fn api_error_returns_none() {
        let docker = FakeDocker::default().with_inspect(vec![Err(err_500())]);
        let result = is_container_running_impl("mcp_session_abc", &docker).await;
        assert_eq!(result, None);
    }

    #[test]
    fn is_running_with_running_status_returns_true() {
        let inspect = running_inspect();
        assert!(is_running(&inspect));
    }

    #[test]
    fn is_running_with_exited_status_returns_false() {
        let inspect = stopped_inspect();
        assert!(!is_running(&inspect));
    }

    #[test]
    fn is_running_with_no_state_returns_false() {
        let inspect = ContainerInspectResponse {
            state: None,
            ..Default::default()
        };
        assert!(!is_running(&inspect));
    }

    #[test]
    fn is_running_with_running_true_but_no_status_returns_true() {
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
}
