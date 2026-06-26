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
            writeln!(writer, "botwork-tools {}", crate::version_string())
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
    println!("Usage: botwork-tools <SUBCOMMAND>");
    println!();
    println!("Available subcommands:");
    println!("  version    Print the botwork-tools build version");
    println!("  ps         List running botwork sessions");
    println!("  bootstrap  Apply a bootstrap.yaml through admin-api");
    println!("  mcp-probe  Probe an MCP image and generate / verify / describe its labels");
    println!();
    println!("Run `botwork-tools <SUBCOMMAND> --help` for subcommand options.");
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("unknown subcommand '{0}'\n\nUsage: botwork-tools <SUBCOMMAND>\n\nAvailable subcommands:\n  version    Print the botwork-tools build version\n  ps         List running botwork sessions\n  bootstrap  Apply a bootstrap.yaml through admin-api\n  mcp-probe  Probe an MCP image and generate / verify / describe its labels")]
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
    use super::dispatch_with_writer;

    #[test]
    fn version_queries_print_the_shared_version() {
        for flag in ["version", "--version", "-V"] {
            let mut output = Vec::new();
            let args = vec!["botwork-tools".to_string(), flag.to_string()];
            let code = dispatch_with_writer(args, &mut output).expect("dispatch ok");
            assert_eq!(code, 0);
            assert_eq!(
                String::from_utf8(output).expect("utf8"),
                format!("botwork-tools {}\n", crate::version_string())
            );
        }
    }
}
