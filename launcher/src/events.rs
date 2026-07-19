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
///
/// This function is intended to be spawned once at launcher startup as a
/// background `tokio::task`.  It **never returns** — on each restart it waits
/// 2 seconds before re-subscribing so transient docker daemon hiccups do not
/// cause a tight restart loop.  Any exit events that occurred while the
/// subscription was down are picked up by the broker-side liveness poll
/// (mechanism B).
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
/// Returns `Some((container_name, event_action, exit_code))` where:
/// - `container_name` — the `Actor.Attributes.name` field, e.g. `mcp_session_5ea57ab800c5`
/// - `event_action`  — one of `"die"`, `"destroy"`, or `"oom"`
/// - `exit_code`     — the process exit code from `Actor.Attributes.exitCode` if present
///
/// Returns `None` for any line that does not represent a `die`/`destroy`/`oom`
/// event for a container whose name matches `^mcp_session_[a-f0-9]+$`, including
/// malformed or incomplete JSON lines (which are silently dropped rather than
/// causing a panic).
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

    let stream = match timeout(
        BROKER_CONNECT_TIMEOUT,
        UnixStream::connect(broker_socket_path),
    )
    .await
    {
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
    map.insert(
        "name".to_string(),
        serde_json::Value::String(name.to_string()),
    );
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

    #[test]
    fn parse_event_line_accepts_lowercase_action_key() {
        // Docker may emit lowercase "action" instead of "Action" in some versions.
        // The parser tries "Action" first, then falls back to "action".
        let line = r#"{"Type":"container","action":"die","Actor":{"ID":"abc","Attributes":{"name":"mcp_session_5ea57ab800c5","exitCode":"137"}},"time":1234567890}"#;
        let result = parse_event_line(line);
        assert!(
            result.is_some(),
            "lowercase 'action' key should be accepted"
        );
        let (name, event, exit_code) = result.unwrap();
        assert_eq!(name, "mcp_session_5ea57ab800c5");
        assert_eq!(event, "die");
        assert_eq!(exit_code, Some(137));
    }

    #[test]
    fn parse_event_line_rejects_missing_required_fields() {
        for line in [
            r#"{"Type":"container","Actor":{"Attributes":{"name":"mcp_session_5ea57ab800c5"}}}"#,
            r#"{"Type":"container","Action":"die"}"#,
            r#"{"Type":"container","Action":"die","Actor":{}}"#,
            r#"{"Type":"container","Action":"die","Actor":{"Attributes":{}}}"#,
            r#"{"Type":"container","Action":123,"action":"die","Actor":{"Attributes":{"name":"mcp_session_5ea57ab800c5"}}}"#,
        ] {
            assert!(
                parse_event_line(line).is_none(),
                "expected None for malformed event: {line}"
            );
        }
    }

    #[test]
    fn parse_event_line_handles_non_string_exit_code_as_none() {
        let line = r#"{"Type":"container","Action":"die","Actor":{"ID":"abc","Attributes":{"name":"mcp_session_5ea57ab800c5","exitCode":137}}}"#;
        let result = parse_event_line(line).expect("event should parse");
        assert_eq!(result.2, None);
    }

    // ── build_exit_payload ──────────────────────────────────────────────────

    #[test]
    fn build_exit_payload_with_exit_code_produces_valid_json() {
        let bytes = build_exit_payload("mcp_session_aabbccddeeff", "die", Some(137))
            .expect("payload must serialize");
        let parsed: serde_json::Value =
            serde_json::from_slice(&bytes).expect("payload must be valid JSON");
        assert_eq!(parsed["name"], "mcp_session_aabbccddeeff");
        assert_eq!(parsed["event"], "die");
        assert_eq!(parsed["exit_code"], 137);
    }

    #[test]
    fn build_exit_payload_without_exit_code_sets_null() {
        let bytes = build_exit_payload("mcp_session_aabbccddeeff", "oom", None)
            .expect("payload must serialize");
        let parsed: serde_json::Value =
            serde_json::from_slice(&bytes).expect("payload must be valid JSON");
        assert_eq!(parsed["name"], "mcp_session_aabbccddeeff");
        assert_eq!(parsed["event"], "oom");
        assert!(parsed["exit_code"].is_null(), "exit_code should be null");
    }

    #[test]
    fn build_exit_payload_negative_exit_code_is_preserved() {
        let bytes = build_exit_payload("mcp_session_001122334455", "die", Some(-1))
            .expect("payload must serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
        assert_eq!(parsed["exit_code"], -1);
    }

    #[test]
    fn build_exit_payload_destroy_event() {
        let bytes = build_exit_payload("mcp_session_ffeeddccbbaa", "destroy", Some(0))
            .expect("payload must serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
        assert_eq!(parsed["event"], "destroy");
        assert_eq!(parsed["exit_code"], 0);
    }

    // ── container_name_pattern ──────────────────────────────────────────────

    #[test]
    fn container_name_pattern_accepts_valid_hex_names() {
        let re = container_name_pattern();
        assert!(re.is_match("mcp_session_aabbccddeeff"));
        assert!(re.is_match("mcp_session_000000000000"));
        assert!(re.is_match("mcp_session_abcdef123456"));
    }

    #[test]
    fn container_name_pattern_rejects_invalid_names() {
        let re = container_name_pattern();
        // Uppercase hex letters — the character class is `[a-f0-9]` (lowercase only)
        assert!(!re.is_match("mcp_session_AABBCCDDEEFF"));
        // Non-hex characters in the suffix
        assert!(!re.is_match("mcp_session_xyz_xxxxxxxxx"));
        // Wrong prefix entirely
        assert!(!re.is_match("session_aabbccddeeff"));
        assert!(!re.is_match("container_aabbccddeeff"));
        // Empty string
        assert!(!re.is_match(""));
        // Just the prefix with no hex part
        assert!(!re.is_match("mcp_session_"));
    }

    // ── forward_exit_event and post_to_stream ──────────────────────────────

    #[tokio::test]
    async fn forward_exit_event_sends_http_request_to_broker_socket() {
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let socket_path = tmp.path().join("broker.sock");
        let socket_path_str = socket_path.to_string_lossy().to_string();

        // Spawn a minimal Unix socket server that reads the HTTP request
        // and writes a 200 response.
        let listener = UnixListener::bind(&socket_path).expect("bind");
        let server_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut reader = BufReader::new(&mut stream);
            let mut request_line = String::new();
            reader
                .read_line(&mut request_line)
                .await
                .expect("read request line");
            // Drain remaining headers + body
            let mut buf = vec![0u8; 4096];
            let _ = tokio::time::timeout(Duration::from_millis(100), reader.read(&mut buf)).await;
            // Send a minimal 200 response
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("write response");
            request_line
        });

        // Wait briefly for the server to be ready
        tokio::time::sleep(Duration::from_millis(10)).await;

        forward_exit_event(
            &socket_path_str,
            "mcp_session_aabbccddeeff",
            "die",
            Some(137),
        )
        .await;

        let request_line = tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("server task timeout")
            .expect("server task join");

        assert!(
            request_line.starts_with("POST /container-exit"),
            "expected POST /container-exit, got: {request_line:?}"
        );
    }

    #[tokio::test]
    async fn forward_exit_event_handles_connect_failure_gracefully() {
        // No server listening — connect should fail and not panic.
        forward_exit_event(
            "/nonexistent/broker.sock",
            "mcp_session_aabbccddeeff",
            "die",
            None,
        )
        .await;
        // Test passes if no panic.
    }

    #[tokio::test]
    async fn post_to_stream_sends_body_and_receives_status() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{UnixListener, UnixStream};

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let socket_path = tmp.path().join("post_test.sock");

        let listener = UnixListener::bind(&socket_path).expect("bind");
        let server_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.expect("read");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("write");
            buf[..n].to_vec()
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let stream = UnixStream::connect(&socket_path).await.expect("connect");
        let body = b"{\"name\":\"test\"}";
        let result = post_to_stream(stream, body).await;
        assert!(result.is_ok(), "post_to_stream must succeed: {result:?}");

        let received = tokio::time::timeout(std::time::Duration::from_secs(2), server_task)
            .await
            .expect("timeout")
            .expect("join");
        let text = String::from_utf8_lossy(&received);
        assert!(text.contains("POST /container-exit"), "request: {text}");
        assert!(
            text.contains(&format!("Content-Length: {}", body.len())),
            "content-length: {text}"
        );
    }

    #[tokio::test]
    async fn build_exit_payload_and_forward_combined_for_destroy_event() {
        // Ensure the JSON body produced for a "destroy" event with no exit
        // code has the right shape before being forwarded.
        let bytes =
            build_exit_payload("mcp_session_ffeeddccbbaa", "destroy", None).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(parsed["event"], "destroy");
        assert!(parsed["exit_code"].is_null());
        assert_eq!(parsed["name"], "mcp_session_ffeeddccbbaa");
    }
}
