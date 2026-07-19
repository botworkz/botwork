//! Filesystem-backed lease keyring storage.
//!
//! ## Why files, not an OS keyring backend
//!
//! Round-1a deployments are libvirt VMs and docker containers — neither
//! runs a D-Bus session, so the `secret-service` keystore that
//! `keyring`'s default Linux backend reaches for is unreachable. The
//! `keyring` 3.x crate then silently falls back to its `mock` backend,
//! which is per-`Entry` in-process memory: `set_password` on one
//! `Entry::new(…)` is invisible to a fresh `Entry::new(…)` in the
//! next process — exactly what broke the round-trip test in CI.
//!
//! v0 sidesteps the whole shape by using a single, deterministic
//! storage path on every platform: one JSON file per tenant under
//! `~/.config/botspace/keyring/<tenant>.json` (mode `0600`, atomic
//! tempfile + rename). A future iteration can re-introduce
//! OS-native backends (Keychain on macOS, Credential Manager on
//! Windows, `linux-keyutils` on Linux) behind a Cargo feature with
//! a deliberate per-platform probe at startup — not silently
//! through the `keyring` crate's mock-on-no-backend fallback.
//!
//! ## Storage shape
//!
//! Each tenant is one [`KeyringEntry`] JSON blob. v0 carries the
//! whole lease state inline — bearer, lease id, expires_at, server
//! URL, credential identifier, suite version.
//!
//! ## Path resolution
//!
//! - `$BOTWORK_LOGIN_KEYRING_DIR` if set (used by tests + power users).
//! - else `$XDG_CONFIG_HOME/botspace/keyring/` if set.
//! - else `$HOME/.config/botspace/keyring/`.
//! - none of the above set → [`KeyringStorageError::NoBackend`].
//!
//! ## Round-trip
//!
//! ```ignore
//! use botwork_cli::keyring_store::{KeyringEntry, KeyringStore};
//! let store = KeyringStore::new();
//! let entry = KeyringEntry { /* … */ };
//! store.write("phlax", &entry)?;
//! let loaded = store.read("phlax")?;
//! ```

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use zeroize::ZeroizeOnDrop;

use crate::error::keyring_storage_error::KeyringStorageError;
use crate::error::LoginError;

/// Encoded JSON payload stored under one tenant's keyring file.
///
/// `bearer` is the only secret-grade field; we still derive
/// `ZeroizeOnDrop` over the whole struct so a future field that
/// stores key-derivation material can't accidentally bypass the
/// wipe.
///
/// `suite_version` mirrors [`botwork_opaque_handshake::SUITE_VERSION`]
/// captured at login time — so a future broker that bumps the
/// OPAQUE suite can refuse to use a stale lease via `status` rather
/// than `/auth/check` silently 401ing.
#[derive(Debug, Clone, Serialize, Deserialize, ZeroizeOnDrop)]
pub struct KeyringEntry {
    /// URL-safe-base64 (no pad) encoded bearer token, exactly as
    /// emitted by `/auth/login/finish`.
    pub bearer: String,
    /// UUID of the lease row on the broker. Used by future admin
    /// flows (lease revoke, lease list).
    #[zeroize(skip)]
    pub lease_id: uuid::Uuid,
    /// Absolute expiry timestamp as returned by the broker. The
    /// broker is the source of truth; client-side wall-clock drift
    /// just means `status` may show a positive `remaining` after
    /// the server has already evicted the row.
    #[zeroize(skip)]
    pub expires_at: DateTime<Utc>,
    /// Server URL the lease was minted against. Carried so `env` /
    /// `status` can echo it back without re-resolving the config;
    /// also lets a future `refresh` know which server to call.
    #[zeroize(skip)]
    pub server: String,
    /// OPAQUE credential identifier used at login time. Carried so
    /// re-login uses the same value even if the config file
    /// changes between logins (changing the identifier
    /// effectively makes the broker treat this as a different
    /// user).
    #[zeroize(skip)]
    pub credential_identifier: String,
    /// OPAQUE suite version observed at login time.
    #[zeroize(skip)]
    pub suite_version: u8,
}

impl KeyringEntry {
    /// Return `true` when the entry's `expires_at` is in the past
    /// relative to `now`.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires_at
    }
}

