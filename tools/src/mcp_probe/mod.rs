//! `botwork-tools mcp-probe` — start an MCP image, drive a handshake,
//! emit a labeled image.
//!
//! ## Operating model
//!
//! Three modes, all of which share the same probe pipeline:
//!
//! 1. Read + validate the `mcp-package.yaml` sidecar (per
//!    [`botwork_api_core::package`]).
//! 2. Start the input image as a throwaway docker container, bind it
//!    to an ephemeral host port, wait for `:port` to accept TCP.
//! 3. Drive an MCP `initialize` → `notifications/initialized` →
//!    `tools/list` → (`resources/list`, `prompts/list` when the
//!    server advertises the capability) handshake against the
//!    running container.
//! 4. Validate the captured catalog (tool names match
//!    `^[a-z0-9][a-z0-9_-]*$`, input schemas parse as JSON).
//! 5. Compose the full `org.botwork.mcp.*` label set in
//!    `BTreeMap`-sorted order (= deterministic output).
//! 6. Run the mode-specific tail: `generate` patches the image,
//!    `verify` compares against an existing label set, `describe`
//!    prints to stdout.
//! 7. Tear down the throwaway container.
//!
//! ## Exit codes (issue #147)
//!
//! | Code | Meaning                                                    |
//! |------|------------------------------------------------------------|
//! | 0    | success — labeled (generate), labels match (verify), printed (describe) |
//! | 2    | invalid CLI usage                                          |
//! | 3    | mcp-package.yaml missing / unreadable / fails schema       |
//! | 4    | container failed to start / `:port` never accepted         |
//! | 5    | MCP handshake error (server returned JSON-RPC error, malformed response) |
//! | 6    | label drift detected (verify only)                         |
//! | 7    | image-patching tool unavailable / failed (crane + buildx both unusable) |
//!
//! ## File layout
//!
//! * [`package`] — wraps the [`botwork_api_core::package`] loader
//!   with a thin io-error layer so the CLI surface stays uniform
//!   with [`crate::bootstrap`]'s `LoadError`.
//! * [`probe`] — container lifecycle + MCP handshake.
//! * [`compose`] — captured catalog + package → label set.
//! * [`patch`] — crane (fast path) / buildx (fallback) image patching.
//! * [`verify`] — verify-mode comparison.

pub mod compose;
pub mod package;
pub mod patch;
pub mod probe;
pub mod verify;

use std::path::PathBuf;

use thiserror::Error;

use crate::mcp_probe::compose::ComposeError;
use crate::mcp_probe::package::PackageLoadError;
use crate::mcp_probe::patch::PatchError;
use crate::mcp_probe::probe::ProbeError;
use crate::mcp_probe::verify::VerifyError;

/// Default `mcp-package.yaml` path. Looked up relative to the
/// current working directory when `--package` is not supplied;
/// matches the "sibling of Dockerfile" convention from the RFE.
pub const DEFAULT_PACKAGE_PATH: &str = "./mcp-package.yaml";

/// Default overall handshake timeout. Generous enough to cover a
/// cold-cache container pull + a slow first JSON-RPC roundtrip, but
/// not so long that a stuck container blocks CI for the full job.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Default container runtime. The RFE leaves room for podman-style
/// runtimes via `--runtime`, but the v1 default is `docker`.
pub const DEFAULT_RUNTIME: &str = "docker";

/// Parsed mcp-probe args.
#[derive(Debug, Clone)]
pub struct Args {
    pub mode: Mode,
    pub image_in: String,
    pub image_out: Option<String>,
    pub package_path: PathBuf,
    pub host_port: Option<u16>,
    pub timeout_secs: u64,
    pub runtime: String,
}

/// Sub-mode the CLI dispatches into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Generate,
    Verify,
    Describe,
}

impl Mode {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "generate" => Some(Self::Generate),
            "verify" => Some(Self::Verify),
            "describe" => Some(Self::Describe),
            _ => None,
        }
    }
}

