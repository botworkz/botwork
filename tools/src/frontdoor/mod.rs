//! `botwork-tools frontdoor` — VM-local control for envoy frontdoor's
//! RDS "spigot" file.
//!
//! This subcommand is deliberately filesystem-only so it keeps working
//! when api/control-plane/postgres are down or masked. Frontdoor exists
//! exactly for that independence.
//!
//! Open/close never try to infer live state from on-disk yaml. They
//! always write+rename the RDS file, then observe the host-published
//! `:80` probe to confirm the served state changed.
//!
//! # CLI shape
//!
//! ```text
//! botwork-tools frontdoor open   [--rds-dir <path>] [--probe-url <url>] [--no-wait] [--timeout <secs>]
//! botwork-tools frontdoor close  [--rds-dir <path>] [--probe-url <url>] [--no-wait] [--timeout <secs>]
//! botwork-tools frontdoor status [--probe-url <url>] [--timeout <secs>]
//! ```

pub mod probe;
pub mod rds;

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

use crate::frontdoor::probe::State;
use crate::frontdoor::rds::{RdsError, HOLDING_RDS, INGRESS_RDS};

pub const DEFAULT_RDS_DIR: &str = "/etc/botwork/envoy/frontdoor/rds";
pub const DEFAULT_PROBE_URL: &str = "http://127.0.0.1/";
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

pub const RDS_DIR_ENV: &str = "BOTWORK_TOOLS_FRONTDOOR_RDS_DIR";
pub const PROBE_URL_ENV: &str = "BOTWORK_TOOLS_FRONTDOOR_PROBE_URL";
pub const TIMEOUT_ENV: &str = "BOTWORK_TOOLS_FRONTDOOR_TIMEOUT";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub action: Action,
    pub rds_dir: PathBuf,
    pub probe_url: String,
    pub no_wait: bool,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Open,
    Close,
    Status,
}

impl Args {
    pub fn from_argv(argv: &[String]) -> Result<Self, FrontdoorError> {
        let action = match argv.first().map(String::as_str) {
            None | Some("-h") | Some("--help") => return Err(FrontdoorError::Usage(help_text())),
            Some("open") => Action::Open,
            Some("close") => Action::Close,
            Some("status") => Action::Status,
            Some(other) => {
                return Err(FrontdoorError::InvalidUsage(format!(
                    "unknown frontdoor subcommand '{other}'"
                )));
            }
        };

        let mut rds_dir: Option<PathBuf> = None;
        let mut probe_url: Option<String> = None;
        let mut no_wait = false;
        let mut timeout_secs: Option<u64> = None;

        let mut iter = argv[1..].iter().peekable();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    return Err(FrontdoorError::Usage(match action {
                        Action::Open => open_help_text(),
                        Action::Close => close_help_text(),
                        Action::Status => status_help_text(),
                    }));
                }
                "--rds-dir" => {
                    let value = iter.next().ok_or_else(|| {
                        FrontdoorError::InvalidUsage("--rds-dir requires a value".to_string())
                    })?;
                    rds_dir = Some(PathBuf::from(value));
                }
                "--probe-url" => {
                    let value = iter.next().ok_or_else(|| {
                        FrontdoorError::InvalidUsage("--probe-url requires a value".to_string())
                    })?;
                    probe_url = Some(value.clone());
                }
                "--no-wait" => {
                    if matches!(action, Action::Status) {
                        return Err(FrontdoorError::InvalidUsage(
                            "--no-wait is only valid for open/close".to_string(),
                        ));
                    }
                    no_wait = true;
                }
                "--timeout" => {
                    let value = iter.next().ok_or_else(|| {
                        FrontdoorError::InvalidUsage("--timeout requires a value".to_string())
                    })?;
                    timeout_secs = Some(parse_timeout(value)?);
                }
                other => {
                    return Err(FrontdoorError::InvalidUsage(format!(
                        "unknown flag '{other}'"
                    )));
                }
            }
        }

        if matches!(action, Action::Status) && rds_dir.is_some() {
            return Err(FrontdoorError::InvalidUsage(
                "--rds-dir is only valid for open/close".to_string(),
            ));
        }

        Ok(Self {
            action,
            rds_dir: rds_dir.unwrap_or_else(|| {
                std::env::var(RDS_DIR_ENV)
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| PathBuf::from(DEFAULT_RDS_DIR))
            }),
            probe_url: probe_url.unwrap_or_else(|| {
                std::env::var(PROBE_URL_ENV).unwrap_or_else(|_| DEFAULT_PROBE_URL.to_string())
            }),
            no_wait,
            timeout_secs: timeout_secs.unwrap_or(env_timeout_or_default()?),
        })
    }
}

fn parse_timeout(raw: &str) -> Result<u64, FrontdoorError> {
    raw.parse::<u64>()
        .map_err(|_| FrontdoorError::InvalidUsage(format!("invalid timeout '{raw}'")))
}

fn env_timeout_or_default() -> Result<u64, FrontdoorError> {
    match std::env::var(TIMEOUT_ENV) {
        Ok(v) => parse_timeout(&v),
        Err(_) => Ok(DEFAULT_TIMEOUT_SECS),
    }
}

pub fn run(argv: &[String]) -> Result<i32, FrontdoorError> {
    run_with_writer(argv, &mut std::io::stdout())
}

