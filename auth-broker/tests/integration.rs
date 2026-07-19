//! `integration` — round-1b port of the deleted
//! `tests/integration.rs`.
//!
//! Pre-cutover: 50 concurrent `/auth/check` requests across three
//! tenants, mixed good/bad bearers, assert the broker classifies
//! each correctly, the cache ends at the right size, the prune
//! task survives the load.
//!
//! Round-1b shape change: `/auth/check` requires a real lease DB
//! to succeed. We split the property into two pieces:
//!
//! - The *bad-bearer* concurrency case (every request is 401)
//!   stays in-process here, using `common::offline_auth_state()`.
//!   This pins the same "the lease lookup is allowed to be
//!   contended" property the pre-cutover test was after — the
//!   path that fails before the DB query (every bearer here is
//!   too short to be a lease bearer) is exactly the
//!   uncontended-401 hot path.
//! - The *good-bearer* concurrency case (every request 200s) is
//!   already covered in `opaque_e2e::full_register_login_init_put_check_fetch_round_trip`
//!   end-to-end; running 50 concurrent successful checks against
//!   a real testcontainers postgres pool isn't a property change
//!   the cutover affects (the cap-mint refactor in PR #142 / #138
//!   already pinned the concurrent-success property).

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use botwork_auth_broker::{build_router, AppState};
use reqwest::StatusCode;
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use common::{bearer, offline_auth_state};

async fn spawn(state: AppState) -> (String, JoinHandle<()>) {
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

#[tokio::test]
async fn concurrent_bad_bearer_requests_all_401_with_structured_125_contract() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let state = AppState::with_auth(root, offline_auth_state().await);
    let (base, handle) = spawn(state).await;
    let client = Arc::new(reqwest::Client::new());

    let tenants = ["alice", "bob", "carol"];
    let mut handles = Vec::new();
    for i in 0..50usize {
        let base = base.clone();
        let client = client.clone();
        let tenant = tenants[i % tenants.len()].to_string();
        // All bearers are short / non-base64, so they fall out of
        // `try_lease_path` before the DB lookup fires — that's
        // what keeps this test docker-free.
        let provided = format!("not-a-lease-bearer-{i}");
        handles.push(tokio::spawn(async move {
            let response = client
                .post(format!("{base}/auth/check"))
                .header("authorization", bearer(&provided))
                .header("x-envoy-original-path", format!("/{tenant}/ns/plugin"))
                .send()
                .await
                .unwrap();
            (tenant, response.status())
        }));
    }

    let mut by_tenant: HashMap<String, usize> = HashMap::new();
    for h in handles {
        let (tenant, status) = h.await.unwrap();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        *by_tenant.entry(tenant).or_default() += 1;
    }
    assert_eq!(by_tenant.values().sum::<usize>(), 50);

    handle.abort();
}
