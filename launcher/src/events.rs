//! Docker event subscription for `mcp_session_*` containers.
//!
//! Spawned once at launcher startup. Subscribes to `docker events`, filters to
//! container names matching `^mcp_session_[a-f0-9]+$` and `die`/`destroy`/`oom`
//! events, then POSTs each exit event as JSON to the broker callback unix socket.

use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::time::timeout;

use crate::cmd::log_info;

const CONTAINER_NAME_RE: &str = r"^mcp_session_[a-f0-9]+$";
const BROKER_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const BROKER_POST_TIMEOUT: Duration = Duration::from_secs(5);

fn container_name_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(CONTAINER_NAME_RE).expect("valid container name regex"))
}

/// Runs forever, restarting the `docker events` subscription on failure.
pub async fn run_event_loop(broker_socket_path: String) -> ! {
    loop {
        log_info("events: starting docker events subscription");
        run_event_loop_once(&broker_socket_path).await;
        log_info("events: docker events subscription ended; restarting in 2s");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn run_event_loop_once(broker_socket_path: &str) {
    let mut child = match Command::new("docker")
        .args([
            "events",
            "--filter",
            "event=die",
            "--filter",
            "event=destroy",
            "--filter",
            "event=oom",
            "--format",
            "{{json .}}",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            log_info(&format!("events: failed to spawn docker events: {e}"));
            return;
        }
    };

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            log_info("events: docker events has no stdout");
            return;
        }
    };

    let mut reader = BufReader::new(stdout).lines();

    while let Ok(Some(line)) = reader.next_line().await {
        if let Some((name, event, exit_code)) = parse_event_line(&line) {
            forward_exit_event(broker_socket_path, &name, &event, exit_code).await;
        }
    }

    let _ = child.wait().await;
}

/// Parses a JSON line from `docker events --format '{{json .}}'` output.
///
/// Returns `Some((container_name, event_action, exit_code))` when the line
/// represents a `die`/`destroy`/`oom` event for a container whose name matches
/// `^mcp_session_[a-f0-9]+$`, or `None` otherwise (including malformed lines).
pub fn parse_event_line(line: &str) -> Option<(String, String, Option<i64>)> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;

    let event = value
        .get("Action")
        .or_else(|| value.get("action"))
        .and_then(|v| v.as_str())?
        .to_string();

    if !matches!(event.as_str(), "die" | "destroy" | "oom") {
        return None;
    }

    let name = value
        .get("Actor")
        .and_then(|a| a.get("Attributes"))
        .and_then(|attrs| attrs.get("name"))
        .and_then(|v| v.as_str())?
        .to_string();

    if !container_name_pattern().is_match(&name) {
        return None;
    }

    let exit_code = value
        .get("Actor")
        .and_then(|a| a.get("Attributes"))
        .and_then(|attrs| attrs.get("exitCode"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok());

    Some((name, event, exit_code))
}

async fn forward_exit_event(
    broker_socket_path: &str,
    name: &str,
    event: &str,
    exit_code: Option<i64>,
) {
    let body = match build_exit_payload(name, event, exit_code) {
        Ok(b) => b,
        Err(e) => {
            log_info(&format!("events: failed to serialize exit payload: {e}"));
            return;
        }
    };

    let stream =
        match timeout(BROKER_CONNECT_TIMEOUT, UnixStream::connect(broker_socket_path)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                log_info(&format!(
                    "events: failed to connect to broker socket {broker_socket_path}: {e}"
                ));
                return;
            }
            Err(_) => {
                log_info(&format!(
                    "events: timeout connecting to broker socket {broker_socket_path}"
                ));
                return;
            }
        };

    if let Err(e) = post_to_stream(stream, &body).await {
        log_info(&format!(
            "events: failed to post exit event for container={name}: {e}"
        ));
    } else {
        log_info(&format!(
            "events: forwarded exit event container={name} event={event}"
        ));
    }
}

fn build_exit_payload(name: &str, event: &str, exit_code: Option<i64>) -> Result<Vec<u8>, String> {
    let mut map = serde_json::Map::new();
    map.insert("name".to_string(), serde_json::Value::String(name.to_string()));
    map.insert(
        "event".to_string(),
        serde_json::Value::String(event.to_string()),
    );
    match exit_code {
        Some(code) => {
            map.insert(
                "exit_code".to_string(),
                serde_json::Value::Number(code.into()),
            );
        }
        None => {
            map.insert("exit_code".to_string(), serde_json::Value::Null);
        }
    }
    serde_json::to_vec(&serde_json::Value::Object(map)).map_err(|e| e.to_string())
}

