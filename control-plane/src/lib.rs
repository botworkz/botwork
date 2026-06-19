//! botwork-control-plane library entrypoint.
//!
//! v0 surface:
//!
//! * `sessions` — in-memory `SessionRecord` store with strict upsert/delete
//!   semantics. The store is the authoritative runtime view of "which plugin
//!   sessions exist right now and what policy applies to each." A
//!   `tokio::sync::watch::Sender<u64>` generation counter wakes the xDS
//!   server on every mutation so subscribers reconcile against a fresh
//!   snapshot.
//! * `handler` — axum router exposing `POST /sessions`,
//!   `DELETE /sessions/<id>`, `GET /sessions/<id>`, `GET /sessions`.
//! * `session_broker` + `recovery` — cold-start recovery sync. Polls
//!   session-broker's `GET /control-plane/sessions` admin endpoint on
//!   startup and bulk-seeds the store so an empty in-memory state
//!   after a control-plane restart does not silently drop live
//!   sessions from the xDS feeder's view.
//! * `policy` — pure compilation of `SessionRecord` snapshots into
//!   envoy LDS / CDS resource protos. No IO; called from the xDS
//!   server on every push.
//! * `xds` — tonic `AggregatedDiscoveryService` impl. Serves a single
//!   ADS stream per envoy connection; pushes a fresh LDS resource
//!   on every generation bump.
//!
//! Out of scope for this crate's v0:
//!
//! * Persistence. State is in-memory; control-plane rebuilds on
//!   startup via recovery sync (see `recovery`).
//! * Caller authentication. The trust boundary is the docker network
//!   (`botwork-internal`), same posture as config-broker and auth-broker.
//!
//! See [issue #81](https://github.com/botworkz/botwork/issues/81) for the
//! full design.

pub mod handler;
pub mod policy;
pub mod recovery;
pub mod session_broker;
pub mod sessions;
pub mod xds;

use std::sync::Arc;

pub use handler::{build_router, AppState};
pub use recovery::run_with_retries as run_recovery_with_retries;
pub use session_broker::{fetch_sessions, SessionBrokerError};
pub use sessions::{SessionRecord, SessionStore, StoreError};
pub use xds::AdsServer;

/// Build an `AppState` with an empty in-memory session store.
///
/// Production callers populate it via `recovery::run_with_retries`
/// before binding the HTTP server; tests typically use it as-is.
pub fn build_app_state() -> AppState {
    AppState {
        sessions: Arc::new(SessionStore::new()),
    }
}
