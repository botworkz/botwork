use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use botwork_launcher::server::AppState;
use botwork_launcher::validate::Validators;
use botwork_launcher::{serve_on, Config};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::{sleep, timeout};

struct HttpResponse {
    status_line: String,
    headers: HashMap<String, String>,
    body: String,
}

struct Case {
    method: &'static str,
    path: &'static str,
    body: &'static str,
    expected_status: &'static str,
    expected_body: &'static str,
}

#[tokio::test]
async fn wire_contract_validation_paths() {
    let temp = tempdir().expect("create temp dir");
    let socket_path = temp.path().join("launcher.sock");

    let listener = UnixListener::bind(&socket_path).expect("bind unix socket");
    let config = Config {
        socket_path: socket_path.to_string_lossy().into_owned(),
        socket_group: None,
        allowed_peer_uid: Some(nix::unistd::geteuid().as_raw()),
        allowed_peer_gid: Some(nix::unistd::getegid().as_raw()),
        plugin_uid: 1000,
        plugin_gid: 1000,
        image_allowlist_regex: r"^botwork/[a-z0-9_-]+:[a-z0-9._-]+$".to_string(),
        container_pids_limit: 256,
        container_cpu_limit: "1.0".to_string(),
        container_memory_limit: "512m".to_string(),
        container_read_only_rootfs: false,
        broker_socket_path: "/run/botwork/broker.sock".to_string(),
        default_network: "botwork-plugin".to_string(),
        egress_proxy: None,
    };
    let validators = Validators::new(&config.image_allowlist_regex).expect("validators");
    let state = Arc::new(AppState { config, validators });

    let server_task = tokio::spawn(async move { serve_on(listener, state).await });
    wait_for_server(&socket_path).await;

    let cases = [
        Case {
            method: "POST",
            path: "/launch",
            body: r#"{"name":"mcp_session_aabbccddeeff","image":"botwork/mcp-echo:local","network":"botwork"}"#,
            expected_status: "HTTP/1.1 400 Bad Request",
            expected_body: r#"{"error": "invalid staging_path"}"#,
        },
        Case {
            method: "POST",
            path: "/launch",
            body: r#"{"name":"mcp_session_aabbccddeeff","image":"botwork/mcp-echo:local","network":"botwork","staging_path":"/var/lib/botwork/tenants/acme/staging/aabbccddeeff","with_workspace":"false"}"#,
            expected_status: "HTTP/1.1 400 Bad Request",
            expected_body: r#"{"error": "invalid with_workspace"}"#,
        },
        Case {
            method: "POST",
            path: "/bind-agent",
            body: r#"{"staging_path":"/var/lib/botwork/tenants/acme/staging/aabbccddeeff","agent_dir":"/var/lib/botwork/tenants/acme/workspaces/mcp/agents/bad.agent"}"#,
            expected_status: "HTTP/1.1 400 Bad Request",
            expected_body: r#"{"error": "invalid agent_dir"}"#,
        },
        Case {
            // Old-format agent_dir without workspaces component → 400
            method: "POST",
            path: "/bind-agent",
            body: r#"{"staging_path":"/var/lib/botwork/tenants/acme/staging/aabbccddeeff","agent_dir":"/var/lib/botwork/tenants/acme/agents/my-agent"}"#,
            expected_status: "HTTP/1.1 400 Bad Request",
            expected_body: r#"{"error": "invalid agent_dir"}"#,
        },
        Case {
            method: "POST",
            path: "/teardown",
            body: r#"{"name":"bad","staging_path":"/var/lib/botwork/tenants/acme/staging/aabbccddeeff"}"#,
            expected_status: "HTTP/1.1 400 Bad Request",
            expected_body: r#"{"error": "invalid container name"}"#,
        },
        Case {
            method: "POST",
            path: "/launch",
            body: r#"{"name":"mcp_session_aabbccddeeff""#,
            expected_status: "HTTP/1.1 400 Bad Request",
            expected_body: r#"{"error": "request body must be valid JSON"}"#,
        },
        Case {
            method: "POST",
            path: "/launch",
            body: "[]",
            expected_status: "HTTP/1.1 400 Bad Request",
            expected_body: r#"{"error": "request body must be a JSON object"}"#,
        },
        Case {
            method: "GET",
            path: "/launch",
            body: "",
            expected_status: "HTTP/1.1 404 Not Found",
            expected_body: r#"{"error": "not found"}"#,
        },
        Case {
            method: "POST",
            path: "/unknown-path",
            body: "{}",
            expected_status: "HTTP/1.1 404 Not Found",
            expected_body: r#"{"error": "not found"}"#,
        },
    ];

    for case in cases {
        let response = send_request(&socket_path, case.method, case.path, case.body).await;
        assert_eq!(response.status_line, case.expected_status);
        assert_eq!(response.body, case.expected_body);
        assert_eq!(
            response.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        let expected_len = response.body.len().to_string();
        assert_eq!(
            response.headers.get("content-length").map(String::as_str),
            Some(expected_len.as_str())
        );
    }

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn wire_contract_rejects_large_request_bodies() {
    let temp = tempdir().expect("create temp dir");
    let socket_path = temp.path().join("launcher.sock");

    let listener = UnixListener::bind(&socket_path).expect("bind unix socket");
    let config = Config {
        socket_path: socket_path.to_string_lossy().into_owned(),
        socket_group: None,
        allowed_peer_uid: Some(nix::unistd::geteuid().as_raw()),
        allowed_peer_gid: Some(nix::unistd::getegid().as_raw()),
        plugin_uid: 1000,
        plugin_gid: 1000,
        image_allowlist_regex: r"^(botspace|botwork)/[a-z0-9_-]+:[a-z0-9._-]+$".to_string(),
        container_pids_limit: 256,
        container_cpu_limit: "1.0".to_string(),
        container_memory_limit: "512m".to_string(),
        container_read_only_rootfs: false,
        broker_socket_path: "/run/botwork/broker.sock".to_string(),
        default_network: "botwork-plugin".to_string(),
        egress_proxy: None,
    };
    let validators = Validators::new(&config.image_allowlist_regex).expect("validators");
    let state = Arc::new(AppState { config, validators });

    let server_task = tokio::spawn(async move { serve_on(listener, state).await });
    wait_for_server(&socket_path).await;

    let oversized = format!("{{\"padding\":\"{}\"}}", "a".repeat(65_537));
    let response = send_request(&socket_path, "POST", "/launch", &oversized).await;
    assert_eq!(response.status_line, "HTTP/1.1 413 Payload Too Large");
    assert_eq!(
        response.body,
        r#"{"error": "request body exceeds 65536 bytes"}"#
    );

    server_task.abort();
    let _ = server_task.await;
}

async fn wait_for_server(socket_path: &Path) {
    for _ in 0..50 {
        match UnixStream::connect(socket_path).await {
            Ok(stream) => {
                drop(stream);
                return;
            }
            Err(err)
                if err.kind() == ErrorKind::NotFound
                    || err.kind() == ErrorKind::ConnectionRefused =>
            {
                sleep(Duration::from_millis(10)).await;
            }
            Err(err) => panic!("failed waiting for server readiness: {err}"),
        }
    }

    panic!("timed out waiting for server readiness");
}

async fn send_request(socket_path: &Path, method: &str, path: &str, body: &str) -> HttpResponse {
    let mut stream = timeout(Duration::from_secs(2), UnixStream::connect(socket_path))
        .await
        .expect("timed out connecting to socket")
        .expect("connect to launcher socket");

    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");

    let mut raw = Vec::new();
    let mut buffer = [0_u8; 1024];
    let mut expected_total = None;
    loop {
        let read = timeout(Duration::from_secs(2), stream.read(&mut buffer))
            .await
            .expect("timed out reading response")
            .expect("read response");
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..read]);

        if expected_total.is_none() {
            if let Some((header_end, headers)) = try_parse_headers(&raw) {
                if let Some(content_length) = headers.get("content-length") {
                    let body_len = content_length
                        .parse::<usize>()
                        .expect("valid content-length");
                    expected_total = Some(header_end + 4 + body_len);
                }
            }
        }

        if let Some(total_len) = expected_total {
            if raw.len() >= total_len {
                break;
            }
        }
    }

    parse_http_response(&raw)
}

fn try_parse_headers(raw: &[u8]) -> Option<(usize, HashMap<String, String>)> {
    let header_end = raw.windows(4).position(|chunk| chunk == b"\r\n\r\n")?;
    let header_block = String::from_utf8(raw[..header_end].to_vec()).ok()?;
    let mut headers = HashMap::new();
    for line in header_block.split("\r\n").skip(1) {
        if let Some((name, value)) = line.split_once(": ") {
            headers.insert(name.to_ascii_lowercase(), value.to_string());
        }
    }
    Some((header_end, headers))
}

fn parse_http_response(raw: &[u8]) -> HttpResponse {
    let header_end = raw
        .windows(4)
        .position(|chunk| chunk == b"\r\n\r\n")
        .expect("response must contain header delimiter");
    let header_block = String::from_utf8(raw[..header_end].to_vec()).expect("utf8 headers");
    let body = String::from_utf8(raw[(header_end + 4)..].to_vec()).expect("utf8 body");

    let mut lines = header_block.split("\r\n");
    let status_line = lines.next().expect("status line").to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(": ") {
            headers.insert(name.to_ascii_lowercase(), value.to_string());
        }
    }

    HttpResponse {
        status_line,
        headers,
        body,
    }
}
