//! Verify-mode label-drift comparison.
//!
//! `bollard::Docker::inspect_image` returns the typed OCI config for a
//! local image.  We extract every key starting with the v1
//! `org.botwork.mcp.` namespace and compare against the freshly
//! composed label set.  Drift on any key — missing, extra, or
//! value-differs — surfaces as [`VerifyError::Drift`], which maps
//! to exit code 6 ("label drift detected").
//!
//! Why namespace-scope rather than full-image-label set: the
//! catalog upserter on the consumer side only cares about
//! `org.botwork.mcp.*`; an operator who's annotating their image
//! with `org.opencontainers.image.*` shouldn't have the verify
//! step fail because we re-rendered those too. The probe-emitted
//! label set is a closed set under the v1 namespace; everything
//! else is the operator's territory.

use std::collections::BTreeMap;

use bollard::models::ImageInspect;
use thiserror::Error;

use crate::mcp_probe::compose::LABEL_NAMESPACE;
use crate::mcp_probe::docker::{connect_docker, is_not_found, is_socket_missing, DockerApi};

/// Inspect `image` via the local docker daemon and compare its labels
/// (filtered to the v1 namespace) against `expected`.
pub fn verify(image: &str, expected: &BTreeMap<String, String>) -> Result<(), VerifyError> {
    let docker = connect_docker().map_err(|e| {
        if is_socket_missing(&e) {
            VerifyError::DockerMissing
        } else {
            VerifyError::InspectFailed {
                image: image.to_string(),
                stderr: e.to_string(),
            }
        }
    })?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| VerifyError::InspectFailed {
            image: image.to_string(),
            stderr: format!("failed to initialize async runtime: {e}"),
        })?;
    let actual = rt.block_on(read_image_labels_impl(image, &docker))?;
    compare_labels_in_namespace(&actual, expected)
}

/// Compare `actual` (a raw label map from inspect, possibly containing
/// non-namespace keys) against `expected` (the re-probed namespace-scoped
/// label set).  Factored out of [`verify`] so tests can exercise the
/// comparison/drift logic without the docker layer.
pub(crate) fn compare_labels_in_namespace(
    actual: &BTreeMap<String, String>,
    expected: &BTreeMap<String, String>,
) -> Result<(), VerifyError> {
    let actual_in_ns = filter_namespace(actual);
    if actual_in_ns == *expected {
        return Ok(());
    }
    let drift = diff(&actual_in_ns, expected);
    Err(VerifyError::Drift { drift })
}

/// Call `Docker::inspect_image` and extract `Config.Labels`.
/// Returns an empty map if the image has no labels (rather than
/// failing) so an as-yet-unlabeled image diffs cleanly against the
/// expected set in describe-from-zero scenarios.
async fn read_image_labels_impl<D: DockerApi + ?Sized>(
    image: &str,
    docker: &D,
) -> Result<BTreeMap<String, String>, VerifyError> {
    let inspect = docker.inspect_image(image).await.map_err(|e| {
        if is_not_found(&e) {
            VerifyError::InspectFailed {
                image: image.to_string(),
                stderr: format!("no such image: {image}"),
            }
        } else if is_socket_missing(&e) {
            VerifyError::DockerMissing
        } else {
            VerifyError::InspectFailed {
                image: image.to_string(),
                stderr: e.to_string(),
            }
        }
    })?;
    labels_from_inspect(&inspect)
}