pub fn run_with_writer<W: Write>(argv: &[String], writer: &mut W) -> Result<i32, FrontdoorError> {
    let args = Args::from_argv(argv)?;

    match args.action {
        Action::Open => {
            rds::write_rds(&args.rds_dir, INGRESS_RDS)?;
            if !args.no_wait {
                probe::poll_until_marker_absent(
                    &args.probe_url,
                    Duration::from_secs(args.timeout_secs),
                )?;
            }
            Ok(0)
        }
        Action::Close => {
            rds::write_rds(&args.rds_dir, HOLDING_RDS)?;
            if !args.no_wait {
                probe::poll_until_marker_present(
                    &args.probe_url,
                    Duration::from_secs(args.timeout_secs),
                )?;
            }
            Ok(0)
        }
        Action::Status => {
            let state = probe::classify_once_with_timeout(
                &args.probe_url,
                Duration::from_secs(args.timeout_secs),
            );
            match state {
                State::Open => {
                    writeln!(writer, "open").expect("failed to write frontdoor status to output");
                    Ok(0)
                }
                State::Closed => {
                    writeln!(writer, "closed").expect("failed to write frontdoor status to output");
                    Ok(0)
                }
                State::Unknown => {
                    writeln!(writer, "unknown")
                        .expect("failed to write frontdoor status to output");
                    Ok(3)
                }
            }
        }
    }
}

fn help_text() -> &'static str {
    "Usage: botwork-tools frontdoor <open|close|status> [OPTIONS]\n\
     \n\
     botwork-tools frontdoor open   [--rds-dir <path>] [--probe-url <url>] [--no-wait] [--timeout <secs>]\n\
     botwork-tools frontdoor close  [--rds-dir <path>] [--probe-url <url>] [--no-wait] [--timeout <secs>]\n\
     botwork-tools frontdoor status [--probe-url <url>] [--timeout <secs>]\n\
     \n\
     Defaults:\n\
       --rds-dir    BOTWORK_TOOLS_FRONTDOOR_RDS_DIR or /etc/botwork/envoy/frontdoor/rds\n\
       --probe-url  BOTWORK_TOOLS_FRONTDOOR_PROBE_URL or http://127.0.0.1/\n\
       --timeout    BOTWORK_TOOLS_FRONTDOOR_TIMEOUT or 30\n\
     \n\
     Exit codes: 0=ok, 2=usage, 3=status-unknown, 4=filesystem, 5=timeout"
}

fn open_help_text() -> &'static str {
    "Usage: botwork-tools frontdoor open [--rds-dir <path>] [--probe-url <url>] [--no-wait] [--timeout <secs>]"
}

fn close_help_text() -> &'static str {
    "Usage: botwork-tools frontdoor close [--rds-dir <path>] [--probe-url <url>] [--no-wait] [--timeout <secs>]"
}

fn status_help_text() -> &'static str {
    "Usage: botwork-tools frontdoor status [--probe-url <url>] [--timeout <secs>]"
}

#[derive(Debug, Error)]
pub enum FrontdoorError {
    #[error("{0}")]
    Usage(&'static str),
    #[error("usage: {0}\n\n{help}", help = help_text())]
    InvalidUsage(String),
    #[error(transparent)]
    Rds(#[from] RdsError),
    #[error(transparent)]
    Probe(#[from] probe::ProbeError),
}

impl FrontdoorError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 0,
            Self::InvalidUsage(_) => 2,
            Self::Rds(_) => 4,
            Self::Probe(probe::ProbeError::TimedOut { .. }) => 5,
            Self::Probe(_) => 3,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    use super::{
        Action, Args, DEFAULT_PROBE_URL, DEFAULT_RDS_DIR, DEFAULT_TIMEOUT_SECS, RDS_DIR_ENV,
    };

    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|v| (*v).to_string()).collect()
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().expect("lock")
    }

    #[test]
    fn parses_open_with_defaults() {
        let args = Args::from_argv(&argv(&["open"])).expect("parse");
        assert_eq!(args.action, Action::Open);
        assert_eq!(args.rds_dir, PathBuf::from(DEFAULT_RDS_DIR));
        assert_eq!(args.probe_url, DEFAULT_PROBE_URL);
        assert_eq!(args.timeout_secs, DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn parses_status() {
        let args = Args::from_argv(&argv(&["status"])).expect("parse");
        assert_eq!(args.action, Action::Status);
    }

    #[test]
    fn rejects_unknown_subcommand() {
        let err = Args::from_argv(&argv(&["wat"])).expect_err("must fail");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn rejects_unknown_flag() {
        let err = Args::from_argv(&argv(&["open", "--bogus"])).expect_err("must fail");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn requires_values_for_value_flags() {
        for args in [
            argv(&["open", "--rds-dir"]),
            argv(&["open", "--probe-url"]),
            argv(&["open", "--timeout"]),
            argv(&["status", "--probe-url"]),
            argv(&["status", "--timeout"]),
        ] {
            let err = Args::from_argv(&args).expect_err("must fail");
            assert_eq!(err.exit_code(), 2);
        }
    }

    #[test]
    fn env_overrides_defaults() {
        let _guard = env_lock();
        std::env::set_var(RDS_DIR_ENV, "/foo");
        let args = Args::from_argv(&argv(&["open"])).expect("parse");
        std::env::remove_var(RDS_DIR_ENV);

        assert_eq!(args.rds_dir, PathBuf::from("/foo"));
    }
}
