//! Cold-start recovery sync.
//!
//! control-plane's `SessionStore` is in-memory; on every restart it
//! starts empty. Without a sync, every restart drops every live
//! session from the (future) xDS feeder's view, and SOTW xDS treats
//! "absent from snapshot" as "removed" -- so the egress envoy would
//! tear down all live routes the moment control-plane restarted.
//!
//! Recovery sync closes that gap by polling session-broker's
//! `GET /control-plane/sessions` admin endpoint at startup and bulk-
//! seeding the store. session-broker is the source of truth for the
//! live transport set; control-plane just rebuilds against it.
//!
//! ## Failure semantics
//!
//! The two cases that matter:
//!
//! * `200 []` -- a legitimate cold start with no live sessions.
//!   Recovery proceeds; `SessionStore` stays empty; control-plane
//!   binds and starts serving. Fresh-deploy boot must work.
//! * `200 [N records]` -- recovered N sessions; store populated;
//!   control-plane binds.
//! * Anything else (transport failure, non-2xx, bad envelope, bad
//!   record) -- we *don't know* the live state, and starting with the
//!   wrong view would silently break the xDS feeder. The retry loop
//!   below tries [`MAX_ATTEMPTS`] times with linear backoff before
//!   giving up; the binary then `exit(1)`s and systemd's
//!   `Restart=always` keeps retrying from scratch.
//!
//! ## Sequencing
//!
//! Recovery requires session-broker to be reachable. The supported
//! systemd ordering puts `botwork-control-plane.service` AFTER
//! `botwork-session-broker.service` (and `Wants=`/`After=`, not
//! `Requires=`, in either direction -- a hard mutual dependency would
//! deadlock on first boot).
//!
//! session-broker's in-process control-plane hard gate (botwork #82)
//! is what actually enforces "no unpoliced traffic ever serves" -- the
//! systemd order is just a sequencing convenience. session-broker
//! itself tolerates control-plane being temporarily unreachable at
//! boot: the gate fires per-spawn, so a cold-start window where
//! control-plane is still syncing produces 503s on new spawns but does
//! not break the already-running ones.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;
use tracing::{info, warn};

use crate::session_broker::{fetch_sessions, SessionBrokerError};
use crate::sessions::SessionStore;

const PREFIX: &str = "[control-plane:recovery]";

/// How many fetch attempts before recovery gives up and the binary
/// exits non-zero. 30 × ~5s spacing = ~150s headroom for a slow
/// session-broker cold start; longer than that and systemd's restart
/// loop is the right place to retry, not us.
pub const MAX_ATTEMPTS: u32 = 30;

/// Pause between attempts. Linear (not exponential): the dominant
/// failure mode at boot is "session-broker hasn't bound yet," which
/// resolves in seconds; we don't want to back off into minutes for a
/// transient issue, and we'd rather give up and let systemd retry
/// than block startup indefinitely.
pub const ATTEMPT_INTERVAL: Duration = Duration::from_secs(5);

/// Per-attempt request timeout. Generous so a slow loopback HTTP path
/// (e.g. session-broker mid-startup) isn't mistaken for a real
/// failure.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Run the recovery loop. Returns once the store has been seeded
/// (possibly with zero records); errors out only after
/// [`MAX_ATTEMPTS`] failures.
///
/// `store` is the same `Arc<SessionStore>` `AppState` holds; we mutate
/// through that handle so the HTTP server (which `main.rs` starts
/// immediately after this returns) sees the seeded state.
pub async fn run_with_retries(
    store: Arc<SessionStore>,
    endpoint: &str,
) -> Result<usize, SessionBrokerError> {
    run_with_retries_config(
        store,
        endpoint,
        MAX_ATTEMPTS,
        ATTEMPT_INTERVAL,
        REQUEST_TIMEOUT,
    )
    .await
}

