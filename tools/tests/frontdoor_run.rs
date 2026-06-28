use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use botwork_tools::frontdoor;
use botwork_tools::frontdoor::rds::{HOLDING_RDS, INGRESS_RDS};

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_string()).collect()
}

async fn spawn_fake(
    status: StatusCode,
    bodies: &'static [&'static str],
) -> (String, JoinHandle<()>) {
    assert!(!bodies.is_empty(), "fake server requires at least one body");
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let count = Arc::new(AtomicUsize::new(0));

    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let count = Arc::clone(&count);
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let _ = http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |_req: Request<Incoming>| {
                            let idx = count.fetch_add(1, Ordering::Relaxed).min(bodies.len() - 1);
                            let body = bodies[idx];
                            async move {
                                let resp: Response<Full<Bytes>> = Response::builder()
                                    .status(status)
                                    .header("content-type", "text/html; charset=utf-8")
                                    .body(Full::new(Bytes::from(body)))
                                    .expect("response");
                                Ok::<_, Infallible>(resp)
                            }
                        }),
                    )
                    .await;
            });
        }
    });

    (format!("http://{addr}/"), handle)
}

#[tokio::test(flavor = "current_thread")]
async fn status_reports_closed_when_holding_marker_present() {
    let (url, _h) = spawn_fake(StatusCode::OK, &["x frontdoor: hello world y"]).await;
    let argv = argv(&["status", "--probe-url", &url]);
    let (code, out) = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        let code = frontdoor::run_with_writer(&argv, &mut out).expect("run");
        (code, out)
    })
    .await
    .expect("join");
    assert_eq!(code, 0);
    assert_eq!(String::from_utf8(out).expect("utf8"), "closed\n");
}

#[tokio::test(flavor = "current_thread")]
async fn status_reports_open_when_marker_absent() {
    let (url, _h) = spawn_fake(StatusCode::OK, &["hello from upstream"]).await;
    let argv = argv(&["status", "--probe-url", &url]);
    let (code, out) = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        let code = frontdoor::run_with_writer(&argv, &mut out).expect("run");
        (code, out)
    })
    .await
    .expect("join");
    assert_eq!(code, 0);
    assert_eq!(String::from_utf8(out).expect("utf8"), "open\n");
}

#[tokio::test(flavor = "current_thread")]
async fn status_reports_unknown_when_unreachable() {
    let argv = argv(&["status", "--probe-url", "http://127.0.0.1:1/"]);
    let (code, out) = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        let code = frontdoor::run_with_writer(&argv, &mut out).expect("run");
        (code, out)
    })
    .await
    .expect("join");
    assert_eq!(code, 3);
    assert_eq!(String::from_utf8(out).expect("utf8"), "unknown\n");
}

#[tokio::test(flavor = "current_thread")]
async fn open_writes_ingress_rds_and_observes_open_state() {
    let (url, _h) = spawn_fake(
        StatusCode::OK,
        &["frontdoor: hello world", "hello from upstream"],
    )
    .await;
    let dir = tempdir().expect("tempdir");
    let argv = argv(&[
        "open",
        "--rds-dir",
        dir.path().to_str().expect("utf8 path"),
        "--probe-url",
        &url,
        "--timeout",
        "5",
    ]);
    let code = tokio::task::spawn_blocking(move || frontdoor::run(&argv))
        .await
        .expect("join")
        .expect("run");
    assert_eq!(code, 0);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("active.yaml")).expect("read"),
        INGRESS_RDS
    );
}

#[tokio::test(flavor = "current_thread")]
async fn close_writes_holding_rds_and_observes_closed_state() {
    let (url, _h) = spawn_fake(
        StatusCode::OK,
        &["hello from upstream", "frontdoor: hello world"],
    )
    .await;
    let dir = tempdir().expect("tempdir");
    let argv = argv(&[
        "close",
        "--rds-dir",
        dir.path().to_str().expect("utf8 path"),
        "--probe-url",
        &url,
        "--timeout",
        "5",
    ]);
    let code = tokio::task::spawn_blocking(move || frontdoor::run(&argv))
        .await
        .expect("join")
        .expect("run");
    assert_eq!(code, 0);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("active.yaml")).expect("read"),
        HOLDING_RDS
    );
}

#[tokio::test(flavor = "current_thread")]
async fn open_times_out_when_state_never_changes() {
    let (url, _h) = spawn_fake(StatusCode::OK, &["frontdoor: hello world"]).await;
    let dir = tempdir().expect("tempdir");
    let argv = argv(&[
        "open",
        "--rds-dir",
        dir.path().to_str().expect("utf8 path"),
        "--probe-url",
        &url,
        "--timeout",
        "2",
    ]);
    let err = tokio::task::spawn_blocking(move || frontdoor::run(&argv))
        .await
        .expect("join")
        .expect_err("must timeout");
    assert_eq!(err.exit_code(), 5);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("active.yaml")).expect("read"),
        INGRESS_RDS
    );
}

#[tokio::test(flavor = "current_thread")]
async fn open_fails_with_exit_4_when_rds_dir_missing() {
    let argv = argv(&["open", "--rds-dir", "/definitely/not/there", "--no-wait"]);
    let err = tokio::task::spawn_blocking(move || frontdoor::run(&argv))
        .await
        .expect("join")
        .expect_err("must fail");
    assert_eq!(err.exit_code(), 4);
}