/// Keyring storage backend. Holds no state today — kept as a struct
/// so a future call-site that wants to inject a mock backend has a
/// type to write against, and so the public API doesn't move when
/// the OS-native backends land in a follow-up.
#[derive(Debug, Default)]
pub struct KeyringStore;

impl KeyringStore {
    /// Construct the default backend selection. Cheap — the actual
    /// filesystem touch happens at `write` / `read` time.
    pub fn new() -> Self {
        Self
    }

    /// Persist an entry for `tenant`.
    pub fn write(&self, tenant: &str, entry: &KeyringEntry) -> Result<(), LoginError> {
        let payload = serde_json::to_string(entry).map_err(KeyringStorageError::from)?;
        file_write(tenant, &payload)
    }

    /// Load an entry for `tenant`. Returns `None` if no entry exists.
    pub fn read(&self, tenant: &str) -> Result<Option<KeyringEntry>, LoginError> {
        match file_read(tenant)? {
            Some(payload) => Ok(Some(parse_entry(&payload)?)),
            None => Ok(None),
        }
    }

    /// Remove an entry for `tenant`. Returns `Ok(true)` if a file
    /// existed and was removed; `Ok(false)` if nothing was there.
    /// Idempotent so the CLI's `logout` can be run confidently
    /// without checking whether the user is logged in.
    pub fn delete(&self, tenant: &str) -> Result<bool, LoginError> {
        file_delete(tenant)
    }
}

fn parse_entry(payload: &str) -> Result<KeyringEntry, LoginError> {
    serde_json::from_str(payload).map_err(|err| LoginError::Keyring(err.into()))
}

/// Resolve the directory the file storage writes into. Prefers
/// `$BOTWORK_LOGIN_KEYRING_DIR` (set by tests so they don't scribble
/// on the user's `$HOME`), then `$XDG_CONFIG_HOME/botspace/keyring`,
/// then `$HOME/.config/botspace/keyring`. Returns an error if no
/// candidate resolves.
fn keyring_dir() -> Result<PathBuf, KeyringStorageError> {
    if let Ok(value) = std::env::var("BOTWORK_LOGIN_KEYRING_DIR") {
        if !value.is_empty() {
            return Ok(PathBuf::from(value));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("botspace").join("keyring"));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return Ok(PathBuf::from(home)
                .join(".config")
                .join("botspace")
                .join("keyring"));
        }
    }
    Err(KeyringStorageError::NoBackend(
        "neither BOTWORK_LOGIN_KEYRING_DIR, XDG_CONFIG_HOME, nor HOME is set; \
         cannot resolve a writable keyring directory"
            .to_string(),
    ))
}

fn file_for(tenant: &str) -> Result<PathBuf, KeyringStorageError> {
    // Tenant names are validated as `[A-Za-z0-9._-]+` upstream
    // (auth-broker's safe_component_re), but the file path doubles
    // as an enumeration surface — defensively reject any tenant
    // name that contains a path separator so a maliciously-named
    // tenant can't escape the keyring dir.
    if tenant.contains('/') || tenant.contains('\\') || tenant == "." || tenant == ".." {
        return Err(KeyringStorageError::NoBackend(format!(
            "refusing to write keyring entry for tenant name '{tenant}' (contains path separator)"
        )));
    }
    Ok(keyring_dir()?.join(format!("{tenant}.json")))
}

fn file_write(tenant: &str, payload: &str) -> Result<(), LoginError> {
    let path = file_for(tenant).map_err(LoginError::Keyring)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| LoginError::Keyring(err.into()))?;
        // Best-effort tighten on the parent dir too — anything we
        // miss the per-file mode 0600 will still cover, but a 0700
        // parent makes the contents inaccessible to other users on
        // the system in the first place.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }

    // Write via a tempfile in the same directory so the rename is
    // atomic and a power loss can't leave a half-written entry.
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut handle = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .map_err(|err| LoginError::Keyring(err.into()))?;
        handle
            .write_all(payload.as_bytes())
            .map_err(|err| LoginError::Keyring(err.into()))?;
        handle
            .sync_all()
            .map_err(|err| LoginError::Keyring(err.into()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|err| LoginError::Keyring(err.into()))?;
    }
    std::fs::rename(&tmp, &path).map_err(|err| LoginError::Keyring(err.into()))?;
    Ok(())
}

