//! botwork-config-broker library entrypoint.
//!
//! Post-RFE #101 PR2, config-broker is a thin reader on top of the
//! `botwork-entity` schema. It resolves a `(tenant, workspace, plugin)`
//! triple to a `PluginDescriptor` by joining four tables and rendering
//! the row(s) into the wire shape session-broker expects.
//!
//! All validation lives in `botwork-bootstrap` on the write side; the
//! broker trusts the DB. The only validation it still does at request
//! time is the regex shape of the request fields themselves
//! (`tenant`/`workspace`/`plugin` names) — operator-facing 400 errors
//! that wouldn't be useful to surface as a 502 from a "row not found".
//!
//! Trust boundary is the docker network (`botwork-internal`). No
//! caller authentication in v0; see README for the full posture.

pub mod handler;
#[cfg(test)]
mod test_support;

pub use handler::{build_router, AppState};