/// Test-visible variant with knobs exposed. Keeps `run_with_retries`
/// a one-line caller so the call site is short and the defaults are
/// the single source of production behaviour.
pub async fn run_with_retries_config(
    store: Arc<SessionStore>,
    endpoint: &str,
    max_attempts: u32,
    interval: Duration,
    request_timeout: Duration,
) -> Result<usize, SessionBrokerError> {
    info!("{PREFIX} starting recovery sync against {endpoint} (max_attempts={max_attempts})");

    let mut last_err: Option<SessionBrokerError> = None;
    for attempt in 1..=max_attempts {
        match fetch_sessions(endpoint, request_timeout).await {
            Ok(records) => {
                let count = seed_store(&store, records).await;
                info!(
                    "{PREFIX} recovered {count} session(s) from session-broker on attempt {attempt}/{max_attempts}"
                );
                return Ok(count);
            }
            Err(err) => {
                warn!(
                    "{PREFIX} attempt {attempt}/{max_attempts} failed: {err}; retrying in {:?}",
                    interval
                );
                last_err = Some(err);
                if attempt < max_attempts {
                    sleep(interval).await;
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        SessionBrokerError::Transport("recovery exhausted with no recorded error".to_string())
    }))
}

/// Bulk-insert recovered records into the store.
///
/// Uses `SessionStore::insert` -- the strict, ergonomic-per-spawn
/// path. In recovery we *expect* the store to be empty, so duplicate
/// errors here would be a bug (probably someone calling
/// `run_with_retries` twice on the same store, or seeding while the
/// HTTP server is already accepting POSTs). We log + skip dupes
/// rather than abort so a transient "control-plane started early"
/// double-call doesn't bring down the binary.
async fn seed_store(store: &SessionStore, records: Vec<crate::sessions::SessionRecord>) -> usize {
    let mut inserted = 0usize;
    for record in records {
        let id = record.session_id.clone();
        match store.insert(record).await {
            Ok(()) => inserted += 1,
            Err(err) => {
                warn!("{PREFIX} skipping recovered record {id}: {err}");
            }
        }
    }
    inserted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::SessionRecord;

    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use http_body_util::Full;
    use hyper::body::{Bytes, Incoming};
    use hyper::server::conn::http1 as server_http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    /// Mini server whose response per-attempt can vary.  Body N is
    /// served on the Nth request; once exhausted, the last body is
    /// served repeatedly so a test that only schedules MAX_ATTEMPTS
    /// retries can stop without exact-count tracking.
    async fn spawn_sequenced(bodies: Vec<(StatusCode, &'static str)>) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let counter = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(bodies);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let counter = counter.clone();
                let bodies = bodies.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let _ = server_http1::Builder::new()
                        .serve_connection(
                            io,
                            service_fn(move |_req: Request<Incoming>| {
                                let counter = counter.clone();
                                let bodies = bodies.clone();
                                async move {
                                    let idx = counter.fetch_add(1, Ordering::SeqCst);
                                    let pick = bodies.get(idx).unwrap_or_else(|| {
                                        bodies.last().expect("at least one body")
                                    });
                                    let response: Response<Full<Bytes>> = Response::builder()
                                        .status(pick.0)
                                        .header("content-type", "application/json")
                                        .body(Full::new(Bytes::from(pick.1)))
                                        .expect("build response");
                                    Ok::<_, Infallible>(response)
                                }
                            }),
                        )
                        .await;
                });
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn first_attempt_succeeds_with_empty_array() {
        let (endpoint, _h) = spawn_sequenced(vec![(StatusCode::OK, r#"{"sessions":[]}"#)]).await;
        let store = Arc::new(SessionStore::new());
        let count = run_with_retries_config(
            store.clone(),
            &endpoint,
            3,
            Duration::from_millis(10),
            Duration::from_secs(1),
        )
        .await
        .expect("ok");
        assert_eq!(count, 0);
        assert!(store.is_empty().await);
    }

    #[tokio::test]
    async fn first_attempt_succeeds_with_records() {
        let (endpoint, _h) = spawn_sequenced(vec![(
            StatusCode::OK,
            r#"{"sessions":[
                {"session_id":"mcp_session_a","container_ip":"172.20.0.5","tenant":"phlax","namespace":"mcp","plugin":"fetch","egress_policy":null},
                {"session_id":"mcp_session_b","container_ip":"172.20.0.6","tenant":"phlax","namespace":"mcp","plugin":"git","egress_policy":{"mode":"allow_all"}}
            ]}"#,
        )])
        .await;
        let store = Arc::new(SessionStore::new());
        let count = run_with_retries_config(
            store.clone(),
            &endpoint,
            3,
            Duration::from_millis(10),
            Duration::from_secs(1),
        )
        .await
        .expect("ok");
        assert_eq!(count, 2);
        assert_eq!(store.len().await, 2);
        let listed = store.list().await;
        assert_eq!(listed[0].session_id, "mcp_session_a");
        assert_eq!(listed[1].plugin, "git");
    }

    #[tokio::test]
    async fn retries_then_succeeds() {
        // Two 503s, then a 200. The retry loop must outlast the
        // transient failures and pick up the eventual good response.
        let (endpoint, _h) = spawn_sequenced(vec![
            (StatusCode::SERVICE_UNAVAILABLE, r#"{"error":"warming"}"#),
            (StatusCode::SERVICE_UNAVAILABLE, r#"{"error":"warming"}"#),
            (StatusCode::OK, r#"{"sessions":[]}"#),
        ])
        .await;
        let store = Arc::new(SessionStore::new());
        let count = run_with_retries_config(
            store.clone(),
            &endpoint,
            5,
            Duration::from_millis(10),
            Duration::from_secs(1),
        )
        .await
        .expect("eventual ok");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        // Permanent 500 -- recovery must surface the final error
        // rather than block forever.
        let (endpoint, _h) = spawn_sequenced(vec![(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"boom"}"#,
        )])
        .await;
        let store = Arc::new(SessionStore::new());
        let err = run_with_retries_config(
            store.clone(),
            &endpoint,
            3,
            Duration::from_millis(10),
            Duration::from_secs(1),
        )
        .await
        .expect_err("must give up");
        match err {
            SessionBrokerError::BadStatus { status, .. } => assert_eq!(status, 500),
            other => panic!("expected BadStatus, got {other:?}"),
        }
        assert!(store.is_empty().await);
    }

    #[tokio::test]
    async fn gives_up_on_unreachable_endpoint() {
        // Port 1 is reserved for tcpmux which nobody runs.
        let store = Arc::new(SessionStore::new());
        let err = run_with_retries_config(
            store.clone(),
            "http://127.0.0.1:1",
            3,
            Duration::from_millis(10),
            Duration::from_millis(100),
        )
        .await
        .expect_err("must give up");
        assert!(matches!(err, SessionBrokerError::Transport(_)), "{err:?}");
    }

    #[tokio::test]
    async fn seed_store_skips_duplicates_without_aborting() {
        // Recovery is supposed to run against an empty store, but if a
        // record collides (e.g. someone seeded externally) we log and
        // continue rather than blow up.
        let store = SessionStore::new();
        let pre = SessionRecord {
            session_id: "mcp_session_a".to_string(),
            container_ip: "172.20.0.5".parse().unwrap(),
            tenant: "phlax".to_string(),
            namespace: "mcp".to_string(),
            plugin: "fetch".to_string(),
            egress_policy: serde_json::Value::Null,
        };
        store.insert(pre).await.expect("pre-insert");

        let records = vec![
            SessionRecord {
                session_id: "mcp_session_a".to_string(),
                container_ip: "172.20.0.5".parse().unwrap(),
                tenant: "phlax".to_string(),
                namespace: "mcp".to_string(),
                plugin: "fetch".to_string(),
                egress_policy: serde_json::Value::Null,
            },
            SessionRecord {
                session_id: "mcp_session_b".to_string(),
                container_ip: "172.20.0.6".parse().unwrap(),
                tenant: "phlax".to_string(),
                namespace: "mcp".to_string(),
                plugin: "git".to_string(),
                egress_policy: serde_json::Value::Null,
            },
        ];
        let inserted = seed_store(&store, records).await;
        // The new id was inserted; the colliding one was logged-and-skipped.
        assert_eq!(inserted, 1);
        assert_eq!(store.len().await, 2);
    }
}
