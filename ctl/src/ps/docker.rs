//! Bollard-based Docker container listing for `botctl ps`.
//!
//! The `DockerApi` trait provides a seam for offline testing.  The only
//! production-unreachable parts are `impl DockerApi for Docker` (the thin
//! bollard wrapper) and `connect_docker` (requires the docker socket).

use std::collections::HashMap;

use bollard::errors::Error as BollardError;
use bollard::models::ContainerSummary;
use bollard::query_parameters::ListContainersOptionsBuilder;
use bollard::Docker;
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Output type (unchanged public contract)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningContainer {
    pub id: String,
    pub name: String,
    pub age: String,
}

// ---------------------------------------------------------------------------
// DockerApi seam
// ---------------------------------------------------------------------------

pub(crate) trait DockerApi {
    fn list_containers<'a>(
        &'a self,
        filters: HashMap<String, Vec<String>>,
    ) -> BoxFuture<'a, Result<Vec<ContainerSummary>, BollardError>>;
}

/// Production implementation — connects over the local docker socket.
/// NOT covered by offline unit tests.
#[cfg(not(tarpaulin_include))]
impl DockerApi for Docker {
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
}

/// Connect to the local docker socket.
/// NOT covered by offline unit tests.
#[cfg(not(tarpaulin_include))]
pub(crate) fn connect_docker() -> Result<Docker, BollardError> {
    Docker::connect_with_local_defaults()
}

// ---------------------------------------------------------------------------
// Session-container filter
// ---------------------------------------------------------------------------

fn session_filters() -> HashMap<String, Vec<String>> {
    let mut filters = HashMap::new();
    filters.insert("name".to_string(), vec!["mcp_session_".to_string()]);
    filters.insert("status".to_string(), vec!["running".to_string()]);
    filters
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// List all running `mcp_session_*` containers via the docker socket.
///
/// Blocks internally by driving the async bollard call on a
/// single-threaded tokio runtime.
pub fn list_running_sessions() -> Result<Vec<RunningContainer>, DockerError> {
    let docker = connect_docker().map_err(|e| DockerError::Api(e.to_string()))?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| DockerError::Api(format!("failed to create tokio runtime: {e}")))?;
    rt.block_on(list_running_sessions_impl(&docker))
}

async fn list_running_sessions_impl<D: DockerApi + ?Sized>(
    docker: &D,
) -> Result<Vec<RunningContainer>, DockerError> {
    let summaries = docker
        .list_containers(session_filters())
        .await
        .map_err(bollard_to_docker_error)?;

    let mut containers = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let id = summary
            .id
            .as_deref()
            .map(|s| s.chars().take(12).collect())
            .unwrap_or_default();

        let name = summary
            .names
            .as_deref()
            .and_then(|ns| ns.first())
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_default();

        let age = summary.status.unwrap_or_default();

        containers.push(RunningContainer { id, name, age });
    }
    Ok(containers)
}

