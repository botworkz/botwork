use thiserror::Error;

use crate::{bootstrap, ps};

pub fn dispatch(args: Vec<String>) -> Result<i32, CliError> {
    match args.get(1).map(String::as_str) {
        None | Some("-h") | Some("--help") => {
            print_usage();
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
        Some(other) => Err(CliError::UnknownSubcommand(other.to_string())),
    }
}

fn print_usage() {
    println!("Usage: botwork-tools <SUBCOMMAND>");
    println!();
    println!("Available subcommands:");
    println!("  ps         List running botwork sessions");
    println!("  bootstrap  Apply a bootstrap.yaml through admin-api");
    println!();
    println!("Run `botwork-tools <SUBCOMMAND> --help` for subcommand options.");
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("unknown subcommand '{0}'\n\nUsage: botwork-tools <SUBCOMMAND>\n\nAvailable subcommands:\n  ps         List running botwork sessions\n  bootstrap  Apply a bootstrap.yaml through admin-api")]
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
