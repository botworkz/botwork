use std::io::Write;

use thiserror::Error;

use crate::{bootstrap, mcp_probe, ps};

pub fn dispatch(args: Vec<String>) -> Result<i32, CliError> {
    dispatch_with_writer(args, std::io::stdout())
}

fn dispatch_with_writer<W: Write>(args: Vec<String>, mut writer: W) -> Result<i32, CliError> {
    match args.get(1).map(String::as_str) {
        None | Some("-h") | Some("--help") => {
            print_usage();
            Ok(0)
        }
        Some("version") | Some("--version") | Some("-V") => {
            writeln!(writer, "botctl {}", crate::version_string())
                .expect("failed to write version output");
            Ok(0)
        }
        Some("ps") => {
            ps::run(&args[2..])?;
            Ok(0)
        }
        Some("bootstrap") => {
            // The bootstrap subcommand owns its own argv-parsing, exit-
            // code mapping, and error display; dispatch hands its
            // argv-tail straight in and surfaces whatever exit code it
            // returns. Errors print their own envelope (which includes
            // help text on InvalidUsage), then we map to the documented
            // exit code from the bootstrap module.
            match bootstrap::run(&args[2..]) {
                Ok(code) => Ok(code),
                Err(err) => {
                    let code = err.exit_code();
                    if code != 0 {
                        eprintln!("{err}");
                    } else {
                        // The Usage / --help branch comes back as Err
                        // with exit_code=0 so the help text routes
                        // through Display. Print to stdout, not stderr,
                        // for `--help` ergonomics.
                        println!("{err}");
                    }
                    Ok(code)
                }
            }
        }
        Some("mcp-probe") => {
            // mcp-probe mirrors bootstrap's posture: owns its own
            // argv-tail parsing + exit-code mapping. The dispatch
            // hands the tail straight in; errors are printed on
            // stderr unless they're the Usage branch (exit 0), which
            // goes to stdout so `--help` pipes work like a normal
            // help text. See `mcp_probe::McpProbeError::exit_code`
            // for the full table — matches the RFE-stated codes.
            match mcp_probe::run(&args[2..]) {
                Ok(code) => Ok(code),
                Err(err) => {
                    let code = err.exit_code();
                    if code != 0 {
                        eprintln!("{err}");
                    } else {
                        println!("{err}");
                    }
                    Ok(code)
                }
            }
        }
        Some(other) => Err(CliError::UnknownSubcommand(other.to_string())),
    }
}

fn print_usage() {
    println!("Usage: botctl <SUBCOMMAND>");
    println!();
    println!("Available subcommands:");
    println!("  version    Print the botctl build version");
    println!("  ps         List running botwork sessions");
    println!("  bootstrap  Apply a bootstrap.yaml through api");
    println!("  mcp-probe  Probe an MCP image and generate / verify / describe its labels");
    println!();
    println!("Run `botctl <SUBCOMMAND> --help` for subcommand options.");
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("unknown subcommand '{0}'\n\nUsage: botctl <SUBCOMMAND>\n\nAvailable subcommands:\n  version    Print the botctl build version\n  ps         List running botwork sessions\n  bootstrap  Apply a bootstrap.yaml through api\n  mcp-probe  Probe an MCP image and generate / verify / describe its labels")]
    UnknownSubcommand(String),
    #[error(transparent)]
    Ps(#[from] ps::PsError),
}

impl CliError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::UnknownSubcommand(_) => 2,
            Self::Ps(err) => err.exit_code(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{dispatch_with_writer, CliError};
    use crate::ps::PsError;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn version_queries_print_the_shared_version() {
        for flag in ["version", "--version", "-V"] {
            let mut output = Vec::new();
            let a = args(&["botctl", flag]);
            let code = dispatch_with_writer(a, &mut output).expect("dispatch ok");
            assert_eq!(code, 0);
            assert_eq!(
                String::from_utf8(output).expect("utf8"),
                format!("botctl {}\n", crate::version_string())
            );
        }
    }

    #[test]
    fn no_args_exits_zero() {
        let code = dispatch_with_writer(args(&["botctl"]), Vec::new()).expect("ok");
        assert_eq!(code, 0);
    }

    #[test]
    fn help_flags_exit_zero() {
        for flag in ["-h", "--help"] {
            let code = dispatch_with_writer(args(&["botctl", flag]), Vec::new()).expect("ok");
            assert_eq!(code, 0, "flag {flag} should exit 0");
        }
    }

    #[test]
    fn unknown_subcommand_returns_cli_error_with_exit_2() {
        let err = dispatch_with_writer(args(&["botctl", "frobnicate"]), Vec::new()).unwrap_err();
        assert!(
            matches!(err, CliError::UnknownSubcommand(ref s) if s == "frobnicate"),
            "expected UnknownSubcommand(frobnicate), got {err:?}"
        );
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn unknown_subcommand_display_includes_name_and_usage_hint() {
        let err = CliError::UnknownSubcommand("badcmd".to_string());
        let msg = format!("{err}");
        assert!(msg.contains("badcmd"), "{msg}");
        assert!(msg.contains("botctl"), "{msg}");
    }

    #[test]
    fn ps_extra_args_propagate_as_ps_error() {
        // ps with an unrecognised flag hits PsError::InvalidUsage
        // before any docker call — no container runtime needed.
        let err = dispatch_with_writer(args(&["botctl", "ps", "--extra"]), Vec::new()).unwrap_err();
        assert!(matches!(err, CliError::Ps(PsError::InvalidUsage)));
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn mcp_probe_no_args_exits_zero_via_usage_branch() {
        // mcp-probe with no argv produces McpProbeError::Usage (exit 0 = help).
        // dispatch converts that to Ok(0).
        let code = dispatch_with_writer(args(&["botctl", "mcp-probe"]), Vec::new()).expect("ok");
        assert_eq!(code, 0);
    }

    #[test]
    fn mcp_probe_help_flag_exits_zero() {
        let code =
            dispatch_with_writer(args(&["botctl", "mcp-probe", "--help"]), Vec::new()).expect("ok");
        assert_eq!(code, 0);
    }

    #[test]
    fn mcp_probe_invalid_subcommand_exits_nonzero() {
        // An unrecognised mcp-probe sub-mode maps to InvalidUsage (exit 2).
        let code =
            dispatch_with_writer(args(&["botctl", "mcp-probe", "bogus"]), Vec::new()).expect("ok");
        assert_eq!(code, 2);
    }

    #[test]
    fn bootstrap_help_flag_exits_zero() {
        let code =
            dispatch_with_writer(args(&["botctl", "bootstrap", "--help"]), Vec::new()).expect("ok");
        assert_eq!(code, 0);
    }

    #[test]
    fn bootstrap_dash_h_exits_zero() {
        let code =
            dispatch_with_writer(args(&["botctl", "bootstrap", "-h"]), Vec::new()).expect("ok");
        assert_eq!(code, 0);
    }

    #[test]
    fn bootstrap_unknown_flag_exits_2() {
        // --frobnicate is an unrecognised bootstrap flag → InvalidUsage (exit 2).
        let code = dispatch_with_writer(args(&["botctl", "bootstrap", "--frobnicate"]), Vec::new())
            .expect("ok");
        assert_eq!(code, 2);
    }

    #[test]
    fn cli_error_exit_codes() {
        assert_eq!(CliError::UnknownSubcommand("x".into()).exit_code(), 2);
        assert_eq!(CliError::Ps(PsError::InvalidUsage).exit_code(), 2);
    }
}
