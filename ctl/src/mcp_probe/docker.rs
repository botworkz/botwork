//! Shared bollard-based Docker client seam for `ctl/src/mcp_probe/`.
//!
//! Mirrors the `DockerApi` / `connect_docker` / `FakeDocker` pattern
//! from `ctl/src/ps/docker.rs` and `launcher/src/docker.rs`.  Only the
//! bollard methods actually used by the three `mcp_probe` sub-modules
//! (`patch`, `verify`, `probe`) are included.
//!
//! The thin `impl DockerApi for Docker` wrappers and `connect_docker`
//! are NOT covered by offline unit tests (they require the local docker
//! socket).

use std::collections::HashMap;

use bollard::errors::Error as BollardError;
use bollard::models::{ContainerConfig, ContainerCreateBody, ImageInspect};
use bollard::query_parameters::{CommitContainerOptionsBuilder, RemoveContainerOptionsBuilder};
use bollard::Docker;
use futures_util::future::BoxFuture;
use futures_util::FutureExt;

// ---------------------------------------------------------------------------
// Seam
// ---------------------------------------------------------------------------

pub(crate) trait DockerApi {
    /// Inspect a local image by name/tag.
    ///
    /// Returns `BollardError::DockerResponseServerError { status_code: 404 }`
    /// when the image does not exist in the local store.
    fn inspect_image<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<ImageInspect, BollardError>>;

    /// Create a container from `config` (not started).  Returns the container
    /// ID assigned by the daemon.
    fn create_container<'a>(
        &'a self,
        config: ContainerCreateBody,
    ) -> BoxFuture<'a, Result<String, BollardError>>;

    /// Start a previously-created container.
    fn start_container<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), BollardError>>;

    /// Force-remove a container (stopped or running).
    fn remove_container<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), BollardError>>;

    /// Commit `container_id` to a new local image at `repo:tag`, setting the
    /// image config labels to exactly the supplied map.
    ///
    /// Note: the docker commit endpoint *replaces* the image's label set with
    /// whatever is passed here — it does not merge with the source image's
    /// existing labels. Callers that need crane-style merge semantics (e.g.
    /// `patch::patch_image`) must inspect the source image and pass the merged
    /// union; this seam commits verbatim what it is handed.
    ///
    /// Returns the new image ID.
    fn commit_container<'a>(
        &'a self,
        container_id: &'a str,
        repo: &'a str,
        tag: &'a str,
        labels: HashMap<String, String>,
    ) -> BoxFuture<'a, Result<String, BollardError>>;
}

// ---------------------------------------------------------------------------
// Production implementation
// ---------------------------------------------------------------------------

/// Production implementation — connects over the local docker socket.
/// NOT covered by offline unit tests.
impl DockerApi for Docker {
    fn inspect_image<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<ImageInspect, BollardError>> {
        Docker::inspect_image(self, name).boxed()
    }