/// Extract the label map from a typed [`ImageInspect`] response.
///
/// Returns an empty map when the image has no labels (Config.Labels is
/// absent or null) — so an as-yet-unlabeled image diffs cleanly.
fn labels_from_inspect(inspect: &ImageInspect) -> Result<BTreeMap<String, String>, VerifyError> {
    let labels = inspect.config.as_ref().and_then(|c| c.labels.as_ref());
    match labels {
        None => Ok(BTreeMap::new()),
        Some(map) => Ok(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
    }
}

fn filter_namespace(all: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    all.iter()
        .filter(|(k, _)| k.starts_with(LABEL_NAMESPACE))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Produce a human-readable diff between `actual` and `expected`.
/// Used inside [`VerifyError::Drift`] so the operator sees what's
/// off.
fn diff(actual: &BTreeMap<String, String>, expected: &BTreeMap<String, String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (k, v) in expected {
        match actual.get(k) {
            None => out.push(format!("missing: {k}={v}")),
            Some(av) if av != v => out.push(format!("changed: {k}: image={av} expected={v}")),
            _ => {}
        }
    }
    for k in actual.keys() {
        if !expected.contains_key(k) {
            out.push(format!("extra:   {k} (not in re-probed label set)"));
        }
    }
    out
}

#[derive(Debug, Error)]
pub enum VerifyError {
    /// The docker socket is not reachable.  Maps to exit 7 via the
    /// catch-all `Verify(_) => 7` arm in McpProbeError::exit_code.
    #[error("docker socket not reachable")]
    DockerMissing,

    #[error("docker inspect {image} failed: {stderr}")]
    InspectFailed { image: String, stderr: String },

    #[error("docker inspect output shape: {0}")]
    InspectShape(String),

    /// One or more labels in the v1 namespace did not match the
    /// re-probed set. Maps to exit 6 — the dedicated drift code.
    #[error("label drift detected:\n  {}", drift.join("\n  "))]
    Drift { drift: Vec<String> },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_probe::docker::test_support::FakeDocker;
    use bollard::errors::Error as BollardError;
    use bollard::models::ImageConfig;

    // ── compare_labels_in_namespace ─────────────────────────────────────────

    #[test]
    fn compare_returns_ok_when_namespace_labels_match() {
        let mut actual = BTreeMap::new();
        actual.insert("org.botwork.mcp.name".into(), "echo".into());
        actual.insert("org.botwork.mcp.port".into(), "8000".into());
        actual.insert("org.opencontainers.image.version".into(), "0.1.0".into()); // filtered out
        let mut expected = BTreeMap::new();
        expected.insert("org.botwork.mcp.name".into(), "echo".into());
        expected.insert("org.botwork.mcp.port".into(), "8000".into());
        assert!(compare_labels_in_namespace(&actual, &expected).is_ok());
    }

    #[test]
    fn compare_returns_drift_when_expected_has_missing_key() {
        let actual: BTreeMap<String, String> = BTreeMap::new();
        let mut expected = BTreeMap::new();
        expected.insert("org.botwork.mcp.name".into(), "echo".into());
        let err = compare_labels_in_namespace(&actual, &expected).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("missing"), "{msg}");
    }

    #[test]
    fn compare_returns_drift_when_actual_has_extra_namespace_key() {
        let mut actual = BTreeMap::new();
        actual.insert("org.botwork.mcp.name".into(), "echo".into());
        actual.insert("org.botwork.mcp.extra".into(), "extra".into());
        let mut expected = BTreeMap::new();
        expected.insert("org.botwork.mcp.name".into(), "echo".into());
        let err = compare_labels_in_namespace(&actual, &expected).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("extra"), "{msg}");
    }

    #[test]
    fn compare_returns_drift_when_value_changed() {
        let mut actual = BTreeMap::new();
        actual.insert("org.botwork.mcp.port".into(), "8000".into());
        let mut expected = BTreeMap::new();
        expected.insert("org.botwork.mcp.port".into(), "9000".into());
        let err = compare_labels_in_namespace(&actual, &expected).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("changed"), "{msg}");
    }

    #[test]
    fn compare_non_namespace_labels_in_actual_are_ignored() {
        let mut actual = BTreeMap::new();
        // Only non-namespace labels; namespace is empty → matches empty expected
        actual.insert("com.example.meta".into(), "v1".into());
        actual.insert(
            "org.opencontainers.image.created".into(),
            "2024-01-01".into(),
        );
        let expected: BTreeMap<String, String> = BTreeMap::new();
        assert!(compare_labels_in_namespace(&actual, &expected).is_ok());
    }

    #[test]
    fn compare_both_empty_returns_ok() {
        let empty: BTreeMap<String, String> = BTreeMap::new();
        assert!(compare_labels_in_namespace(&empty, &empty).is_ok());
    }

    // ── labels_from_inspect ─────────────────────────────────────────────────

    fn make_inspect(labels: Option<std::collections::HashMap<String, String>>) -> ImageInspect {
        ImageInspect {
            config: Some(ImageConfig {
                labels,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn labels_from_inspect_happy_path() {
        let mut map = std::collections::HashMap::new();
        map.insert("org.botwork.mcp.name".to_string(), "echo".to_string());
        map.insert("org.botwork.mcp.port".to_string(), "8000".to_string());
        let inspect = make_inspect(Some(map));
        let labels = labels_from_inspect(&inspect).expect("parse");
        assert_eq!(
            labels.get("org.botwork.mcp.name").map(String::as_str),
            Some("echo")
        );
        assert_eq!(
            labels.get("org.botwork.mcp.port").map(String::as_str),
            Some("8000")
        );
        assert_eq!(labels.len(), 2);
    }

    #[test]
    fn labels_from_inspect_returns_empty_when_labels_is_none() {
        let inspect = make_inspect(None);
        let labels = labels_from_inspect(&inspect).expect("parse");
        assert!(labels.is_empty());
    }

    #[test]
    fn labels_from_inspect_returns_empty_when_config_missing() {
        let inspect = ImageInspect {
            config: None,
            ..Default::default()
        };
        let labels = labels_from_inspect(&inspect).expect("parse");
        assert!(labels.is_empty());
    }

    #[test]
    fn labels_from_inspect_handles_multiple_labels() {
        let mut map = std::collections::HashMap::new();
        map.insert("org.botwork.mcp.name".to_string(), "echo".to_string());
        map.insert("org.botwork.mcp.port".to_string(), "8000".to_string());
        map.insert(
            "org.botwork.mcp.schema-version".to_string(),
            "1".to_string(),
        );
        map.insert(
            "org.opencontainers.image.version".to_string(),
            "0.1.0".to_string(),
        );
        let inspect = make_inspect(Some(map));
        let labels = labels_from_inspect(&inspect).expect("parse");
        assert_eq!(labels.len(), 4);
        assert_eq!(labels["org.botwork.mcp.schema-version"], "1");
    }

    // ── read_image_labels_impl via FakeDocker ───────────────────────────────

    #[tokio::test]
    async fn read_labels_returns_map_on_success() {
        let mut map = std::collections::HashMap::new();
        map.insert("org.botwork.mcp.name".to_string(), "echo".to_string());
        let fake = FakeDocker::default().with_inspect_image(Ok(make_inspect(Some(map))));
        let labels = read_image_labels_impl("image:tag", &fake)
            .await
            .expect("ok");
        assert_eq!(
            labels.get("org.botwork.mcp.name").map(String::as_str),
            Some("echo")
        );
    }

    #[tokio::test]
    async fn read_labels_returns_empty_when_image_has_no_labels() {
        let fake = FakeDocker::default().with_inspect_image(Ok(make_inspect(None)));
        let labels = read_image_labels_impl("image:tag", &fake)
            .await
            .expect("ok");
        assert!(labels.is_empty());
    }

    #[tokio::test]
    async fn read_labels_maps_404_to_inspect_failed() {
        let fake = FakeDocker::default().with_inspect_image(Err(
            BollardError::DockerResponseServerError {
                status_code: 404,
                message: "no such image: image:tag".into(),
            },
        ));
        let err = read_image_labels_impl("image:tag", &fake)
            .await
            .unwrap_err();
        assert!(matches!(err, VerifyError::InspectFailed { .. }), "{err:?}");
        let msg = format!("{err}");
        assert!(msg.contains("image:tag"), "{msg}");
    }

    #[tokio::test]
    async fn read_labels_maps_socket_error_to_docker_missing() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let fake =
            FakeDocker::default().with_inspect_image(Err(BollardError::IOError { err: io_err }));
        let err = read_image_labels_impl("image:tag", &fake)
            .await
            .unwrap_err();
        assert!(matches!(err, VerifyError::DockerMissing), "{err:?}");
    }

    // ── filter_namespace ────────────────────────────────────────────────────

    #[test]
    fn filter_namespace_keeps_only_v1_prefix() {
        let mut all = BTreeMap::new();
        all.insert("org.botwork.mcp.name".into(), "echo".into());
        all.insert("org.opencontainers.image.version".into(), "0.1.0".into());
        all.insert("org.botwork.mcp.port".into(), "8000".into());
        let kept = filter_namespace(&all);
        assert_eq!(kept.len(), 2);
        assert!(kept.contains_key("org.botwork.mcp.name"));
        assert!(kept.contains_key("org.botwork.mcp.port"));
    }

    #[test]
    fn filter_namespace_with_empty_map_returns_empty() {
        let empty: BTreeMap<String, String> = BTreeMap::new();
        assert!(filter_namespace(&empty).is_empty());
    }

    #[test]
    fn filter_namespace_with_no_matching_keys_returns_empty() {
        let mut all = BTreeMap::new();
        all.insert("com.example.other".into(), "v1".into());
        all.insert("org.opencontainers.image.title".into(), "my-image".into());
        assert!(filter_namespace(&all).is_empty());
    }

    #[test]
    fn diff_reports_missing_changed_and_extra() {
        let mut actual = BTreeMap::new();
        actual.insert("org.botwork.mcp.name".into(), "echo".into());
        actual.insert("org.botwork.mcp.port".into(), "8000".into());
        actual.insert("org.botwork.mcp.stale".into(), "yes".into());
        let mut expected = BTreeMap::new();
        expected.insert("org.botwork.mcp.name".into(), "echo".into());
        expected.insert("org.botwork.mcp.port".into(), "9000".into());
        expected.insert("org.botwork.mcp.new".into(), "yes".into());
        let drift = diff(&actual, &expected);
        let joined = drift.join("\n");
        assert!(
            joined.contains("missing: org.botwork.mcp.new=yes"),
            "{joined}"
        );
        assert!(
            joined.contains("changed: org.botwork.mcp.port: image=8000 expected=9000"),
            "{joined}"
        );
        assert!(
            joined.contains("extra:   org.botwork.mcp.stale"),
            "{joined}"
        );
    }

    #[test]
    fn diff_empty_when_sets_match() {
        let mut both = BTreeMap::new();
        both.insert("org.botwork.mcp.name".into(), "echo".into());
        assert!(diff(&both, &both).is_empty());
    }

    #[test]
    fn diff_all_missing_when_actual_is_empty() {
        let actual: BTreeMap<String, String> = BTreeMap::new();
        let mut expected = BTreeMap::new();
        expected.insert("org.botwork.mcp.name".into(), "echo".into());
        expected.insert("org.botwork.mcp.tools.count".into(), "1".into());

        let drift = diff(&actual, &expected);
        assert_eq!(drift.len(), 2);
        assert!(drift.iter().all(|d| d.starts_with("missing:")), "{drift:?}");
    }

    #[test]
    fn diff_all_extra_when_expected_is_empty() {
        let mut actual = BTreeMap::new();
        actual.insert("org.botwork.mcp.name".into(), "echo".into());
        actual.insert("org.botwork.mcp.tools.count".into(), "1".into());
        let expected: BTreeMap<String, String> = BTreeMap::new();

        let drift = diff(&actual, &expected);
        assert_eq!(drift.len(), 2);
        assert!(drift.iter().all(|d| d.starts_with("extra:")), "{drift:?}");
    }

    #[test]
    fn drift_error_renders_diff_lines() {
        let err = VerifyError::Drift {
            drift: vec!["missing: org.botwork.mcp.name=echo".into()],
        };
        let msg = format!("{err}");
        assert!(msg.contains("missing"), "{msg}");
        assert!(msg.contains("org.botwork.mcp.name"), "{msg}");
    }

    // --- VerifyError display for all variants ---

    #[test]
    fn verify_error_docker_missing_display() {
        let err = VerifyError::DockerMissing;
        let msg = format!("{err}");
        assert!(msg.contains("docker"), "{msg}");
    }

    #[test]
    fn verify_error_inspect_failed_display() {
        let err = VerifyError::InspectFailed {
            image: "my-image:latest".into(),
            stderr: "no such image".into(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("my-image:latest"), "{msg}");
        assert!(msg.contains("no such image"), "{msg}");
    }

    #[test]
    fn verify_error_inspect_shape_display() {
        let err = VerifyError::InspectShape("Config.Labels is not an object".into());
        let msg = format!("{err}");
        assert!(msg.contains("Config.Labels"), "{msg}");
    }

    #[test]
    fn verify_error_drift_with_multiple_lines() {
        let err = VerifyError::Drift {
            drift: vec![
                "missing: org.botwork.mcp.name=echo".into(),
                "extra:   org.botwork.mcp.stale (not in re-probed label set)".into(),
            ],
        };
        let msg = format!("{err}");
        assert!(msg.contains("missing"), "{msg}");
        assert!(msg.contains("extra"), "{msg}");
    }
}