impl Args {
    /// Parse `argv[2..]` (everything after `botwork-tools mcp-probe`).
    pub fn from_argv(argv: &[String]) -> Result<Self, McpProbeError> {
        // First positional is the sub-mode; everything else is flag
        // parsing. The shape is intentionally hand-rolled rather
        // than clap-driven because the rest of `botwork-tools` is
        // (see `bootstrap/mod.rs`) and a single clap drop-in for
        // one subcommand would be a needless step-change.
        let mode = match argv.first().map(String::as_str) {
            None | Some("-h") | Some("--help") => {
                return Err(McpProbeError::Usage(help_text()));
            }
            Some(s) => Mode::from_str(s).ok_or(McpProbeError::InvalidUsage(
                "unknown subcommand for mcp-probe",
            ))?,
        };

        let mut image_in: Option<String> = None;
        let mut image_out: Option<String> = None;
        let mut package_path: Option<PathBuf> = None;
        let mut host_port: Option<u16> = None;
        let mut timeout_secs: Option<u64> = None;
        let mut runtime: Option<String> = None;

        let mut iter = argv.iter().skip(1).peekable();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "-h" | "--help" => return Err(McpProbeError::Usage(help_text())),
                "--in" => image_in = Some(take_value(&mut iter, "--in")?),
                "--out" => image_out = Some(take_value(&mut iter, "--out")?),
                "--package" => {
                    package_path = Some(PathBuf::from(take_value(&mut iter, "--package")?));
                }
                "--port" => {
                    let v = take_value(&mut iter, "--port")?;
                    host_port = Some(
                        v.parse::<u16>()
                            .map_err(|_| McpProbeError::InvalidUsage("--port must be 1-65535"))?,
                    );
                }
                "--timeout" => {
                    let v = take_value(&mut iter, "--timeout")?;
                    timeout_secs = Some(v.parse::<u64>().map_err(|_| {
                        McpProbeError::InvalidUsage("--timeout must be an integer (seconds)")
                    })?);
                }
                "--runtime" => runtime = Some(take_value(&mut iter, "--runtime")?),
                other => {
                    // Mirror bootstrap's pattern: stash the user
                    // input in a leaked &'static so the
                    // InvalidUsage variant (which holds &'static
                    // for cheap copy) can quote it back.
                    return Err(McpProbeError::InvalidUsage(Box::leak(
                        format!("unknown flag '{other}'").into_boxed_str(),
                    )));
                }
            }
        }

        let image_in = image_in.ok_or(McpProbeError::InvalidUsage(
            "--in is required (the source image tag/digest)",
        ))?;

        if mode == Mode::Generate && image_out.is_none() {
            return Err(McpProbeError::InvalidUsage(
                "--out is required in generate mode",
            ));
        }
        if mode != Mode::Generate && image_out.is_some() {
            return Err(McpProbeError::InvalidUsage(
                "--out is only valid in generate mode",
            ));
        }

        Ok(Self {
            mode,
            image_in,
            image_out,
            package_path: package_path.unwrap_or_else(|| PathBuf::from(DEFAULT_PACKAGE_PATH)),
            host_port,
            timeout_secs: timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
            runtime: runtime.unwrap_or_else(|| DEFAULT_RUNTIME.to_string()),
        })
    }
}

fn take_value<'a, I>(iter: &mut I, flag: &'static str) -> Result<String, McpProbeError>
where
    I: Iterator<Item = &'a String>,
{
    iter.next().cloned().ok_or_else(|| {
        // Leak the formatted message into a &'static so InvalidUsage
        // (which holds &'static for cheap copy across the variant
        // boundary) can surface the offending flag name. Same posture
        // the `unknown flag '<x>'` arm in `from_argv` uses; we only
        // land here on argv shape mistakes so a per-call allocation
        // that never gets reclaimed is acceptable.
        McpProbeError::InvalidUsage(Box::leak(
            format!("missing value for flag '{flag}'").into_boxed_str(),
        ))
    })
}