async fn post_to_stream(mut stream: UnixStream, body: &[u8]) -> Result<(), String> {
    let request = format!(
        "POST /container-exit HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );

    let send_future = async {
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        stream.write_all(body).await.map_err(|e| e.to_string())?;
        // Read enough to get the status line and discard
        let mut buf = [0u8; 64];
        let _ = stream.read(&mut buf).await;
        Ok(())
    };

    timeout(BROKER_POST_TIMEOUT, send_future)
        .await
        .map_err(|_| "timeout posting exit event to broker".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_event_line_accepts_die_event_for_mcp_session() {
        let line = r#"{"Type":"container","Action":"die","Actor":{"ID":"abc","Attributes":{"name":"mcp_session_5ea57ab800c5","exitCode":"137"}},"time":1234567890}"#;
        let result = parse_event_line(line);
        assert!(result.is_some(), "expected Some for die event");
        let (name, event, exit_code) = result.unwrap();
        assert_eq!(name, "mcp_session_5ea57ab800c5");
        assert_eq!(event, "die");
        assert_eq!(exit_code, Some(137));
    }

    #[test]
    fn parse_event_line_accepts_oom_event() {
        let line = r#"{"Type":"container","Action":"oom","Actor":{"ID":"abc","Attributes":{"name":"mcp_session_aabbccddeeff"}},"time":1234567890}"#;
        let result = parse_event_line(line);
        assert!(result.is_some(), "expected Some for oom event");
        let (name, event, exit_code) = result.unwrap();
        assert_eq!(name, "mcp_session_aabbccddeeff");
        assert_eq!(event, "oom");
        assert_eq!(exit_code, None);
    }

    #[test]
    fn parse_event_line_accepts_destroy_event() {
        let line = r#"{"Type":"container","Action":"destroy","Actor":{"ID":"abc","Attributes":{"name":"mcp_session_001122334455"}},"time":1234567890}"#;
        let result = parse_event_line(line);
        assert!(result.is_some(), "expected Some for destroy event");
        let (_, event, _) = result.unwrap();
        assert_eq!(event, "destroy");
    }

    #[test]
    fn parse_event_line_rejects_non_mcp_session_names() {
        for name in &[
            "other_container",
            "mcp_session_",
            "mcp_session_UPPER",
            "mcp_session_xyz",
            "",
        ] {
            let line = format!(
                r#"{{"Type":"container","Action":"die","Actor":{{"ID":"abc","Attributes":{{"name":"{name}","exitCode":"1"}}}},"time":1234}}"#
            );
            assert!(
                parse_event_line(&line).is_none(),
                "expected None for container name '{name}'"
            );
        }
    }

    #[test]
    fn parse_event_line_rejects_unknown_event_types() {
        for action in &["start", "stop", "pause", "kill", "create"] {
            let line = format!(
                r#"{{"Type":"container","Action":"{action}","Actor":{{"ID":"abc","Attributes":{{"name":"mcp_session_5ea57ab800c5"}}}},"time":1234}}"#
            );
            assert!(
                parse_event_line(&line).is_none(),
                "expected None for action '{action}'"
            );
        }
    }

    #[test]
    fn parse_event_line_drops_malformed_json() {
        assert!(parse_event_line("not json").is_none());
        assert!(parse_event_line("").is_none());
        assert!(parse_event_line("{").is_none());
        assert!(parse_event_line("null").is_none());
    }

    #[test]
    fn parse_event_line_handles_missing_exit_code() {
        let line = r#"{"Type":"container","Action":"die","Actor":{"ID":"abc","Attributes":{"name":"mcp_session_5ea57ab800c5"}},"time":1234567890}"#;
        let result = parse_event_line(line);
        assert!(result.is_some());
        let (_, _, exit_code) = result.unwrap();
        assert_eq!(exit_code, None);
    }

    #[test]
    fn parse_event_line_handles_non_numeric_exit_code_as_none() {
        let line = r#"{"Type":"container","Action":"die","Actor":{"ID":"abc","Attributes":{"name":"mcp_session_5ea57ab800c5","exitCode":"N/A"}},"time":1234567890}"#;
        let result = parse_event_line(line);
        assert!(result.is_some());
        let (_, _, exit_code) = result.unwrap();
        assert_eq!(exit_code, None);
    }
}
