//! `botwork-tools ps` — list live `mcp_session_*` containers.
//!
//! Reads the operator-visible view from session-broker's admin
//! `GET /sessions` endpoint (rendered from the broker's in-memory
//! `transport_sessions` map) and prints the same `ID CONTAINER
//! AGENT IMAGE AGE` table this command has always rendered.
//!
//! Pre-RFE-#105-round-3 the data came from
//! `/var/lib/botwork/sessions.json` and `docker ps` together; the
//! JSON file was retired in botwork 0.3.5 (#116, #117) and the
//! host-side bind mount in vm 0.4.10. After the round-3 cleanup
//! (vm#121 / this PR) the JSON is gone for good and `docker ps`
//! alone can't surface the (tenant, workspace, plugin, agent-id)
//! fields the table promised.
//!
//! Two read shapes were considered:
//!
//! 1. `docker exec botwork-postgres psql …` and join the entity
//!    tables directly. This duplicates the JOIN api already
//!    runs, requires the postgres password from
//!    `/var/lib/botwork-db/secret.env` (a root-only file), and
//!    couples the operator tool to the schema rather than the
//!    wire contract.
//! 2. HTTP `GET` against session-broker's admin endpoint. The
//!    endpoint already exists (`session-broker/src/admin.rs`),
//!    serves the exact shape this tool wants, and was kept
//!    explicitly to back this command — see the comment in
//!    `admin.rs`: "Anything that needs to observe in-memory state
//!    from outside the broker still uses this `GET /sessions`
//!    view (`botwork-tools ps` reads it)."
//!
//! We picked (2). The broker endpoint is on `botwork-internal`
//! only (trust boundary = docker network membership, same posture
//! as every other broker-to-broker call), and bot can reach it
//! from the host by binding through `botwork-launcher`'s sibling
//! container or — more simply — by running this command inside a
//! sibling container on the docker network. For host SSH callers,
//! the default endpoint expects the docker bridge IP to be
//! reachable from the host; the existing `BOTWORK_TOOLS_SESSIONS_URL`
//! env var (introduced in this PR) lets operators override when
//! they're hitting the broker via a port-forward or off-host.

pub mod docker;
pub mod render;
pub mod sessions;

use thiserror::Error;

use crate::ps::docker::DockerError;
use crate::ps::sessions::SessionsError;

const DEFAULT_SESSIONS_URL: &str = "http://session_broker:9002/sessions";
const SESSIONS_URL_ENV: &str = "BOTWORK_TOOLS_SESSIONS_URL";

pub fn run(args: &[String]) -> Result<(), PsError> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_usage();
        return Ok(());
    }

    if !args.is_empty() {
        return Err(PsError::InvalidUsage);
    }

    let sessions_url =
        std::env::var(SESSIONS_URL_ENV).unwrap_or_else(|_| DEFAULT_SESSIONS_URL.to_string());

    // The broker keeps the same shape it always did even when the
    // map is empty (`{"sessions": {}}`), so we don't special-case
    // "no rows yet" — the table renders with a header and no
    // body lines, which is what the pre-cutover tool also did when
    // `docker ps` returned nothing.
    let sessions = sessions::fetch(&sessions_url)?;
    let running = docker::list_running_sessions()?;

    let rows = build_rows(running, &sessions);
    print!("{}", render::render_table(&rows));
    Ok(())
}

/// Map a list of running containers and the broker's session view
/// into the [`render::TableRow`] list that [`run`] prints.
///
/// Extracted for unit-testability: the logic is pure given the two
/// inputs; the actual I/O (docker + HTTP) lives in the callers.
pub(crate) fn build_rows(
    running: Vec<docker::RunningContainer>,
    sessions: &std::collections::BTreeMap<String, sessions::SessionView>,
) -> Vec<render::TableRow> {
    let mut rows = Vec::with_capacity(running.len());
    for container in running {
        let (agent, image) = match sessions.get(&container.name) {
            Some(entry) => (
                entry
                    .agent_id
                    .clone()
                    .unwrap_or_else(|| "(unbound)".to_string()),
                entry.plugin.clone(),
            ),
            // Container is running but the broker doesn't know
            // about it — either it crashed pre-bind (so no
            // transport entry was ever installed) or it's a
            // hand-launched container that wandered onto the
            // botwork-plugin network. Surface it loudly rather
            // than skipping it.
            None => ("(unregistered)".to_string(), "?".to_string()),
        };
        rows.push(render::TableRow {
            id: container.id,
            container: container.name,
            agent,
            image,
            age: container.age,
        });
    }
    rows
}

