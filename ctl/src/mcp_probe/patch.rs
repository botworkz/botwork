//! Image-label patching via the local docker daemon.
//!
//! `patch_image` stamps a new label set onto `image_in` and writes the
//! result to `image_out` by:
//!
//! 1. Creating a stopped container from `image_in`.
//! 2. Committing it with a `ContainerConfig { labels }` override,
//!    producing a new local image tagged as `image_out`.
//! 3. Removing the temporary container.
//!
//! This is the daemon-native analog of what `crane mutate --tag` did
//! previously — it rewrites the image config locally without touching
//! a registry.  The `docker buildx` fallback that existed for
//! environments where `crane` was absent has been deleted: a bollard
//! call is a library call and is never "missing".

use std::collections::BTreeMap;
use std::collections::HashMap;

use thiserror::Error;

use super::docker::{connect_docker, is_socket_missing, DockerApi};

/// Patch `image_in` with the supplied label set, writing the result to
/// `image_out` in the local daemon image store.
pub fn patch_image(
    image_in: &str,
    image_out: &str,
    labels: &BTreeMap<String, String>,
) -> Result<(), PatchError> {
    let docker = connect_docker().map_err(|e| {
        if is_socket_missing(&e) {
            PatchError::DockerMissing
        } else {
            PatchError::Patch(e.to_string())
        }
    })?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| PatchError::Patch(format!("failed to create tokio runtime: {e}")))?;
    rt.block_on(patch_image_impl(image_in, image_out, labels, &docker))
}

/// Inner async implementation; driven against a [`DockerApi`] seam so
/// tests can exercise the full label-commit flow without a real daemon.
async fn patch_image_impl<D: DockerApi + ?Sized>(
    image_in: &str,
    image_out: &str,
    labels: &BTreeMap<String, String>,
    docker: &D,
) -> Result<(), PatchError> {
    use bollard::models::ContainerCreateBody;

    // Split image_out into repo + tag for CommitContainerOptions.
    let (repo, tag) = parse_image_ref(image_out);

    // 1. Create a stopped container from image_in (don't start it).
    let body = ContainerCreateBody {
        image: Some(image_in.to_string()),
        ..Default::default()
    };
    let container_id = docker
        .create_container(body)
        .await
        .map_err(|e| PatchError::Patch(format!("create container from {image_in}: {e}")))?;

    // 2. Commit the container with the new label set, producing image_out.
    let label_map: HashMap<String, String> =
        labels.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let result = docker
        .commit_container(&container_id, repo, tag, label_map)
        .await;

    // 3. Remove the temporary container regardless of commit success.
    let _ = docker.remove_container(&container_id).await;

    match result {
        Ok(id) if id.is_empty() => Err(PatchError::Patch(format!(
            "commit {image_in} → {image_out}: daemon returned empty image ID"
        ))),
        Ok(_) => Ok(()),
        Err(e) => Err(PatchError::Patch(format!(
            "commit {image_in} → {image_out}: {e}"
        ))),
    }
}

/// Split an image reference `repo:tag` into `(repo, tag)`.
///
/// The tag is the suffix after the **last** `:` that is not followed
/// by a `/` (which would indicate a registry host:port rather than a
/// tag separator).  If no tag is found, `"latest"` is returned.
fn parse_image_ref(image: &str) -> (&str, &str) {
    if let Some(pos) = image.rfind(':') {
        let after = &image[pos + 1..];
        if !after.contains('/') {
            return (&image[..pos], after);
        }
    }
    (image, "latest")
}

#[derive(Debug, Error)]
pub enum PatchError {
    /// The docker socket is not reachable.  Maps to exit 7
    /// ("image-patching tool unavailable / failed") in the CLI
    /// exit-code table.
    #[error("docker socket not reachable")]
    DockerMissing,

    /// A daemon API call failed (create, commit, or remove).
    #[error("image patch failed: {0}")]
    Patch(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_probe::docker::test_support::FakeDocker;

    fn two_labels() -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        labels.insert("org.botwork.mcp.name".to_string(), "echo".to_string());
        labels.insert("org.botwork.mcp.port".to_string(), "8000".to_string());
        labels
    }

    // ── parse_image_ref ─────────────────────────────────────────────────────

    #[test]
    fn parse_image_ref_splits_on_last_colon() {
        assert_eq!(parse_image_ref("repo/name:tag"), ("repo/name", "tag"));
        assert_eq!(
            parse_image_ref("ghcr.io/org/name:1.0.0"),
            ("ghcr.io/org/name", "1.0.0")
        );
        assert_eq!(parse_image_ref("name:latest"), ("name", "latest"));
    }

    #[test]
    fn parse_image_ref_defaults_tag_when_no_colon() {
        assert_eq!(
            parse_image_ref("registry.io/name"),
            ("registry.io/name", "latest")
        );
        assert_eq!(parse_image_ref("plain-name"), ("plain-name", "latest"));
    }

