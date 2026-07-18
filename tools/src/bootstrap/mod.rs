//! `botwork-tools bootstrap` — operator-facing replacement for the
//! `botwork-bootstrap` boot-time binary.
//!
//! Parses the same `bootstrap.yaml` shape via
//! [`botwork_api_core::BootstrapConfig`], then POSTs/PUTs each
//! validated row through `botwork-api` instead of writing
//! sea-orm directly. The end-state behaviour matches bootstrap
//! exactly: idempotent upsert against `(tenant, workspace,
//! plugin, workspace_plugin)`.
//!
//! # Cutover plan
//!
//! Round 1 (this commit): adds the subcommand, leaves the old
//! `botwork-bootstrap` crate + systemd unit in place. Round 2 (vm
//! follow-up PR) drops the `botwork-bootstrap.service` unit and
//! replaces it with a oneshot calling `botwork-tools bootstrap`.
//! Round 3 (botwork follow-up PR) deletes the `botwork-bootstrap`
//! crate, container, and CI entries.
//!
//! # Exit codes (matches `botwork-bootstrap` for systemd swap-in)
//!
//! | Code | Meaning                                                    |
//! |------|------------------------------------------------------------|
//! | 0    | apply succeeded (no-op or mutations both count as success) |
//! | 2    | invalid CLI usage                                          |
//! | 4    | bootstrap config file missing / read failure               |
//! | 5    | bootstrap config validation failure                        |
//! | 6    | api write failed mid-apply                           |
//! | 7    | api unreachable / 5xx                                |
//!
//! # CLI shape
//!
//! ```text
//! botwork-tools bootstrap [--config <path>] [--endpoint <url>]
//!                         [--operator <name>] [--dry-run]
//! ```
//!
//! Defaults match the old bootstrap binary's env contract:
//!
//! * `--config` — `BOTWORK_BOOTSTRAP_CONFIG` or
//!   `/etc/botwork/bootstrap.yaml`.
//! * `--endpoint` — `BOTWORK_API_ENDPOINT` or
//!   `http://admin_api:9400` (the docker alias `admin_api` binds on
//!   `botwork-internal`).
//! * `--operator` — defaults to `bootstrap-import`. Sent in the
//!   `x-botwork-admin` header so api's audit log distinguishes
//!   machine-driven imports from operator UI writes.
//! * `--dry-run` — validate yaml + plan diffs but issue no writes.
//!   Exit 0 if the plan would succeed, exit 6 if anything in the
//!   plan would be a no-op-on-failure.
//!
//! See [`apply`] for the apply algorithm.

pub mod apply;
pub mod client;

use std::path::PathBuf;

use botwork_api_core::config::LoadError;
use thiserror::Error;
use tracing::info;

use crate::bootstrap::apply::ApplyOutcome;
use crate::bootstrap::client::AdminClient;

/// Default endpoint for api on the production `botwork-internal`
/// docker network. The systemd unit overrides via `--endpoint` only if
/// the alias changes; the default keeps the tool usable inside the
/// existing fleet without configuration.
pub const DEFAULT_ENDPOINT: &str = "http://admin_api:9400";

/// Default config path matching the old bootstrap binary; the systemd
/// oneshot renders bootstrap.yaml into `/etc/botwork`.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/botwork/bootstrap.yaml";

/// Default operator identity for the audit log.
pub const DEFAULT_OPERATOR: &str = "bootstrap-import";

/// Env-var overrides recognised by [`Args::from_argv`] when the
/// corresponding flag is absent. These mirror the env vars the old
/// bootstrap binary honoured so the systemd cutover is a 1:1 swap.
pub const CONFIG_PATH_ENV: &str = "BOTWORK_BOOTSTRAP_CONFIG";
pub const ENDPOINT_ENV: &str = "BOTWORK_API_ENDPOINT";

/// Parsed bootstrap-subcommand args.
#[derive(Debug, Clone)]
pub struct Args {
    pub config_path: PathBuf,
    pub endpoint: String,
    pub operator: String,
    pub dry_run: bool,
}

