//! End-to-end wire-contract tests for control-plane's session intake/read
//! surface.
//!
//! Spins up the full axum server bound to `127.0.0.1:0` against a fresh
//! in-memory store, so the tests exercise the same code path the real
//! binary does (request → JSON parse → store mutation → response render).
//!
//! Each test gets its own server / store: state isolation is more
//! valuable than the small startup cost (<1 ms in practice).

use botwork_control_plane::{build_app_state, build_router};
use reqwest::StatusCode;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

struct Server {
    base: String,
    _handle: JoinHandle<()>,
}

async fn spawn_server() -> Server {
    let state = build_app_state();
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Server {
        base: format!("http://{addr}"),
        _handle: handle,
    }
}

fn record_body(session_id: &str, ip: &str, plugin: &str) -> serde_json::Value {
    serde_json::json!({
        "session_id": session_id,
        "container_ip": ip,
        "tenant": "phlax",
        "namespace": "mcp",
        "plugin": plugin,
        "egress_policy": {"allow_hosts": ["github.com"]},
    })
}

#[tokio::test]
async fn post_get_delete_round_trip() {
    let server = spawn_server().await;
    let base = &server.base;
    let client = reqwest::Client::new();

    // POST creates.
    let post = client
        .post(format!("{base}/sessions"))
        .json(&record_body("mcp_session_abc", "172.20.0.5", "fetch"))
        .send()
        .await
        .expect("send post");
    assert_eq!(post.status(), StatusCode::CREATED);
    let post_body: serde_json::Value = post.json().await.expect("json");
    assert_eq!(post_body["status"], "stored");
    assert_eq!(post_body["session_id"], "mcp_session_abc");

    // GET single round-trips the record.
    let get = client
        .get(format!("{base}/sessions/mcp_session_abc"))
        .send()
        .await
        .expect("send get");
    assert_eq!(get.status(), StatusCode::OK);
    let got: serde_json::Value = get.json().await.expect("json");
    assert_eq!(got["session_id"], "mcp_session_abc");
    assert_eq!(got["container_ip"], "172.20.0.5");
    assert_eq!(got["plugin"], "fetch");
    assert_eq!(got["egress_policy"]["allow_hosts"][0], "github.com");

    // DELETE removes.
    let del = client
        .delete(format!("{base}/sessions/mcp_session_abc"))
        .send()
        .await
        .expect("send delete");
    assert_eq!(del.status(), StatusCode::OK);
    let del_body: serde_json::Value = del.json().await.expect("json");
    assert_eq!(del_body["status"], "removed");

    // Subsequent GET 404s.
    let get_again = client
        .get(format!("{base}/sessions/mcp_session_abc"))
        .send()
        .await
        .expect("send get again");
    assert_eq!(get_again.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn duplicate_post_returns_409_with_already_exists_code() {
    let server = spawn_server().await;
    let base = &server.base;
    let client = reqwest::Client::new();

    let body = record_body("mcp_session_abc", "172.20.0.5", "fetch");
    let first = client
        .post(format!("{base}/sessions"))
        .json(&body)
        .send()
        .await
        .expect("send first");
    assert_eq!(first.status(), StatusCode::CREATED);

    let second = client
        .post(format!("{base}/sessions"))
        .json(&body)
        .send()
        .await
        .expect("send second");
    assert_eq!(second.status(), StatusCode::CONFLICT);
    let body: serde_json::Value = second.json().await.expect("json");
    assert_eq!(body["error"], "already_exists");
}

#[tokio::test]
async fn delete_unknown_session_returns_404_with_not_found_code() {
    let server = spawn_server().await;
    let base = &server.base;
    let client = reqwest::Client::new();

    let response = client
        .delete(format!("{base}/sessions/mcp_session_neverwasthere"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"], "not_found");
}

#[tokio::test]
async fn list_returns_sessions_sorted_by_id() {
    let server = spawn_server().await;
    let base = &server.base;
    let client = reqwest::Client::new();

    for (id, ip, plugin) in [
        ("mcp_session_b", "172.20.0.6", "fetch"),
        ("mcp_session_a", "172.20.0.5", "git"),
        ("mcp_session_c", "172.20.0.7", "exec-jq"),
    ] {
        let response = client
            .post(format!("{base}/sessions"))
            .json(&record_body(id, ip, plugin))
            .send()
            .await
            .expect("send post");
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    let listed = client
        .get(format!("{base}/sessions"))
        .send()
        .await
        .expect("send list");
    assert_eq!(listed.status(), StatusCode::OK);
    let body: serde_json::Value = listed.json().await.expect("json");
    let ids: Vec<&str> = body["sessions"]
        .as_array()
        .expect("array")
        .iter()
        .map(|s| s["session_id"].as_str().expect("session_id str"))
        .collect();
    assert_eq!(ids, vec!["mcp_session_a", "mcp_session_b", "mcp_session_c"]);
}

#[tokio::test]
async fn post_with_missing_field_returns_400_invalid_request() {
    let server = spawn_server().await;
    let base = &server.base;
    let client = reqwest::Client::new();

    // No `plugin` field.
    let body = serde_json::json!({
        "session_id": "mcp_session_abc",
        "container_ip": "172.20.0.5",
        "tenant": "phlax",
        "namespace": "mcp",
    });
    let response = client
        .post(format!("{base}/sessions"))
        .json(&body)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"], "invalid_request");
    assert!(
        body["message"].as_str().unwrap_or("").contains("'plugin'"),
        "should name missing field: {body}"
    );
}

#[tokio::test]
async fn post_with_bad_ip_returns_400_invalid_request() {
    let server = spawn_server().await;
    let base = &server.base;
    let client = reqwest::Client::new();

    let mut body = record_body("mcp_session_abc", "not-an-ip", "fetch");
    body["container_ip"] = serde_json::Value::String("not-an-ip".to_string());

    let response = client
        .post(format!("{base}/sessions"))
        .json(&body)
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"], "invalid_request");
}

#[tokio::test]
async fn post_with_bad_session_id_returns_400_invalid_request() {
    let server = spawn_server().await;
    let base = &server.base;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{base}/sessions"))
        .json(&record_body("not-a-session-id", "172.20.0.5", "fetch"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"], "invalid_request");
}

#[tokio::test]
async fn post_with_garbage_body_returns_400_invalid_request() {
    let server = spawn_server().await;
    let base = &server.base;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{base}/sessions"))
        .header("content-type", "application/json")
        .body("not-json".to_string())
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"], "invalid_request");
}

#[tokio::test]
async fn unknown_path_returns_404() {
    let server = spawn_server().await;
    let base = &server.base;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{base}/unknown"))
        .json(&record_body("mcp_session_abc", "172.20.0.5", "fetch"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn wrong_method_on_sessions_returns_405() {
    let server = spawn_server().await;
    let base = &server.base;
    let client = reqwest::Client::new();

    // PUT is not allowed on /sessions in v0.
    let response = client
        .put(format!("{base}/sessions"))
        .json(&record_body("mcp_session_abc", "172.20.0.5", "fetch"))
        .send()
        .await
        .expect("send");
    assert!(
        matches!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_FOUND
        ),
        "PUT /sessions should not succeed, got {}",
        response.status()
    );
}

#[tokio::test]
async fn post_omitted_egress_policy_is_stored_as_null() {
    let server = spawn_server().await;
    let base = &server.base;
    let client = reqwest::Client::new();

    // Operator hasn't (yet) populated `egress:` on this plugin in
    // config-broker: session-broker just omits the field. Control-plane
    // stores null and the future xDS materialiser treats it as
    // default-open (decision deferred to the materialiser, not here).
    let body = serde_json::json!({
        "session_id": "mcp_session_abc",
        "container_ip": "172.20.0.5",
        "tenant": "phlax",
        "namespace": "mcp",
        "plugin": "fetch",
    });
    let post = client
        .post(format!("{base}/sessions"))
        .json(&body)
        .send()
        .await
        .expect("send post");
    assert_eq!(post.status(), StatusCode::CREATED);

    let get = client
        .get(format!("{base}/sessions/mcp_session_abc"))
        .send()
        .await
        .expect("send get");
    assert_eq!(get.status(), StatusCode::OK);
    let got: serde_json::Value = get.json().await.expect("json");
    assert!(got["egress_policy"].is_null(), "egress_policy: {got}");
}
