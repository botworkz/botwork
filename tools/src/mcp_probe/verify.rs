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
    let actual_in_ns = filter_namespace(&actual);
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
    let docs: JsonValue = serde_json::from_slice(&output.stdout)
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
    fn drift_error_renders_diff_lines() {
        let err = VerifyError::Drift {
            drift: vec!["missing: org.botwork.mcp.name=echo".into()],
        };
        let msg = format!("{err}");
        assert!(msg.contains("missing"), "{msg}");
        assert!(msg.contains("org.botwork.mcp.name"), "{msg}");
    }
}
