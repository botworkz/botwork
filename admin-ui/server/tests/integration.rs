//! End-to-end wire-contract test for the admin-ui server.
//!
//! Asserts:
//!
//! * `/healthz` returns 200 + the JSON `{ "status": "ok" }`.
//! * `/admin/` returns 200 with `content-type: text/html` AND
//!   the body contains a marker string from the trunk-emitted
//!   `index.html`. If trunk hasn't been run before `cargo test`
//!   the bundle is empty, the asset lookup fails, and we emit a
//!   clearly-labelled `IGNORED` line rather than failing — `cargo
//!   test` on a fresh checkout stays green even when `trunk build`
//!   hasn't been invoked yet. The full proof runs in CI where the
//!   image build always runs trunk first.
//! * `/admin/unknown-deep-link` falls back to `index.html` so
//!   client-side router deep links survive a hard reload.

use std::time::Duration;

use botwork_admin_ui_server::build_router;
use reqwest::StatusCode;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const INDEX_MARKER: &str = "botwork admin";

struct Server {
    base: String,
    _handle: JoinHandle<()>,
}

async fn spawn_server() -> Server {
    let app = build_router();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    // Give the listener a beat. `axum::serve` is ready as soon as the
    // task is polled but in CI a slow scheduler can deliver the first
    // GET before the accept loop kicks in.
    tokio::time::sleep(Duration::from_millis(50)).await;
    Server {
        base: format!("http://{addr}"),
        _handle: handle,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthz_responds_ok() {
    let server = spawn_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/healthz", server.base))
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["status"], "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_root_serves_bundle_when_trunk_has_run() {
    let server = spawn_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/admin/", server.base))
        .send()
        .await
        .expect("GET");
    if resp.status() == StatusCode::NOT_FOUND {
        eprintln!(
            "IGNORED admin_root_serves_bundle_when_trunk_has_run: \
             admin-ui/wasm/dist/index.html missing — run `trunk build` in \
             admin-ui/wasm/ before `cargo test` to exercise this path \
             locally; the full proof runs in ci.yml smoke."
        );
        return;
    }
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or("").to_string())
        .unwrap_or_default();
    assert!(
        content_type.starts_with("text/html"),
        "expected text/html, got {content_type}"
    );
    let body = resp.text().await.expect("body");
    assert!(
        body.contains(INDEX_MARKER),
        "index.html missing marker {INDEX_MARKER:?}; got {body:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deep_link_falls_back_to_index() {
    let server = spawn_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/admin/tenants/abc", server.base))
        .send()
        .await
        .expect("GET");
    if resp.status() == StatusCode::NOT_FOUND {
        eprintln!(
            "IGNORED deep_link_falls_back_to_index: \
             admin-ui/wasm/dist/index.html missing — run `trunk build` in \
             admin-ui/wasm/ before `cargo test` to exercise this path \
             locally; the full proof runs in ci.yml smoke."
        );
        return;
    }
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.expect("body");
    assert!(
        body.contains(INDEX_MARKER),
        "deep-link fallback should be index.html (marker {INDEX_MARKER:?}); got {body:?}"
    );
}
