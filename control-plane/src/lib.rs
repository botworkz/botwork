//! botwork-control-plane library entrypoint.
//!
//! v0 surface:
//!
//! * `sessions` — in-memory `SessionRecord` store with strict upsert/delete
//!   semantics. The store is the authoritative runtime view of "which plugin
//!   sessions exist right now and what policy applies to each."
//! * `handler` — axum router exposing `POST /sessions`,
//!   `DELETE /sessions/<id>`, `GET /sessions/<id>`, `GET /sessions`.
//! * `session_broker` + `recovery` — cold-start recovery sync. Polls
//!   session-broker's `GET /control-plane/sessions` admin endpoint on
//!   startup and bulk-seeds the store so an empty in-memory state
//!   after a control-plane restart does not silently drop live
//!   sessions from the (future) xDS feeder's view.
//!
//! Out of scope for this crate's v0:
//!
//! * The xDS server that turns `SessionStore` into push-shaped envoy
//!   resources. Lands in a follow-up PR once the resource-type and tonic
//!   spikes are done (see botworkz/vm#84). Until then the store exists as
//!   the agreed shape for that consumer.
//! * Persistence. State is in-memory; control-plane rebuilds on
//!   startup via recovery sync (see `recovery`).
//! * Caller authentication. The trust boundary is the docker network
//!   (`botwork-internal`), same posture as config-broker and auth-broker.
//!
//! See [issue #81](https://github.com/botworkz/botwork/issues/81) for the
//! full design.

pub mod handler;
pub mod recovery;
pub mod session_broker;
pub mod sessions;

use std::sync::Arc;

pub use handler::{build_router, AppState};
pub use recovery::run_with_retries as run_recovery_with_retries;
pub use session_broker::{fetch_sessions, SessionBrokerError};
pub use sessions::{SessionRecord, SessionStore, StoreError};

/// Build an `AppState` with an empty in-memory session store.
///
/// Production callers populate it via `recovery::run_with_retries`
/// before binding the HTTP server; tests typically use it as-is.
pub fn build_app_state() -> AppState {
    AppState {
        sessions: Arc::new(SessionStore::new()),
    }
}
