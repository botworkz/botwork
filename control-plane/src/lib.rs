//! botwork-control-plane library entrypoint.
//!
//! v0 surface:
//!
//! * `sessions` — in-memory `SessionRecord` store with strict upsert/delete
//!   semantics. The store is the authoritative runtime view of "which plugin
//!   sessions exist right now and what policy applies to each."
//! * `handler` — axum router exposing `POST /sessions`,
//!   `DELETE /sessions/<id>`, `GET /sessions/<id>`, `GET /sessions`.
//!
//! Out of scope for this crate's v0:
//!
//! * The xDS server that turns `SessionStore` into push-shaped envoy
//!   resources. Lands in a follow-up PR once the resource-type and tonic
//!   spikes are done (see botworkz/vm#84). Until then the store exists as
//!   the agreed shape for that consumer.
//! * Persistence. State is in-memory; control-plane rebuilds on restart by
//!   polling session-broker's `/sessions` admin endpoint (which becomes
//!   the source of truth for the runtime view in PR B).
//! * Caller authentication. The trust boundary is the docker network
//!   (`botwork-internal`), same posture as config-broker and auth-broker.
//!
//! See [issue #81](https://github.com/botworkz/botwork/issues/81) for the
//! full design.

pub mod handler;
pub mod sessions;

use std::sync::Arc;

pub use handler::{build_router, AppState};
pub use sessions::{SessionRecord, SessionStore, StoreError};

/// Build an `AppState` with an empty in-memory session store.
///
/// Production callers always start from empty: cold-start recovery is the
/// responsibility of a follow-up subscriber to session-broker's
/// `GET /sessions`, not of this crate.
pub fn build_app_state() -> AppState {
    AppState {
        sessions: Arc::new(SessionStore::new()),
    }
}
