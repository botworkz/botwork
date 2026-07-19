//! `botwork-cli` — client-side OPAQUE login + lease bearer keyring
//! management for `botwork-auth-broker`.
//!
//! This is the round-1a deliverable for [issue #139][issue-139], which
//! turns the server-side OPAQUE endpoints from #136 into a thing an
//! actual user can run before `goose session`.
//!
//! [issue-139]: https://github.com/botworkz/botwork-extra/issues/139
//!
//! ## Library shape
//!
//! Every subcommand has a `commands::*::run` entry point that takes a
//! typed args struct and returns [`Result<(), LoginError>`]. The CLI
//! (`bin/bw`) is a thin `clap`-driven shim; the library
//! API is designed so a future web UI / admin UI can call the same
//! functions without shelling out.
//!
//! ## Module map
//!
//! - [`config`] — TOML config file + env + CLI flag resolution.
//! - [`duration`] — `humantime` parsing for `--lease`.
//! - [`error`] — [`LoginError`] enum + exit code mapping.
//! - [`keyring_store`] — OS keyring (secret-service / Keychain /
//!   Credential Manager) with a Linux file-fallback at
//!   `~/.config/botspace/keyring/<tenant>.json`.
//! - [`client`] — wire client; drives [`botwork_opaque_handshake`].
//! - [`commands`] — the five subcommands.
//!
//! ## Out of scope (per issue #139)
//!
//! - Auto-refresh / sliding lease renewal on the client side. The
//!   broker slides on `/auth/check`; the CLI just re-`login`s when
//!   the user asks. A future `bw refresh` subcommand can
//!   trade an existing bearer for a fresh one once the broker grows
//!   the matching endpoint.
//! - TUI / multi-tenant switcher.
//! - Web UI (the library shape will accommodate; this crate ships
//!   only the CLI).
//! - Lease revocation. The admin endpoint that `logout --revoke`
//!   would call doesn't exist yet, so v0 is keyring-only.

#![deny(missing_docs)]

pub mod client;
pub mod commands;
pub mod config;
pub mod duration;
pub mod error;
pub mod keyring_store;

pub use config::{Config, ResolvedServerSettings, TenantConfig};
pub use error::{exit_code_for, LoginError};
pub use keyring_store::{KeyringEntry, KeyringStore};

#[cfg(test)]
pub(crate) mod test_env_lock {
    use std::sync::{Mutex, MutexGuard};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: Mutex<()> = Mutex::new(());
        &LOCK
    }

    /// Acquire the env-mutation lock, recovering from a poisoned mutex
    /// so that a panicking test doesn't cascade failures into every
    /// subsequent test that touches env vars.
    pub(crate) fn lock_env() -> MutexGuard<'static, ()> {
        env_lock().lock().unwrap_or_else(|p| p.into_inner())
    }
}