fn file_read(tenant: &str) -> Result<Option<String>, LoginError> {
    let path = match file_for(tenant) {
        Ok(p) => p,
        Err(KeyringStorageError::NoBackend(_)) => return Ok(None),
        Err(err) => return Err(LoginError::Keyring(err)),
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(Some(content)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(LoginError::Keyring(err.into())),
    }
}

fn file_delete(tenant: &str) -> Result<bool, LoginError> {
    let path = match file_for(tenant) {
        Ok(p) => p,
        Err(KeyringStorageError::NoBackend(_)) => return Ok(false),
        Err(err) => return Err(LoginError::Keyring(err)),
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(LoginError::Keyring(err.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Tests in this module mutate `BOTWORK_LOGIN_KEYRING_DIR` /
    /// `XDG_CONFIG_HOME` / `HOME` to drive the path resolver
    /// through specific code paths. Cargo's default test runner is
    /// multi-threaded, so the mutations race; we serialise them
    /// behind a `Mutex` so each env-mutating test gets a clean
    /// window. `std::sync::Mutex` is enough — these tests are
    /// sub-millisecond so lock contention is trivial.
    fn env_lock() -> &'static Mutex<()> {
        crate::test_env_lock::env_lock()
    }

    fn fixture_entry() -> KeyringEntry {
        KeyringEntry {
            bearer: "ABCDEF0123456789".to_string(),
            lease_id: uuid::Uuid::nil(),
            expires_at: Utc::now() + chrono::Duration::seconds(3_600),
            server: "http://192.168.122.50:9100".to_string(),
            credential_identifier: "phlax".to_string(),
            suite_version: botwork_opaque_handshake::SUITE_VERSION,
        }
    }

    #[test]
    fn json_round_trip_preserves_every_field() {
        let entry = fixture_entry();
        let s = serde_json::to_string(&entry).unwrap();
        let back: KeyringEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(back.bearer, entry.bearer);
        assert_eq!(back.lease_id, entry.lease_id);
        assert_eq!(back.expires_at, entry.expires_at);
        assert_eq!(back.server, entry.server);
        assert_eq!(back.credential_identifier, entry.credential_identifier);
        assert_eq!(back.suite_version, entry.suite_version);
    }

    #[test]
    fn is_expired_compares_against_now() {
        let mut entry = fixture_entry();
        let now = Utc::now();
        // expires_at one hour from now → not expired now, expired in 2h.
        assert!(!entry.is_expired(now));
        assert!(entry.is_expired(now + chrono::Duration::seconds(7200)));
        // expires_at in the past → expired regardless of how `now` shifts.
        entry.expires_at = now - chrono::Duration::seconds(1);
        assert!(entry.is_expired(now));
    }

    #[test]
    fn store_round_trips_via_env_dir() {
        let _lock = env_lock().lock().unwrap();
        let dir = TempDir::new().unwrap();
        std::env::set_var("BOTWORK_LOGIN_KEYRING_DIR", dir.path());

        let store = KeyringStore::new();
        let entry = fixture_entry();
        store.write("phlax", &entry).expect("write");
        let back = store
            .read("phlax")
            .expect("read")
            .expect("Some after write");
        assert_eq!(back.bearer, entry.bearer);

        // Mode 0600 on the file.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = file_for("phlax").unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "keyring file must be mode 0600");
        }

        assert!(store.delete("phlax").unwrap());
        // Second delete is a no-op.
        assert!(!store.delete("phlax").unwrap());
        // Read after delete is None.
        assert!(store.read("phlax").unwrap().is_none());

        std::env::remove_var("BOTWORK_LOGIN_KEYRING_DIR");
    }

    #[test]
    fn rejects_path_traversal_in_tenant_name() {
        let _lock = env_lock().lock().unwrap();
        let dir = TempDir::new().unwrap();
        std::env::set_var("BOTWORK_LOGIN_KEYRING_DIR", dir.path());
        for bad in ["..", ".", "../escape", "with/slash", r"win\slash"] {
            let err = file_for(bad).expect_err(bad);
            assert!(
                matches!(err, KeyringStorageError::NoBackend(_)),
                "should refuse {bad}; got {err:?}"
            );
        }
        std::env::remove_var("BOTWORK_LOGIN_KEYRING_DIR");
    }

    #[test]
    fn missing_dir_env_is_an_error_at_resolve_time() {
        let _lock = env_lock().lock().unwrap();
        // Save + clear all three env vars the resolver consults.
        let saved_keyring = std::env::var("BOTWORK_LOGIN_KEYRING_DIR").ok();
        let saved_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let saved_home = std::env::var("HOME").ok();
        std::env::remove_var("BOTWORK_LOGIN_KEYRING_DIR");
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("HOME");
        let err = keyring_dir().expect_err("no env should error");
        assert!(matches!(err, KeyringStorageError::NoBackend(_)));
        if let Some(v) = saved_keyring {
            std::env::set_var("BOTWORK_LOGIN_KEYRING_DIR", v);
        }
        if let Some(v) = saved_xdg {
            std::env::set_var("XDG_CONFIG_HOME", v);
        }
        if let Some(v) = saved_home {
            std::env::set_var("HOME", v);
        }
    }

    #[test]
    fn keyring_dir_falls_back_from_xdg_to_home() {
        let _lock = env_lock().lock().unwrap();
        std::env::remove_var("BOTWORK_LOGIN_KEYRING_DIR");
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg-config-home");
        std::env::set_var("HOME", "/tmp/home-dir");
        assert_eq!(
            keyring_dir().unwrap(),
            PathBuf::from("/tmp/xdg-config-home")
                .join("botspace")
                .join("keyring")
        );

        std::env::remove_var("XDG_CONFIG_HOME");
        assert_eq!(
            keyring_dir().unwrap(),
            PathBuf::from("/tmp/home-dir")
                .join(".config")
                .join("botspace")
                .join("keyring")
        );
        std::env::remove_var("HOME");
    }

    #[test]
    fn malformed_json_payload_is_shaped_as_keyring_error() {
        let err = parse_entry("{not-json").unwrap_err();
        assert!(matches!(err, LoginError::Keyring(_)), "got {err:?}");
    }

    #[test]
    fn read_and_delete_are_noops_when_backend_cannot_be_resolved() {
        let _lock = env_lock().lock().unwrap();
        let saved_keyring = std::env::var("BOTWORK_LOGIN_KEYRING_DIR").ok();
        let saved_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let saved_home = std::env::var("HOME").ok();
        std::env::remove_var("BOTWORK_LOGIN_KEYRING_DIR");
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("HOME");

        let store = KeyringStore::new();
        assert!(store.read("phlax").unwrap().is_none());
        assert!(!store.delete("phlax").unwrap());

        if let Some(v) = saved_keyring {
            std::env::set_var("BOTWORK_LOGIN_KEYRING_DIR", v);
        }
        if let Some(v) = saved_xdg {
            std::env::set_var("XDG_CONFIG_HOME", v);
        }
        if let Some(v) = saved_home {
            std::env::set_var("HOME", v);
        }
    }

    #[test]
    fn read_and_delete_surface_non_notfound_fs_errors() {
        let _lock = env_lock().lock().unwrap();
        let dir = TempDir::new().unwrap();
        std::env::set_var("BOTWORK_LOGIN_KEYRING_DIR", dir.path());

        // Create a directory where the tenant file should be so read_to_string/remove_file
        // both fail with a non-NotFound error path.
        let tenant_path = dir.path().join("phlax.json");
        std::fs::create_dir_all(&tenant_path).unwrap();

        let store = KeyringStore::new();
        let read_err = store.read("phlax").unwrap_err();
        assert!(
            matches!(read_err, LoginError::Keyring(_)),
            "got {read_err:?}"
        );

        let delete_err = store.delete("phlax").unwrap_err();
        assert!(
            matches!(delete_err, LoginError::Keyring(_)),
            "got {delete_err:?}"
        );

        std::env::remove_var("BOTWORK_LOGIN_KEYRING_DIR");
    }
}
