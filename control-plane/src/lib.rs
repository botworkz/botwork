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
//! * `recovery` — cold-start recovery sync, post-RFE-#105-round-3.
//!   Reads `session_worker` JOIN `agent_session` (plus tenant/workspace/
//!   plugin) directly from postgres on startup and bulk-seeds the
//!   store so an empty in-memory state after a control-plane restart
//!   does not silently drop live sessions from the xDS feeder's view.
//!   See `recovery::run_with_retries` for the wire-shape mapping.
//! * `policy` — pure compilation of `SessionRecord` snapshots into
//!   envoy LDS / CDS resource protos. No IO; called from the xDS
//!   server on every push.
//! * `xds` — tonic `AggregatedDiscoveryService` impl. Serves a single
//!   ADS stream per envoy connection; pushes a fresh LDS resource
//!   on every generation bump.
//!
//! Out of scope for this crate's v0:
//!
//! * Persistence of the in-memory `SessionStore` (writes still live
//!   in-process; we re-seed from the DB on every restart).
//! * Caller authentication. The trust boundary is the docker network
//!   (`botwork-internal`), same posture as config-broker and auth-broker.
//!
//! See [issue #81](https://github.com/botworkz/botwork/issues/81) for the
//! full design and [RFE #105](https://github.com/botworkz/botwork/issues/105)
//! for the recovery cutover.

pub mod handler;
pub mod policy;
pub mod recovery;
pub mod sessions;
pub mod xds;

use std::sync::Arc;

pub use handler::{build_router, AppState, ACK_DISABLED_ENV, DEFAULT_ACK_WAIT};
pub use recovery::{run_with_retries as run_recovery_with_retries, RecoveryError};
pub use sessions::{AckWaitError, SessionRecord, SessionStore, StoreError, XdsSubscriberGuard};
pub use xds::AdsServer;

/// Build an `AppState` with an empty in-memory session store and the
/// default ack-wait gate (5s, gate enabled).
///
/// Production callers populate the store via
/// `recovery::run_with_retries` before binding the HTTP server, and
/// usually override `ack_wait` / `ack_disabled` from env in
/// `main.rs`. Tests typically use this as-is or via
/// `AppState::new` directly.
#[cfg(not(tarpaulin_include))]
pub fn build_app_state() -> AppState {
    AppState::new(Arc::new(SessionStore::new()))
}