/// Map a bollard error to a [`DockerError`].
///
/// An IO error with `NotFound` or `ConnectionRefused` kind is reported as
/// [`DockerError::NotFound`] (socket unreachable).  All other errors map to
/// [`DockerError::Api`].
fn bollard_to_docker_error(e: BollardError) -> DockerError {
    if let BollardError::IOError { err } = &e {
        if matches!(
            err.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        ) {
            return DockerError::NotFound;
        }
    }
    DockerError::Api(e.to_string())
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum DockerError {
    #[error("docker socket not reachable")]
    NotFound,
    #[error("docker API error: {0}")]
    Api(String),
}

impl DockerError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::NotFound => 2,
            Self::Api(_) => 1,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    type ListQueue = Arc<Mutex<VecDeque<Result<Vec<ContainerSummary>, BollardError>>>>;

    // FakeDocker returns pre-canned responses for offline testing.
    // The production `impl DockerApi for Docker` and `connect_docker` are
    // the only offline-unreachable parts.
    #[derive(Default, Clone)]
    struct FakeDocker {
        list_results: ListQueue,
    }

    impl FakeDocker {
        fn with_list(self, results: Vec<Result<Vec<ContainerSummary>, BollardError>>) -> Self {
            *self.list_results.lock().expect("list lock") = VecDeque::from(results);
            self
        }
    }

    impl DockerApi for FakeDocker {
        fn list_containers<'a>(
            &'a self,
            _filters: HashMap<String, Vec<String>>,
        ) -> BoxFuture<'a, Result<Vec<ContainerSummary>, BollardError>> {
            async move {
                self.list_results
                    .lock()
                    .expect("list lock")
                    .pop_front()
                    .expect("missing list result")
            }
            .boxed()
        }
    }

    fn summary(id: &str, name: &str, status: &str) -> ContainerSummary {
        ContainerSummary {
            id: Some(id.to_string()),
            names: Some(vec![format!("/{name}")]),
            status: Some(status.to_string()),
            ..Default::default()
        }
    }

    // --- RunningContainer struct ---

    #[test]
    fn running_container_fields_roundtrip() {
        let c = RunningContainer {
            id: "abc123".into(),
            name: "mcp_session_foo".into(),
            age: "Up 3 minutes".into(),
        };
        assert_eq!(c.id, "abc123");
        assert_eq!(c.name, "mcp_session_foo");
        assert_eq!(c.age, "Up 3 minutes");
    }

    // --- DockerError display and exit codes ---

    #[test]
    fn not_found_display_mentions_docker() {
        let msg = format!("{}", DockerError::NotFound);
        assert!(msg.contains("docker"), "{msg}");
    }

    #[test]
    fn api_error_display_includes_message() {
        let err = DockerError::Api("permission denied".into());
        let msg = format!("{err}");
        assert!(msg.contains("permission denied"), "{msg}");
    }

    #[test]
    fn not_found_exit_code_is_2() {
        assert_eq!(DockerError::NotFound.exit_code(), 2);
    }

    #[test]
    fn api_error_exit_code_is_1() {
        assert_eq!(DockerError::Api("x".into()).exit_code(), 1);
    }

    // --- list_running_sessions_impl via FakeDocker ---

    #[tokio::test]
    async fn multiple_containers_are_mapped() {
        let docker = FakeDocker::default().with_list(vec![Ok(vec![
            summary("aabbccddeeff00112233", "mcp_session_alpha", "Up 5 minutes"),
            summary("bbccddee00112233aabb", "mcp_session_beta", "Up 2 minutes"),
        ])]);
        let result = list_running_sessions_impl(&docker).await.expect("ok");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, "aabbccddeeff");
        assert_eq!(result[0].name, "mcp_session_alpha");
        assert_eq!(result[0].age, "Up 5 minutes");
        assert_eq!(result[1].id, "bbccddee0011");
        assert_eq!(result[1].name, "mcp_session_beta");
    }

    #[tokio::test]
    async fn empty_list_returns_empty_vec() {
        let docker = FakeDocker::default().with_list(vec![Ok(vec![])]);
        let result = list_running_sessions_impl(&docker).await.expect("ok");
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn container_with_missing_optional_fields_uses_defaults() {
        let docker = FakeDocker::default().with_list(vec![Ok(vec![ContainerSummary {
            id: None,
            names: None,
            status: None,
            ..Default::default()
        }])]);
        let result = list_running_sessions_impl(&docker).await.expect("ok");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "");
        assert_eq!(result[0].name, "");
        assert_eq!(result[0].age, "");
    }

    #[tokio::test]
    async fn api_error_propagates() {
        let docker =
            FakeDocker::default().with_list(vec![Err(BollardError::DockerResponseServerError {
                status_code: 500,
                message: "daemon error".into(),
            })]);
        let err = list_running_sessions_impl(&docker).await.unwrap_err();
        assert!(matches!(err, DockerError::Api(_)));
        let msg = format!("{err}");
        assert!(msg.contains("daemon error") || msg.contains("500"), "{msg}");
    }

    #[tokio::test]
    async fn id_is_truncated_to_12_chars() {
        let long_id = "a".repeat(64);
        let docker = FakeDocker::default().with_list(vec![Ok(vec![summary(
            &long_id,
            "mcp_session_x",
            "Up 1 minute",
        )])]);
        let result = list_running_sessions_impl(&docker).await.expect("ok");
        assert_eq!(result[0].id.len(), 12);
        assert_eq!(result[0].id, "a".repeat(12));
    }

    #[tokio::test]
    async fn name_slash_prefix_is_stripped() {
        let docker = FakeDocker::default().with_list(vec![Ok(vec![ContainerSummary {
            id: Some("abc".to_string()),
            names: Some(vec!["/mcp_session_stripped".to_string()]),
            status: Some("Up 1 second".to_string()),
            ..Default::default()
        }])]);
        let result = list_running_sessions_impl(&docker).await.expect("ok");
        assert_eq!(result[0].name, "mcp_session_stripped");
    }

    #[tokio::test]
    async fn socket_io_error_maps_to_not_found() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let docker =
            FakeDocker::default().with_list(vec![Err(BollardError::IOError { err: io_err })]);
        let err = list_running_sessions_impl(&docker).await.unwrap_err();
        assert!(
            matches!(err, DockerError::NotFound),
            "expected NotFound, got {err:?}"
        );
    }
}
