//! Image-label patching: crane fast path, buildx fallback.
//!
//! ## Why two paths
//!
//! `crane mutate --label k=v <ref>` rewrites only the OCI config
//! blob — no layer rebuild, ~100ms. It's the production default
//! because the producer-side want is "attach labels without changing
//! the binary". The fallback exists for environments where crane
//! isn't installed (external-plugin wrappers we don't control); it
//! shells out to `docker buildx build` with a single-line
//! `FROM <src>` Dockerfile + `--label` flags. Slower (~10s for the
//! buildx warm-up) but functionally equivalent.
//!
//! Both paths produce a byte-identical label set on the resulting
//! image config — the difference is only in how the config blob
//! gets there. `verify` runs `docker inspect` afterwards so the
//! actual on-disk labels are what's compared, not the side-channel
//! we used to write them.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

use thiserror::Error;

/// Patch `image_in` with the supplied label set, writing the
/// result to `image_out`. Tries crane first; falls back to buildx
/// on `crane not found`. Surfaces any non-availability error
/// verbatim — a crane that exists but fails should NOT silently
/// fall back to buildx because that masks broken-registry issues.
pub fn patch_image(
    image_in: &str,
    image_out: &str,
    labels: &BTreeMap<String, String>,
) -> Result<(), PatchError> {
    match crane_mutate(image_in, image_out, labels) {
        Ok(()) => Ok(()),
        Err(PatchError::CraneMissing) => match buildx_label(image_in, image_out, labels) {
            // Both image-patch paths are unavailable — surface the
            // combined missing-tool error rather than only the
            // buildx-side one, because the operator's fix is "install
            // either crane or docker buildx", not just buildx. Maps
            // to exit 7 ("image-patching tool unavailable / failed")
            // in the CLI exit-code table.
            Err(PatchError::BuildxMissing) => Err(PatchError::BothMissing),
            other => other,
        },
        Err(other) => Err(other),
    }
}

/// `crane mutate --label k=v --tag <dst> <src>` — config-blob-only
/// rewrite. The output tag lives locally (we don't push); the
/// `--tag` arg overrides the target tag in the config so the local
/// daemon's `docker image inspect <image_out>` sees the labels.
fn crane_mutate(
    image_in: &str,
    image_out: &str,
    labels: &BTreeMap<String, String>,
) -> Result<(), PatchError> {
    let mut cmd = Command::new("crane");
    cmd.arg("mutate");
    for (k, v) in labels {
        cmd.arg("--label").arg(format!("{k}={v}"));
    }
    cmd.arg("--tag").arg(image_out).arg(image_in);

    let output = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).output() {
        Ok(o) => o,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(PatchError::CraneMissing);
        }
        Err(err) => return Err(PatchError::Io(err.to_string())),
    };

    if !output.status.success() {
        return Err(PatchError::CraneFailed {
            stderr: String::from_utf8_lossy(&output.stderr)
                .trim_end()
                .to_string(),
        });
    }
    Ok(())
}

