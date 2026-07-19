//! Docker event subscription for `mcp_session_*` containers.
//!
//! Spawned once at launcher startup. Subscribes to docker socket events,
//! filters to container names matching `^mcp_session_[a-f0-9]+$` and
//! `die`/`destroy`/`oom` actions, then POSTs each exit event as JSON to the
//! broker callback unix socket.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use bollard::models::{EventActor, EventMessage};
use bollard::query_parameters::EventsOptionsBuilder;
use futures_util::{Stream, StreamExt};
use regex::Regex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::timeout;

use crate::cmd::log_info;
use crate::docker::{connect_docker, DockerApi};

const CONTAINER_NAME_RE: &str = r"^mcp_session_[a-f0-9]+$";
const BROKER_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const BROKER_POST_TIMEOUT: Duration = Duration::from_secs(5);

fn container_name_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(CONTAINER_NAME_RE).expect("valid container name regex"))
}

/// Runs forever, restarting the docker events subscription on failure.
pub async fn run_event_loop(broker_socket_path: String) -> ! {
    loop {
        log_info("events: starting docker events subscription");
        run_event_loop_once(&broker_socket_path).await;
        log_info("events: docker events subscription ended; restarting in 2s");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn run_event_loop_once(broker_socket_path: &str) {
    let docker = match connect_docker() {
        Ok(docker) => docker,
        Err(e) => {
            log_info(&format!("events: failed to connect to docker socket: {e}"));
            return;
        }
    };

    run_event_loop_once_with_docker(&docker, broker_socket_path).await;
}

async fn run_event_loop_once_with_docker<D: DockerApi + ?Sized>(
    docker: &D,
    broker_socket_path: &str,
) {
    let mut filters = HashMap::new();
    filters.insert(
        "event".to_string(),
        vec!["die".to_string(), "destroy".to_string(), "oom".to_string()],
    );

    let options = EventsOptionsBuilder::new().filters(&filters).build();
    let stream = docker.events(Some(options));

    drive_event_stream(stream, broker_socket_path).await;
}

pub(crate) fn process_event(message: &EventMessage) -> Option<(String, String, Option<i64>)> {
    let action = message.action.as_deref()?;
    if !matches!(action, "die" | "destroy" | "oom") {
        return None;
    }

    let EventActor { attributes, .. } = message.actor.as_ref()?;
    let attributes = attributes.as_ref()?;

    let name = attributes.get("name")?.to_string();
    if !container_name_pattern().is_match(&name) {
        return None;
    }

    let exit_code = attributes
        .get("exitCode")
        .and_then(|value| value.parse::<i64>().ok());

    Some((name, action.to_string(), exit_code))
}

pub(crate) async fn drive_event_stream<S, E>(mut stream: S, broker_socket_path: &str)
where
    S: Stream<Item = Result<EventMessage, E>> + Unpin,
    E: std::fmt::Display,
{
    while let Some(next) = stream.next().await {
        match next {
            Ok(message) => {
                if let Some((name, event, exit_code)) = process_event(&message) {
                    forward_exit_event(broker_socket_path, &name, &event, exit_code).await;
                }
            }
            Err(e) => {
                log_info(&format!("events: failed to read docker events stream: {e}"));
                return;
            }
        }
    }
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
    use futures_util::stream;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    #[test]
    fn process_event_accepts_die_event_for_mcp_session() {
        let mut attributes = HashMap::new();
        attributes.insert("name".to_string(), "mcp_session_5ea57ab800c5".to_string());
        attributes.insert("exitCode".to_string(), "137".to_string());
        let message = EventMessage {
            action: Some("die".to_string()),
            actor: Some(EventActor {
                attributes: Some(attributes),
                ..Default::default()
            }),
            ..Default::default()
        };

        let result = process_event(&message).expect("event should be accepted");
        assert_eq!(result.0, "mcp_session_5ea57ab800c5");
        assert_eq!(result.1, "die");
        assert_eq!(result.2, Some(137));
    }

    #[test]
    fn process_event_rejects_non_matching_name() {
        let mut attributes = HashMap::new();
        attributes.insert("name".to_string(), "other_container".to_string());
        let message = EventMessage {
            action: Some("die".to_string()),
            actor: Some(EventActor {
                attributes: Some(attributes),
                ..Default::default()
            }),
            ..Default::default()
        };

        assert!(process_event(&message).is_none());
    }

    #[test]
    fn process_event_rejects_unknown_action() {
        let mut attributes = HashMap::new();
        attributes.insert("name".to_string(), "mcp_session_5ea57ab800c5".to_string());
        let message = EventMessage {
            action: Some("start".to_string()),
            actor: Some(EventActor {
                attributes: Some(attributes),
                ..Default::default()
            }),
            ..Default::default()
        };

        assert!(process_event(&message).is_none());
    }

    #[tokio::test]
    async fn drive_event_stream_forwards_payload_to_broker_socket() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let socket_path = tmp.path().join("broker.sock");
        let socket_path_str = socket_path.to_string_lossy().to_string();
        let captured = Arc::new(Mutex::new(String::new()));

        let listener = UnixListener::bind(&socket_path).expect("bind");
        let captured_server = Arc::clone(&captured);
        let server_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut reader = BufReader::new(&mut stream);
            let mut request_line = String::new();
            reader
                .read_line(&mut request_line)
                .await
                .expect("request line");

            let mut buf = vec![0u8; 4096];
            let n = tokio::time::timeout(Duration::from_millis(100), reader.read(&mut buf))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(0);
            *captured_server.lock().expect("capture lock") =
                format!("{request_line}{}", String::from_utf8_lossy(&buf[..n]));

            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("write response");
        });

        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), "mcp_session_aabbccddeeff".to_string());
        attrs.insert("exitCode".to_string(), "0".to_string());

        let event = EventMessage {
            action: Some("destroy".to_string()),
            actor: Some(EventActor {
                attributes: Some(attrs),
                ..Default::default()
            }),
            ..Default::default()
        };

        let stream = stream::iter(vec![Ok::<EventMessage, String>(event)]);
        drive_event_stream(stream, &socket_path_str).await;

        tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("server timeout")
            .expect("server join");

        let captured = captured.lock().expect("capture lock").clone();
        assert!(
            captured.contains("POST /container-exit HTTP/1.1"),
            "{captured}"
        );
        assert!(
            captured.contains("\"name\":\"mcp_session_aabbccddeeff\""),
            "{captured}"
        );
        assert!(captured.contains("\"event\":\"destroy\""), "{captured}");
        assert!(captured.contains("\"exit_code\":0"), "{captured}");
    }

    #[tokio::test]
    async fn forward_exit_event_handles_connect_failure_gracefully() {
        forward_exit_event(
            "/nonexistent/broker.sock",
            "mcp_session_aabbccddeeff",
            "die",
            None,
        )
        .await;
    }

    #[test]
    fn build_exit_payload_without_exit_code_sets_null() {
        let bytes = build_exit_payload("mcp_session_aabbccddeeff", "oom", None).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("valid json");
        assert_eq!(parsed["name"], "mcp_session_aabbccddeeff");
        assert_eq!(parsed["event"], "oom");
        assert!(parsed["exit_code"].is_null());
    }

    #[test]
    fn build_exit_payload_with_exit_code_sets_numeric_value() {
        let bytes =
            build_exit_payload("mcp_session_aabbccddeeff", "die", Some(137)).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("valid json");
        assert_eq!(parsed["name"], "mcp_session_aabbccddeeff");
        assert_eq!(parsed["event"], "die");
        assert_eq!(parsed["exit_code"], 137);
    }

    #[test]
    fn build_exit_payload_preserves_negative_exit_code() {
        let bytes =
            build_exit_payload("mcp_session_aabbccddeeff", "die", Some(-1)).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("valid json");
        assert_eq!(parsed["exit_code"], -1);
    }
}