fn print_usage() {
    println!("Usage: botwork-tools ps");
    println!();
    println!("Lists running mcp_session_* containers with their bound");
    println!("agent identity, plugin, and age.");
    println!();
    println!("Environment:");
    println!("  {SESSIONS_URL_ENV}");
    println!("    Override the session-broker admin endpoint URL.");
    println!("    Default: {DEFAULT_SESSIONS_URL}");
}

#[derive(Debug, Error)]
pub enum PsError {
    #[error("usage: botwork-tools ps")]
    InvalidUsage,
    #[error(transparent)]
    Sessions(#[from] SessionsError),
    #[error(transparent)]
    Docker(#[from] DockerError),
}

impl PsError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::InvalidUsage => 2,
            Self::Sessions(_) => 1,
            Self::Docker(err) => err.exit_code(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{build_rows, run, PsError};
    use crate::ps::docker::{DockerError, RunningContainer};
    use crate::ps::render::TableRow;
    use crate::ps::sessions::SessionView;

    fn session(plugin: &str, agent_id: Option<&str>) -> SessionView {
        SessionView {
            tenant: "t".into(),
            workspace: "w".into(),
            plugin: plugin.into(),
            agent_id: agent_id.map(str::to_string),
        }
    }

    fn container(id: &str, name: &str) -> RunningContainer {
        RunningContainer {
            id: id.into(),
            name: name.into(),
            age: "5 minutes ago".into(),
        }
    }

    // --- build_rows: row-assembly logic ---

    #[test]
    fn build_rows_empty_containers_yields_empty_rows() {
        let rows = build_rows(vec![], &BTreeMap::new());
        assert!(rows.is_empty());
    }

    #[test]
    fn build_rows_registered_container_with_agent_id() {
        let containers = vec![container("abc123", "mcp_session_foo")];
        let mut sessions = BTreeMap::new();
        sessions.insert("mcp_session_foo".into(), session("fetch", Some("agent-42")));

        let rows = build_rows(containers, &sessions);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            TableRow {
                id: "abc123".into(),
                container: "mcp_session_foo".into(),
                agent: "agent-42".into(),
                image: "fetch".into(),
                age: "5 minutes ago".into(),
            }
        );
    }

    #[test]
    fn build_rows_registered_container_without_agent_id_shows_unbound() {
        let containers = vec![container("def456", "mcp_session_bar")];
        let mut sessions = BTreeMap::new();
        sessions.insert("mcp_session_bar".into(), session("echo", None));

        let rows = build_rows(containers, &sessions);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent, "(unbound)");
        assert_eq!(rows[0].image, "echo");
    }

    #[test]
    fn build_rows_unregistered_container_shows_unregistered_and_unknown_image() {
        let containers = vec![container("ghi789", "mcp_session_mystery")];
        let sessions = BTreeMap::new(); // broker has no entry

        let rows = build_rows(containers, &sessions);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent, "(unregistered)");
        assert_eq!(rows[0].image, "?");
    }

    #[test]
    fn build_rows_mixed_registered_and_unregistered() {
        let containers = vec![
            container("aaa", "mcp_session_known"),
            container("bbb", "mcp_session_unknown"),
        ];
        let mut sessions = BTreeMap::new();
        sessions.insert(
            "mcp_session_known".into(),
            session("plugin-a", Some("agent-1")),
        );

        let rows = build_rows(containers, &sessions);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].agent, "agent-1");
        assert_eq!(rows[1].agent, "(unregistered)");
    }

    // --- PsError exit-codes ---

    #[test]
    fn ps_error_invalid_usage_exit_code_is_2() {
        assert_eq!(PsError::InvalidUsage.exit_code(), 2);
    }

    #[test]
    fn ps_error_sessions_exit_code_is_1() {
        use crate::ps::sessions::SessionsError;
        let err = PsError::Sessions(SessionsError::Transport("x".into()));
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn ps_error_docker_not_found_exit_code_is_2() {
        let err = PsError::Docker(DockerError::NotFound);
        assert_eq!(err.exit_code(), 2);
    }

    // --- run: pure error paths (no docker, no network) ---

    #[test]
    fn run_with_extra_args_returns_invalid_usage() {
        let err = run(&["--unknown".to_string()]).unwrap_err();
        assert!(matches!(err, PsError::InvalidUsage));
    }

    #[test]
    fn run_with_help_flag_exits_cleanly() {
        // -h prints usage to stdout and returns Ok(()) — no docker needed.
        let result = run(&["-h".to_string()]);
        assert!(result.is_ok());
        let result = run(&["--help".to_string()]);
        assert!(result.is_ok());
    }
}