    fn create_container<'a>(
        &'a self,
        config: ContainerCreateBody,
    ) -> BoxFuture<'a, Result<String, BollardError>> {
        async move {
            let result = Docker::create_container(self, None, config).await?;
            Ok(result.id)
        }
        .boxed()
    }

    fn start_container<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), BollardError>> {
        Docker::start_container(self, id, None).boxed()
    }

    fn remove_container<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), BollardError>> {
        let options = Some(RemoveContainerOptionsBuilder::new().force(true).build());
        Docker::remove_container(self, id, options).boxed()
    }

    fn commit_container<'a>(
        &'a self,
        container_id: &'a str,
        repo: &'a str,
        tag: &'a str,
        labels: HashMap<String, String>,
    ) -> BoxFuture<'a, Result<String, BollardError>> {
        let options = CommitContainerOptionsBuilder::new()
            .container(container_id)
            .repo(repo)
            .tag(tag)
            .build();
        let config = ContainerConfig {
            labels: Some(labels),
            ..Default::default()
        };
        async move {
            let result = Docker::commit_container(self, options, config).await?;
            // The docker daemon's commit endpoint always returns the new image's
            // SHA256 digest as a non-empty string; we propagate it as-is.
            Ok(result.id)
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// Connection helper
// ---------------------------------------------------------------------------

/// Connect to the local docker socket.
/// NOT covered by offline unit tests.
pub(crate) fn connect_docker() -> Result<Docker, BollardError> {
    Docker::connect_with_local_defaults()
}

/// Map a bollard error to a human-readable "is the socket missing?" check.
/// An IO error with `NotFound` or `ConnectionRefused` means the docker
/// socket is not reachable.
pub(crate) fn is_socket_missing(e: &BollardError) -> bool {
    if let BollardError::IOError { err } = e {
        return matches!(
            err.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        );
    }
    false
}

/// Return true when a bollard error is a 404 "no such image/container".
pub(crate) fn is_not_found(e: &BollardError) -> bool {
    matches!(
        e,
        BollardError::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

// ---------------------------------------------------------------------------
// FakeDocker for offline tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    type Queue<T> = Arc<Mutex<VecDeque<Result<T, BollardError>>>>;

    fn enqueue<T>(q: &Queue<T>, v: Result<T, BollardError>) {
        q.lock().expect("queue lock").push_back(v);
    }

    fn dequeue<T>(q: &Queue<T>, method: &str) -> Result<T, BollardError> {
        q.lock()
            .expect("queue lock")
            .pop_front()
            .unwrap_or_else(|| panic!("FakeDocker: no queued result for {method}"))
    }

    /// Pre-canned Docker fake for offline testing.
    ///
    /// Queue results via the `with_*` builder methods before driving
    /// the inner `*_impl` functions.  Calls are consumed FIFO.
    #[derive(Clone, Default)]
    pub(crate) struct FakeDocker {
        inspect_image_q: Queue<ImageInspect>,
        create_container_q: Queue<String>,
        start_container_q: Queue<()>,
        remove_container_q: Queue<()>,
        commit_container_q: Queue<String>,
    }

    impl FakeDocker {
        pub fn with_inspect_image(self, r: Result<ImageInspect, BollardError>) -> Self {
            enqueue(&self.inspect_image_q, r);
            self
        }
        pub fn with_create_container(self, r: Result<String, BollardError>) -> Self {
            enqueue(&self.create_container_q, r);
            self
        }
        pub fn with_start_container(self, r: Result<(), BollardError>) -> Self {
            enqueue(&self.start_container_q, r);
            self
        }
        pub fn with_remove_container(self, r: Result<(), BollardError>) -> Self {
            enqueue(&self.remove_container_q, r);
            self
        }
        pub fn with_commit_container(self, r: Result<String, BollardError>) -> Self {
            enqueue(&self.commit_container_q, r);
            self
        }
    }

    impl DockerApi for FakeDocker {
        fn inspect_image<'a>(
            &'a self,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<ImageInspect, BollardError>> {
            async move { dequeue(&self.inspect_image_q, "inspect_image") }.boxed()
        }

        fn create_container<'a>(
            &'a self,
            _config: ContainerCreateBody,
        ) -> BoxFuture<'a, Result<String, BollardError>> {
            async move { dequeue(&self.create_container_q, "create_container") }.boxed()
        }

        fn start_container<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<(), BollardError>> {
            async move { dequeue(&self.start_container_q, "start_container") }.boxed()
        }

        fn remove_container<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<(), BollardError>> {
            async move { dequeue(&self.remove_container_q, "remove_container") }.boxed()
        }

        fn commit_container<'a>(
            &'a self,
            _container_id: &'a str,
            _repo: &'a str,
            _tag: &'a str,
            _labels: HashMap<String, String>,
        ) -> BoxFuture<'a, Result<String, BollardError>> {
            async move { dequeue(&self.commit_container_q, "commit_container") }.boxed()
        }
    }

    // ── SpyDocker ────────────────────────────────────────────────────────────

    /// Configurable spy for `DockerApi` that records calls and captures
    /// committed labels.
    ///
    /// - `inspect_image` returns the provided `ImageInspect` (cloned each call).
    /// - `create_container` returns a canned container ID without starting.
    /// - `start_container` panics — `patch_image_impl` must never start.
    /// - `remove_container` records that it was called via an `AtomicBool`.
    /// - `commit_container` captures the label map and returns `Ok` or an
    ///   error depending on whether `.with_commit_failure()` was called.
    pub(crate) struct SpyDocker {
        inspect_result: ImageInspect,
        committed_labels: Arc<Mutex<Option<HashMap<String, String>>>>,
        removed: Arc<AtomicBool>,
        commit_fail: bool,
    }

    impl SpyDocker {
        pub fn new(inspect_result: ImageInspect) -> Self {
            Self {
                inspect_result,
                committed_labels: Arc::new(Mutex::new(None)),
                removed: Arc::new(AtomicBool::new(false)),
                commit_fail: false,
            }
        }

        /// Make `commit_container` return a 500 server error.
        pub fn with_commit_failure(mut self) -> Self {
            self.commit_fail = true;
            self
        }

        /// Returns an `Arc` to the captured label map so callers can read it
        /// after the call.
        pub fn committed_labels(&self) -> Arc<Mutex<Option<HashMap<String, String>>>> {
            Arc::clone(&self.committed_labels)
        }

        /// Returns `true` if `remove_container` was called at least once.
        pub fn was_removed(&self) -> bool {
            self.removed.load(Ordering::SeqCst)
        }
    }

    impl DockerApi for SpyDocker {
        fn inspect_image<'a>(
            &'a self,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<ImageInspect, BollardError>> {
            let result = self.inspect_result.clone();
            async move { Ok(result) }.boxed()
        }

        fn create_container<'a>(
            &'a self,
            _config: ContainerCreateBody,
        ) -> BoxFuture<'a, Result<String, BollardError>> {
            async { Ok("spy-container".to_string()) }.boxed()
        }

        fn start_container<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<(), BollardError>> {
            async { unreachable!("patch_image_impl never starts the container") }.boxed()
        }

        fn remove_container<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<(), BollardError>> {
            self.removed.store(true, Ordering::SeqCst);
            async { Ok(()) }.boxed()
        }

        fn commit_container<'a>(
            &'a self,
            _container_id: &'a str,
            _repo: &'a str,
            _tag: &'a str,
            labels: HashMap<String, String>,
        ) -> BoxFuture<'a, Result<String, BollardError>> {
            *self.committed_labels.lock().unwrap() = Some(labels);
            if self.commit_fail {
                async {
                    Err(BollardError::DockerResponseServerError {
                        status_code: 500,
                        message: "commit error".into(),
                    })
                }
                .boxed()
            } else {
                async { Ok("sha256:spy".to_string()) }.boxed()
            }
        }
    }
}
