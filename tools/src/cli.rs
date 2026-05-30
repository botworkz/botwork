use thiserror::Error;

use crate::ps;

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
        Some(other) => Err(CliError::UnknownSubcommand(other.to_string())),
    }
}

fn print_usage() {
    println!("Usage: botwork-tools <SUBCOMMAND>");
    println!();
    println!("Available subcommands:");
    println!("  ps     List running botwork sessions");
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("unknown subcommand '{0}'\n\nUsage: botwork-tools <SUBCOMMAND>\n\nAvailable subcommands:\n  ps     List running botwork sessions")]
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