impl Args {
    /// Parse `argv[2..]` (everything after `botwork-tools bootstrap`).
    pub fn from_argv(argv: &[String]) -> Result<Self, BootstrapError> {
        let mut config_path: Option<PathBuf> = None;
        let mut endpoint: Option<String> = None;
        let mut operator: Option<String> = None;
        let mut dry_run = false;

        let mut iter = argv.iter().peekable();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "-h" | "--help" => return Err(BootstrapError::Usage(help_text())),
                "--config" => {
                    let v = iter
                        .next()
                        .ok_or(BootstrapError::InvalidUsage("--config requires a value"))?;
                    config_path = Some(PathBuf::from(v));
                }
                "--endpoint" => {
                    let v = iter
                        .next()
                        .ok_or(BootstrapError::InvalidUsage("--endpoint requires a value"))?;
                    endpoint = Some(v.clone());
                }
                "--operator" => {
                    let v = iter
                        .next()
                        .ok_or(BootstrapError::InvalidUsage("--operator requires a value"))?;
                    operator = Some(v.clone());
                }
                "--dry-run" => dry_run = true,
                other => {
                    return Err(BootstrapError::InvalidUsage(Box::leak(
                        format!("unknown flag '{other}'").into_boxed_str(),
                    )));
                }
            }
        }

        Ok(Self {
            config_path: config_path.unwrap_or_else(|| {
                std::env::var(CONFIG_PATH_ENV)
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| PathBuf::from(DEFAULT_CONFIG_PATH))
            }),
            endpoint: endpoint.unwrap_or_else(|| {
                std::env::var(ENDPOINT_ENV).unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string())
            }),
            operator: operator.unwrap_or_else(|| DEFAULT_OPERATOR.to_string()),
            dry_run,
        })
    }
}

pub fn help_text() -> &'static str {
    "Usage: botwork-tools bootstrap [--config <path>] [--endpoint <url>]\n\
     \x20                              [--operator <name>] [--dry-run]\n\
     \n\
     Apply a bootstrap.yaml through api. Idempotent: every operation\n\
     is an upsert. Same yaml shape the legacy botwork-bootstrap binary\n\
     consumed; the only difference is the writer side talks HTTP+JSON to\n\
     api instead of sea-orm-writing the DB directly.\n\
     \n\
     Defaults:\n\
       --config    BOTWORK_BOOTSTRAP_CONFIG or /etc/botwork/bootstrap.yaml\n\
       --endpoint  BOTWORK_API_ENDPOINT or http://admin_api:9400\n\
       --operator  bootstrap-import\n\
     \n\
     Exit codes: 0=ok, 2=usage, 4=file-io, 5=validation, 6=apply, 7=transport"
}

/// Entry point dispatched from `cli::dispatch`.
pub fn run(argv: &[String]) -> Result<i32, BootstrapError> {
    let args = Args::from_argv(argv)?;
    let cfg = botwork_api_core::BootstrapConfig::load(&args.config_path)?;
    let client = AdminClient::new(&args.endpoint, &args.operator)?;
    let outcome = apply::apply(&client, &cfg, args.dry_run)?;
    print_summary(&outcome, args.dry_run);
    Ok(0)
}

fn print_summary(outcome: &ApplyOutcome, dry_run: bool) {
    info!("{}", summary_message(outcome, dry_run));
}

fn summary_message(outcome: &ApplyOutcome, dry_run: bool) -> String {
    let verb = if dry_run { "would apply" } else { "applied" };
    format!(
        "[bootstrap] {verb}: tenants={}/{} workspaces={}/{} plugins={}/{} bindings={}/{}",
        outcome.tenants_created,
        outcome.tenants_total,
        outcome.workspaces_created,
        outcome.workspaces_total,
        outcome.plugins_created,
        outcome.plugins_total,
        outcome.bindings_created,
        outcome.bindings_total,
    )
}

/// Errors emitted by the bootstrap subcommand.
///
/// Variants are organised so each maps cleanly to one exit code in
/// [`BootstrapError::exit_code`] — the same exit-code contract the
/// legacy `botwork-bootstrap` binary surfaced.
#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error("{0}")]
    Usage(&'static str),
    #[error("usage: {0}\n\n{help}", help = help_text())]
    InvalidUsage(&'static str),
    #[error(transparent)]
    Load(#[from] LoadError),
    #[error(transparent)]
    Client(#[from] client::ClientError),
    #[error(transparent)]
    Apply(#[from] apply::ApplyError),
}

impl BootstrapError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 0,
            Self::InvalidUsage(_) => 2,
            Self::Load(LoadError::NotFound(_)) | Self::Load(LoadError::Read { .. }) => 4,
            Self::Load(LoadError::Parse(_)) | Self::Load(LoadError::Validation(_)) => 5,
            Self::Apply(_) => 6,
            Self::Client(client::ClientError::Transport(_)) => 7,
            Self::Client(_) => 6,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::summary_message;
    use crate::bootstrap::apply::ApplyOutcome;

    #[test]
    fn summary_message_preserves_operator_facing_text() {
        let outcome = ApplyOutcome {
            tenants_total: 2,
            tenants_created: 1,
            workspaces_total: 3,
            workspaces_created: 2,
            plugins_total: 4,
            plugins_created: 1,
            plugins_updated: 0,
            bindings_total: 5,
            bindings_created: 2,
            bindings_updated: 0,
        };

        assert_eq!(
            summary_message(&outcome, false),
            "[bootstrap] applied: tenants=1/2 workspaces=2/3 plugins=1/4 bindings=2/5"
        );
        assert_eq!(
            summary_message(&outcome, true),
            "[bootstrap] would apply: tenants=1/2 workspaces=2/3 plugins=1/4 bindings=2/5"
        );
    }
}