/// `docker buildx build --label k=v ...` against an inline
/// `FROM <src>` Dockerfile. Used when crane isn't available.
///
/// We pipe the Dockerfile via stdin (`-f -`) so we don't have to
/// create a temp directory; the build context is `.` but the
/// trivial Dockerfile doesn't COPY anything so the context size
/// is moot.
fn buildx_label(
    image_in: &str,
    image_out: &str,
    labels: &BTreeMap<String, String>,
) -> Result<(), PatchError> {
    let dockerfile = format!("FROM {image_in}\n");

    let mut cmd = Command::new("docker");
    cmd.args(["buildx", "build", "--load", "-t", image_out, "-f", "-"]);
    for (k, v) in labels {
        cmd.arg("--label").arg(format!("{k}={v}"));
    }
    // Use an empty build context — `-` after the flags is a
    // tar-on-stdin convention buildx accepts but we don't need
    // since FROM-only has no COPY. Passing `.` from cwd works.
    cmd.arg(".");

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => PatchError::BuildxMissing,
            _ => PatchError::Io(err.to_string()),
        })?;

    child
        .stdin
        .as_mut()
        .expect("stdin piped")
        .write_all(dockerfile.as_bytes())
        .map_err(|err| PatchError::Io(err.to_string()))?;

    let output = child
        .wait_with_output()
        .map_err(|err| PatchError::Io(err.to_string()))?;

    if !output.status.success() {
        return Err(PatchError::BuildxFailed {
            stderr: String::from_utf8_lossy(&output.stderr)
                .trim_end()
                .to_string(),
        });
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum PatchError {
    #[error("neither `crane` nor `docker buildx` is available")]
    BothMissing,

    /// Not directly returned from [`patch_image`] — used by
    /// `crane_mutate` to signal the fallback.
    #[error("crane not on PATH; falling back to buildx")]
    CraneMissing,

    #[error("crane mutate failed: {stderr}")]
    CraneFailed { stderr: String },

    #[error("docker buildx not available on PATH")]
    BuildxMissing,

    #[error("docker buildx label-patch failed: {stderr}")]
    BuildxFailed { stderr: String },

    #[error("io error during image patch: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_have_distinct_display_strings() {
        // Sanity test on the error surface: the operator pages on
        // these strings, so a future refactor that collapses them
        // into a single "patch failed" Display string would lose
        // operator-meaningful detail. Cheap to lock here.
        let a = format!("{}", PatchError::CraneFailed { stderr: "x".into() });
        let b = format!("{}", PatchError::BuildxFailed { stderr: "x".into() });
        let c = format!("{}", PatchError::CraneMissing);
        let d = format!("{}", PatchError::BuildxMissing);
        let e = format!("{}", PatchError::BothMissing);
        for (i, x) in [&a, &b, &c, &d, &e].iter().enumerate() {
            for (j, y) in [&a, &b, &c, &d, &e].iter().enumerate() {
                if i != j {
                    assert_ne!(
                        x, y,
                        "PatchError displays at index {i} and {j} are equal: {x}"
                    );
                }
            }
        }
    }

    #[test]
    fn both_missing_surfaces_when_crane_and_buildx_unavailable() {
        // Force the fallback chain to traverse a missing crane and
        // a missing docker by running with a PATH that contains
        // neither. The fallback in `patch_image` should land on
        // `PatchError::BothMissing` — that's the operator-actionable
        // signal "install one of crane or docker buildx".
        //
        // We can't easily mock `Command::new` here, so the test
        // restricts PATH to a directory we control and that holds
        // neither binary. On the CI runner this is the same
        // RUNNER_TEMP write we use elsewhere; on dev machines tmp
        // serves the same purpose. Skip if we can't make a temp
        // dir (vanishingly rare).
        let Ok(tmp) = tempfile::TempDir::new() else {
            return;
        };
        let prior_path = std::env::var_os("PATH");
        // SAFETY: setting PATH within a single-threaded unit test
        // is fine; we restore it before returning.
        // Restricted PATH = no `crane`, no `docker`. Both branches
        // of the fallback see NotFound and BothMissing surfaces.
        // SAFETY: setting PATH within a single-threaded unit test
        // is fine; we restore it before returning via the
        // PathGuard's Drop impl.
        struct PathGuard(Option<std::ffi::OsString>);
        impl Drop for PathGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => std::env::set_var("PATH", v),
                    None => std::env::remove_var("PATH"),
                }
            }
        }
        let _guard = PathGuard(prior_path);
        std::env::set_var("PATH", tmp.path());

        let mut labels = BTreeMap::new();
        labels.insert("org.botwork.mcp.name".to_string(), "echo".to_string());
        let err = patch_image("in:tag", "out:tag", &labels).unwrap_err();
        assert!(
            matches!(err, PatchError::BothMissing),
            "expected BothMissing, got {err:?}"
        );
    }
}