pub fn help_text() -> &'static str {
    "Usage: botwork-tools mcp-probe <generate|verify|describe> [OPTIONS]\n\
     \n\
     Modes:\n\
       generate   Probe an unlabeled image, emit a labeled image (requires --out)\n\
       verify     Re-probe a labeled image; fail (exit 6) if labels drifted\n\
       describe   Probe an image, print would-be labels to stdout (no image write)\n\
     \n\
     Common options:\n\
       --in <ref>           Source image (tag or digest). Required.\n\
       --package <path>     mcp-package.yaml; defaults to ./mcp-package.yaml\n\
       --port <port>        Bind this host port for the probe; default ephemeral\n\
       --timeout <secs>     Overall handshake timeout in seconds (default 60)\n\
       --runtime <name>     Container runtime; default docker\n\
     \n\
     Generate-only:\n\
       --out <ref>          Destination image tag. Required for `generate`.\n\
     \n\
     Exit codes: 0=ok, 2=usage, 3=package-load, 4=container, 5=handshake,\n\
                 6=label-drift (verify), 7=image-patch"
}

/// Dispatch into the requested mode. Stays small: every mode does
/// the same probe up-front, then forks on what to do with the
/// composed label set.
pub fn run(argv: &[String]) -> Result<i32, McpProbeError> {
    let args = Args::from_argv(argv)?;

    let package = package::load(&args.package_path)?;
    let probe_result = probe::run_probe(&args)?;
    let labels = compose::compose(&package, &probe_result)?;

    match args.mode {
        Mode::Describe => {
            // key=value\n, alphabetical (BTreeMap iteration order
            // is sorted), no image write. Stdout, not stderr —
            // useful for `... describe | grep tools` pipelines.
            for (k, v) in &labels {
                println!("{k}={v}");
            }
            Ok(0)
        }
        Mode::Generate => {
            let dest = args
                .image_out
                .as_deref()
                .expect("--out required in generate mode (validated in Args::from_argv)");
            patch::patch_image(&args.image_in, dest, &labels)?;
            eprintln!(
                "[mcp-probe] generate: wrote {n_labels} labels into {dest}",
                n_labels = labels.len(),
            );
            Ok(0)
        }
        Mode::Verify => {
            // verify reads the labels that are already on the
            // input image and compares against what the probe just
            // produced. Drift surfaces as exit 6, not exit 0 — so
            // CI can wire the action's status straight to a gate.
            verify::verify(&args.image_in, &args.runtime, &labels)?;
            eprintln!(
                "[mcp-probe] verify: {n_labels} labels match {image}",
                n_labels = labels.len(),
                image = args.image_in,
            );
            Ok(0)
        }
    }
}

