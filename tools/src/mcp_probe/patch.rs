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

    /// Mutex that serialises every test that mutates the process-global
    /// PATH environment variable. Tests in a single crate share one
    /// process and run concurrently by default; without this guard
    /// they race on `set_var`/`remove_var` and flake unpredictably.
    static PATH_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Helper: write a shell script to `dir/<name>` and make it executable.
    #[cfg(unix)]
    fn write_fake_bin(dir: &std::path::Path, name: &str, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, script).expect("write fake binary");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake binary");
    }

    /// RAII guard that holds `PATH_MUTEX` while also restoring the
    /// original PATH value on drop.  Always construct via `lock_path`.
    struct PathGuard(
        #[allow(dead_code)] std::sync::MutexGuard<'static, ()>,
        Option<std::ffi::OsString>,
    );
    impl Drop for PathGuard {
        fn drop(&mut self) {
            match self.1.take() {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
            // MutexGuard drops after the PATH is restored.
        }
    }

    /// Acquire the PATH mutex and return a guard that restores the
    /// original PATH when dropped.
    fn lock_path() -> PathGuard {
        let guard = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os("PATH");
        PathGuard(guard, prior)
    }

    fn minimal_labels() -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        labels.insert("org.botwork.mcp.name".to_string(), "echo".to_string());
        labels
    }

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
    fn io_error_display_includes_message() {
        let err = PatchError::Io("write failed: disk full".into());
        let msg = format!("{err}");
        assert!(msg.contains("write failed"), "{msg}");
        assert!(msg.contains("disk full"), "{msg}");
    }

    #[test]
    fn crane_failed_display_includes_stderr() {
        let err = PatchError::CraneFailed {
            stderr: "registry auth failed".into(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("registry auth failed"), "{msg}");
    }

    #[test]
    fn buildx_failed_display_includes_stderr() {
        let err = PatchError::BuildxFailed {
            stderr: "buildx daemon not running".into(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("buildx daemon not running"), "{msg}");
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
        let _guard = lock_path();
        std::env::set_var("PATH", tmp.path());

        let err = patch_image("in:tag", "out:tag", &minimal_labels()).unwrap_err();
        assert!(
            matches!(err, PatchError::BothMissing),
            "expected BothMissing, got {err:?}"
        );
    }

    // ── crane_mutate: success and failure paths ─────────────────────────────

    #[cfg(unix)]
    #[test]
    fn crane_mutate_succeeds_when_crane_exits_zero() {
        let Ok(tmp) = tempfile::TempDir::new() else {
            return;
        };
        write_fake_bin(tmp.path(), "crane", "#!/bin/sh\nexit 0\n");
        let _guard = lock_path();
        std::env::set_var("PATH", tmp.path());

        let result = crane_mutate("in:tag", "out:tag", &minimal_labels());
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[cfg(unix)]
    #[test]
    fn crane_mutate_returns_crane_failed_when_crane_exits_nonzero() {
        let Ok(tmp) = tempfile::TempDir::new() else {
            return;
        };
        write_fake_bin(
            tmp.path(),
            "crane",
            "#!/bin/sh\necho 'registry error' >&2\nexit 1\n",
        );
        let _guard = lock_path();
        std::env::set_var("PATH", tmp.path());

        let err = crane_mutate("in:tag", "out:tag", &minimal_labels()).unwrap_err();
        assert!(
            matches!(err, PatchError::CraneFailed { .. }),
            "expected CraneFailed, got {err:?}"
        );
        let msg = format!("{err}");
        assert!(msg.contains("registry error"), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn crane_mutate_returns_crane_missing_when_not_on_path() {
        let Ok(tmp) = tempfile::TempDir::new() else {
            return;
        };
        // Empty tempdir: no crane binary at all
        let _guard = lock_path();
        std::env::set_var("PATH", tmp.path());

        let err = crane_mutate("in:tag", "out:tag", &minimal_labels()).unwrap_err();
        assert!(
            matches!(err, PatchError::CraneMissing),
            "expected CraneMissing, got {err:?}"
        );
    }

    // ── buildx_label: success and failure paths ─────────────────────────────

    #[cfg(unix)]
    #[test]
    fn buildx_label_succeeds_when_docker_exits_zero() {
        let Ok(tmp) = tempfile::TempDir::new() else {
            return;
        };
        // The buildx_label fn runs `docker buildx build ...` and pipes a
        // Dockerfile via stdin. The fake docker just exits 0 regardless.
        write_fake_bin(tmp.path(), "docker", "#!/bin/sh\nexit 0\n");
        let _guard = lock_path();
        std::env::set_var("PATH", tmp.path());

        let result = buildx_label("in:tag", "out:tag", &minimal_labels());
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[cfg(unix)]
    #[test]
    fn buildx_label_returns_buildx_failed_when_docker_exits_nonzero() {
        let Ok(tmp) = tempfile::TempDir::new() else {
            return;
        };
        write_fake_bin(
            tmp.path(),
            "docker",
            "#!/bin/sh\necho 'buildx daemon not running' >&2\nexit 1\n",
        );
        let _guard = lock_path();
        std::env::set_var("PATH", tmp.path());

        let err = buildx_label("in:tag", "out:tag", &minimal_labels()).unwrap_err();
        assert!(
            matches!(err, PatchError::BuildxFailed { .. }),
            "expected BuildxFailed, got {err:?}"
        );
        let msg = format!("{err}");
        assert!(msg.contains("buildx daemon not running"), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn buildx_label_returns_buildx_missing_when_docker_not_on_path() {
        let Ok(tmp) = tempfile::TempDir::new() else {
            return;
        };
        // Empty tempdir: no docker binary
        let _guard = lock_path();
        std::env::set_var("PATH", tmp.path());

        let err = buildx_label("in:tag", "out:tag", &minimal_labels()).unwrap_err();
        assert!(
            matches!(err, PatchError::BuildxMissing),
            "expected BuildxMissing, got {err:?}"
        );
    }

    // ── patch_image: fallback chain ─────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn patch_image_uses_crane_when_available_and_succeeds() {
        let Ok(tmp) = tempfile::TempDir::new() else {
            return;
        };
        write_fake_bin(tmp.path(), "crane", "#!/bin/sh\nexit 0\n");
        let _guard = lock_path();
        std::env::set_var("PATH", tmp.path());

        let result = patch_image("in:tag", "out:tag", &minimal_labels());
        assert!(
            result.is_ok(),
            "expected Ok when crane succeeds: {result:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn patch_image_falls_back_to_buildx_when_crane_missing() {
        let Ok(tmp) = tempfile::TempDir::new() else {
            return;
        };
        // No crane; docker buildx exits 0
        write_fake_bin(tmp.path(), "docker", "#!/bin/sh\nexit 0\n");
        let _guard = lock_path();
        std::env::set_var("PATH", tmp.path());

        let result = patch_image("in:tag", "out:tag", &minimal_labels());
        assert!(
            result.is_ok(),
            "expected Ok via buildx fallback: {result:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn patch_image_surfaces_crane_failure_without_falling_back() {
        // crane exists but fails → should NOT fall back to buildx, but
        // surface the crane error directly.
        let Ok(tmp) = tempfile::TempDir::new() else {
            return;
        };
        write_fake_bin(
            tmp.path(),
            "crane",
            "#!/bin/sh\necho 'push failed' >&2\nexit 1\n",
        );
        write_fake_bin(tmp.path(), "docker", "#!/bin/sh\nexit 0\n");
        let _guard = lock_path();
        std::env::set_var("PATH", tmp.path());

        let err = patch_image("in:tag", "out:tag", &minimal_labels()).unwrap_err();
        assert!(
            matches!(err, PatchError::CraneFailed { .. }),
            "expected CraneFailed (no fallback to buildx when crane fails), got {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn patch_image_buildx_fallback_failure_returns_buildx_failed() {
        let Ok(tmp) = tempfile::TempDir::new() else {
            return;
        };
        // No crane; docker buildx fails
        write_fake_bin(
            tmp.path(),
            "docker",
            "#!/bin/sh\necho 'daemon error' >&2\nexit 1\n",
        );
        let _guard = lock_path();
        std::env::set_var("PATH", tmp.path());

        let err = patch_image("in:tag", "out:tag", &minimal_labels()).unwrap_err();
        assert!(
            matches!(err, PatchError::BuildxFailed { .. }),
            "expected BuildxFailed, got {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn crane_mutate_passes_all_labels_as_flag_pairs() {
        // Verify that for N labels, 2×N `--label k=v` flag pairs appear
        // in the args the fake crane receives (via its stdin/argv).
        // The fake crane prints its argv to stdout for inspection.
        let Ok(tmp) = tempfile::TempDir::new() else {
            return;
        };
        let script_path = tmp.path().join("crane_args.txt");
        let script_path_str = script_path.to_string_lossy();
        write_fake_bin(
            tmp.path(),
            "crane",
            &format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {script_path_str}\nexit 0\n"),
        );
        let _guard = lock_path();
        std::env::set_var("PATH", tmp.path());

        let mut labels = BTreeMap::new();
        labels.insert("org.botwork.mcp.name".to_string(), "echo".to_string());
        labels.insert("org.botwork.mcp.port".to_string(), "8000".to_string());
        crane_mutate("in:tag", "out:tag", &labels).expect("should succeed");

        let written = std::fs::read_to_string(&script_path).expect("crane_args.txt");
        assert!(written.contains("--label"), "args must include --label");
        assert!(
            written.contains("org.botwork.mcp.name=echo"),
            "must include name label"
        );
        assert!(
            written.contains("org.botwork.mcp.port=8000"),
            "must include port label"
        );
        assert!(written.contains("--tag"), "args must include --tag out:tag");
    }
}