    #[test]
    fn parse_image_ref_treats_host_port_colon_as_repo() {
        // "ghcr.io:443/org/name" has no tag — defaults to "latest"
        assert_eq!(
            parse_image_ref("ghcr.io:443/org/name"),
            ("ghcr.io:443/org/name", "latest")
        );
    }

    // ── patch_image_impl via FakeDocker ─────────────────────────────────────

    #[tokio::test]
    async fn patch_succeeds_and_commits_labels() {
        let fake = FakeDocker::default()
            .with_create_container(Ok("tmp-container-id".to_string()))
            .with_commit_container(Ok("sha256:newimgid".to_string()))
            .with_remove_container(Ok(()));

        let labels = two_labels();
        let result = patch_image_impl("in:latest", "out:labeled", &labels, &fake).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[tokio::test]
    async fn patch_fails_when_create_fails() {
        use bollard::errors::Error as BollardError;
        let fake = FakeDocker::default().with_create_container(Err(
            BollardError::DockerResponseServerError {
                status_code: 500,
                message: "daemon overloaded".into(),
            },
        ));

        let err = patch_image_impl("in:latest", "out:labeled", &two_labels(), &fake)
            .await
            .unwrap_err();
        assert!(matches!(err, PatchError::Patch(_)), "{err:?}");
        let msg = format!("{err}");
        assert!(msg.contains("create container"), "{msg}");
    }

    #[tokio::test]
    async fn patch_fails_when_commit_fails_and_still_removes_container() {
        use bollard::errors::Error as BollardError;
        let fake = FakeDocker::default()
            .with_create_container(Ok("tmp-id".to_string()))
            .with_commit_container(Err(BollardError::DockerResponseServerError {
                status_code: 500,
                message: "commit error".into(),
            }))
            .with_remove_container(Ok(()));

        let err = patch_image_impl("in:latest", "out:labeled", &two_labels(), &fake)
            .await
            .unwrap_err();
        assert!(matches!(err, PatchError::Patch(_)), "{err:?}");
        let msg = format!("{err}");
        assert!(msg.contains("commit"), "{msg}");
        // The remove_container result was consumed — confirming it was called.
    }

    /// Load-bearing contract: every key=value in `labels` is passed to
    /// `commit_container` as a label on the new image config.
    #[tokio::test]
    async fn patch_passes_all_labels_to_commit() {
        use crate::mcp_probe::docker::DockerApi;
        use bollard::errors::Error as BollardError;
        use bollard::models::{ContainerCreateBody, ImageInspect};
        use futures_util::future::BoxFuture;
        use futures_util::FutureExt;
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        // Spy docker that captures the labels passed to commit_container.
        #[derive(Default)]
        struct SpyDocker {
            committed_labels: Arc<Mutex<Option<HashMap<String, String>>>>,
        }

        impl DockerApi for SpyDocker {
            fn inspect_image<'a>(
                &'a self,
                _name: &'a str,
            ) -> BoxFuture<'a, Result<ImageInspect, BollardError>> {
                async { panic!("unexpected") }.boxed()
            }
            fn create_container<'a>(
                &'a self,
                _config: ContainerCreateBody,
            ) -> BoxFuture<'a, Result<String, BollardError>> {
                async { Ok("spy-container".to_string()) }.boxed()
            }
            fn start_container<'a>(
                &'a self,
                _id: &'a str,
            ) -> BoxFuture<'a, Result<(), BollardError>> {
                async { panic!("unexpected") }.boxed()
            }
            fn remove_container<'a>(
                &'a self,
                _id: &'a str,
            ) -> BoxFuture<'a, Result<(), BollardError>> {
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
                async { Ok("sha256:spy".to_string()) }.boxed()
            }
        }

        let spy = SpyDocker::default();
        let captured = Arc::clone(&spy.committed_labels);
        let labels = two_labels();
        patch_image_impl("in:latest", "out:tag", &labels, &spy)
            .await
            .expect("ok");

        let got = captured.lock().unwrap().clone().expect("commit was called");
        assert_eq!(
            got.get("org.botwork.mcp.name").map(String::as_str),
            Some("echo")
        );
        assert_eq!(
            got.get("org.botwork.mcp.port").map(String::as_str),
            Some("8000")
        );
        assert_eq!(got.len(), labels.len(), "all labels passed through");
    }

    // ── PatchError display ───────────────────────────────────────────────────

    #[test]
    fn docker_missing_display() {
        let msg = format!("{}", PatchError::DockerMissing);
        assert!(msg.contains("docker"), "{msg}");
    }

    #[test]
    fn patch_error_display_includes_detail() {
        let err = PatchError::Patch("create container from bad:tag: 500 daemon error".into());
        let msg = format!("{err}");
        assert!(msg.contains("daemon error"), "{msg}");
    }
}