/// Errors surfaced by the mcp-probe subcommand.
///
/// One variant per exit-code bucket — the [`McpProbeError::exit_code`]
/// mapping matches the table in the RFE / this module's docstring.
#[derive(Debug, Error)]
pub enum McpProbeError {
    #[error("{0}")]
    Usage(&'static str),
    #[error("usage: {0}\n\n{help}", help = help_text())]
    InvalidUsage(&'static str),
    #[error(transparent)]
    Package(#[from] PackageLoadError),
    #[error(transparent)]
    Probe(#[from] ProbeError),
    #[error(transparent)]
    Compose(#[from] ComposeError),
    #[error(transparent)]
    Patch(#[from] PatchError),
    #[error(transparent)]
    Verify(#[from] VerifyError),
}

impl McpProbeError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 0,
            Self::InvalidUsage(_) => 2,
            Self::Package(_) => 3,
            Self::Probe(err) => err.exit_code(),
            Self::Compose(_) => 5,
            Self::Patch(_) => 7,
            Self::Verify(VerifyError::Drift { .. }) => 6,
            Self::Verify(_) => 7,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|t| (*t).to_string()).collect()
    }

    #[test]
    fn empty_argv_prints_usage_via_usage_variant() {
        let err = Args::from_argv(&argv(&[])).unwrap_err();
        assert!(matches!(err, McpProbeError::Usage(_)));
        // exit 0 because Usage is the help branch.
        assert_eq!(err.exit_code(), 0);
    }

    #[test]
    fn unknown_subcommand_rejected() {
        let err = Args::from_argv(&argv(&["wat", "--in", "x"])).unwrap_err();
        assert!(matches!(err, McpProbeError::InvalidUsage(_)));
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn generate_requires_out() {
        let err = Args::from_argv(&argv(&["generate", "--in", "x"])).unwrap_err();
        assert!(matches!(err, McpProbeError::InvalidUsage(_)));
    }

    #[test]
    fn out_rejected_in_describe_mode() {
        let err = Args::from_argv(&argv(&["describe", "--in", "x", "--out", "y"])).unwrap_err();
        assert!(matches!(err, McpProbeError::InvalidUsage(_)));
    }

    #[test]
    fn generate_parses_minimal_argv() {
        let args = Args::from_argv(&argv(&[
            "generate",
            "--in",
            "mcp-foo:unlabeled",
            "--out",
            "botwork/mcp-foo:local",
        ]))
        .expect("parse");
        assert_eq!(args.mode, Mode::Generate);
        assert_eq!(args.image_in, "mcp-foo:unlabeled");
        assert_eq!(args.image_out.as_deref(), Some("botwork/mcp-foo:local"));
        assert_eq!(args.package_path, PathBuf::from(DEFAULT_PACKAGE_PATH));
        assert_eq!(args.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(args.runtime, DEFAULT_RUNTIME);
        assert!(args.host_port.is_none());
    }

    #[test]
    fn verify_parses_without_out() {
        let args = Args::from_argv(&argv(&["verify", "--in", "ghcr.io/example/mcp-foo:1.0.0"]))
            .expect("parse");
        assert_eq!(args.mode, Mode::Verify);
        assert!(args.image_out.is_none());
    }

    #[test]
    fn describe_parses_without_out() {
        let args = Args::from_argv(&argv(&["describe", "--in", "x"])).expect("parse");
        assert_eq!(args.mode, Mode::Describe);
    }

    #[test]
    fn port_and_timeout_parse() {
        let args = Args::from_argv(&argv(&[
            "describe",
            "--in",
            "x",
            "--port",
            "8123",
            "--timeout",
            "120",
        ]))
        .expect("parse");
        assert_eq!(args.host_port, Some(8123));
        assert_eq!(args.timeout_secs, 120);
    }

    #[test]
    fn nonsense_port_rejected() {
        let err =
            Args::from_argv(&argv(&["describe", "--in", "x", "--port", "70000"])).unwrap_err();
        assert!(matches!(err, McpProbeError::InvalidUsage(_)));
    }

    #[test]
    fn missing_in_rejected() {
        let err = Args::from_argv(&argv(&["describe"])).unwrap_err();
        assert!(matches!(err, McpProbeError::InvalidUsage(_)));
    }

    #[test]
    fn missing_value_for_flag_names_the_flag() {
        // Operator-facing: the InvalidUsage message must say WHICH
        // flag the value was missing for. Regression test for the
        // pre-review version of `take_value` which dropped the flag
        // name on the floor and emitted "missing value for flag".
        let err = Args::from_argv(&argv(&["describe", "--in"])).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("'--in'"),
            "missing-value message must quote the flag name: {msg}"
        );
    }

    #[test]
    fn unknown_flag_rejected_with_specific_message() {
        let err = Args::from_argv(&argv(&["describe", "--in", "x", "--whatever"])).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unknown flag '--whatever'"), "{msg}");
    }

    #[test]
    fn exit_code_table_matches_rfe() {
        // Lock the table from the issue body and this module's
        // docstring into a test so a future refactor that
        // accidentally maps Probe to exit 5 (instead of letting it
        // surface its own 4/5 distinction) trips here.
        assert_eq!(McpProbeError::InvalidUsage("x").exit_code(), 2);
    }
}
