//! Verify-mode label-drift comparison.
//!
//! `docker image inspect <ref>` returns the OCI config including
//! `.Config.Labels`. We extract every key starting with the v1
//! `org.botwork.mcp.` namespace and compare against the freshly
//! composed label set. Drift on any key — missing, extra, or
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
use std::process::{Command, Stdio};

use serde_json::Value as JsonValue;
use thiserror::Error;

use crate::mcp_probe::compose::LABEL_NAMESPACE;

/// Run `<runtime> image inspect <image>` and compare its labels
/// (filtered to the v1 namespace) against `expected`.
pub fn verify(
    image: &str,
    runtime: &str,
    expected: &BTreeMap<String, String>,
) -> Result<(), VerifyError> {
    let actual = read_image_labels(runtime, image)?;
    compare_labels_in_namespace(&actual, expected)
}

/// Compare `actual` (a raw label map from inspect, possibly containing
/// non-namespace keys) against `expected` (the re-probed namespace-scoped
/// label set).  Factored out of [`verify`] so tests can exercise the
/// comparison/drift logic without spawning a container runtime.
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

/// `docker image inspect <ref>` and parse `.[0].Config.Labels`.
/// Returns an empty map if the image has no labels (rather than
/// failing) so an as-yet-unlabeled image diffs cleanly against the
/// expected set in describe-from-zero scenarios.
fn read_image_labels(runtime: &str, image: &str) -> Result<BTreeMap<String, String>, VerifyError> {
    let output = Command::new(runtime)
        .args(["image", "inspect", image])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => VerifyError::RuntimeMissing(runtime.to_string()),
            _ => VerifyError::Io(err.to_string()),
        })?;
    if !output.status.success() {
        return Err(VerifyError::InspectFailed {
            image: image.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr)
                .trim_end()
                .to_string(),
        });
    }
    parse_inspect_output(&output.stdout)
}

/// Parse the raw stdout bytes from `<runtime> image inspect <ref>` into a
/// label map.  Extracted from [`read_image_labels`] so the JSON-parsing
/// logic is unit-testable against synthetic payloads without invoking a
/// container runtime.
pub(crate) fn parse_inspect_output(stdout: &[u8]) -> Result<BTreeMap<String, String>, VerifyError> {
    let docs: JsonValue = serde_json::from_slice(stdout)
        .map_err(|err| VerifyError::InspectShape(format!("docker inspect decode: {err}")))?;
    let first = docs.as_array().and_then(|a| a.first()).ok_or_else(|| {
        VerifyError::InspectShape("docker inspect returned empty array".to_string())
    })?;
    let labels = first
        .get("Config")
        .and_then(|c| c.get("Labels"))
        .cloned()
        .unwrap_or(JsonValue::Null);
    let mut out = BTreeMap::new();
    if labels.is_null() {
        return Ok(out);
    }
    let labels = labels
        .as_object()
        .ok_or_else(|| VerifyError::InspectShape("Config.Labels is not an object".to_string()))?;
    for (k, v) in labels {
        if let Some(s) = v.as_str() {
            out.insert(k.clone(), s.to_string());
        } else {
            // OCI config Labels are spec'd as map<string,string>.
            // A non-string value would be a producer-side bug;
            // surface it loudly.
            return Err(VerifyError::InspectShape(format!(
                "label {k:?} has non-string value"
            )));
        }
    }
    Ok(out)
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
    #[error("container runtime '{0}' not found on PATH")]
    RuntimeMissing(String),

    #[error("io error invoking inspect: {0}")]
    Io(String),

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

    // ── parse_inspect_output ────────────────────────────────────────────────

    #[test]
    fn parse_inspect_output_happy_path() {
        let json = br#"[{"Config":{"Labels":{"org.botwork.mcp.name":"echo","org.botwork.mcp.port":"8000"}}}]"#;
        let labels = parse_inspect_output(json).expect("parse");
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
    fn parse_inspect_output_returns_empty_map_when_labels_is_null() {
        let json = br#"[{"Config":{"Labels":null}}]"#;
        let labels = parse_inspect_output(json).expect("parse");
        assert!(labels.is_empty());
    }

    #[test]
    fn parse_inspect_output_returns_empty_map_when_config_labels_missing() {
        let json = br#"[{"Config":{}}]"#;
        let labels = parse_inspect_output(json).expect("parse");
        assert!(labels.is_empty());
    }

    #[test]
    fn parse_inspect_output_returns_empty_map_when_config_missing() {
        // An inspect output with no Config key at all should behave like
        // "no labels" — the value resolves to Null via the chained and_then.
        let json = br#"[{}]"#;
        let labels = parse_inspect_output(json).expect("parse");
        assert!(labels.is_empty());
    }

    #[test]
    fn parse_inspect_output_fails_on_invalid_json() {
        let err = parse_inspect_output(b"not json").unwrap_err();
        assert!(matches!(err, VerifyError::InspectShape(_)), "{err:?}");
    }

    #[test]
    fn parse_inspect_output_fails_on_empty_array() {
        let err = parse_inspect_output(b"[]").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("empty array"), "{msg}");
    }

    #[test]
    fn parse_inspect_output_fails_when_labels_not_an_object() {
        let json = br#"[{"Config":{"Labels":"should-be-an-object"}}]"#;
        let err = parse_inspect_output(json).unwrap_err();
        assert!(matches!(err, VerifyError::InspectShape(_)), "{err:?}");
    }

    #[test]
    fn parse_inspect_output_fails_on_non_string_label_value() {
        let json = br#"[{"Config":{"Labels":{"org.botwork.mcp.port":8000}}}]"#;
        let err = parse_inspect_output(json).unwrap_err();
        assert!(matches!(err, VerifyError::InspectShape(_)), "{err:?}");
        let msg = format!("{err}");
        assert!(msg.contains("non-string"), "{msg}");
    }

    #[test]
    fn parse_inspect_output_handles_multiple_labels() {
        let json = r#"[{"Config":{"Labels":{
            "org.botwork.mcp.name":"echo",
            "org.botwork.mcp.port":"8000",
            "org.botwork.mcp.schema-version":"1",
            "org.opencontainers.image.version":"0.1.0"
        }}}]"#;
        let labels = parse_inspect_output(json.as_bytes()).expect("parse");
        assert_eq!(labels.len(), 4);
        assert_eq!(labels["org.botwork.mcp.schema-version"], "1");
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
    fn verify_error_runtime_missing_display() {
        let err = VerifyError::RuntimeMissing("podman".into());
        let msg = format!("{err}");
        assert!(msg.contains("podman"), "{msg}");
    }

    #[test]
    fn verify_error_io_display() {
        let err = VerifyError::Io("connection refused".into());
        let msg = format!("{err}");
        assert!(msg.contains("connection refused"), "{msg}");
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
