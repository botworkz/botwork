//! botwork-config-broker library entrypoint.
//!
//! Splits cleanly into two modules:
//! * `registry`: parses `plugins.yaml`, holds `PluginEntry` values.
//! * `handler`: axum router exposing `POST /resolve`, renders the wire shape.
//!
//! The trust boundary is the docker network (or whatever cluster posture is
//! deployed). v0 has no caller authentication; treat any reachable peer as
//! authorised. See README for the full posture.

pub mod handler;
pub mod registry;

use std::path::Path;
use std::sync::Arc;

pub use handler::{build_router, AppState};
pub use registry::{
    load, PluginEntry, PluginRegistry, PluginResources, RegistryError, UpstreamAuth,
    CONFIG_ENV_NAME,
};

/// Build an `AppState` from a `plugins.yaml` path. Refuses to start on
/// validation errors — same posture session-broker had before the split.
pub fn build_app_state(path: &Path) -> Result<AppState, RegistryError> {
    let path_str = path
        .to_str()
        .ok_or_else(|| RegistryError::Invalid(format!("non-UTF8 path: {}", path.display())))?;
    let registry = load(path_str)?;
    Ok(AppState {
        registry: Arc::new(registry),
    })
}
