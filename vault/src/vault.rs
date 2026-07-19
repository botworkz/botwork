use std::fs;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use chrono::Utc;
use rand::RngCore;
use tracing::{debug, info, trace, warn};
use zeroize::{Zeroize, Zeroizing};

use crate::atomic::{atomic_write_private, ensure_vault_root};
use crate::contents::{
    open_entry, seal_entry, DecryptedSecret, EntryEnvelope, EntryMeta, SecretEntry, SecretKey,
    SecretMeta, VaultContents,
};
use crate::error::VaultError;
use crate::kdf::{self, KEY_LEN, SALT_LEN};
use crate::lock::{init_generation, peek_generation, VaultLock};
use crate::path::{validate_name, validate_service};
use crate::PREFIX;

// On-disk vault file layout.
//
// `header_core` (22 bytes) — fed verbatim to the AEAD as associated
// data, so any single-byte tamper of the magic, the version, the
// suite_version, or the salt invalidates the AEAD tag and `decrypt`
// fails:
//
//     [4   magic            "BSVL"          ]
//     [1   format version   4               ]
//     [1   suite version    matches OPAQUE  ]
//     [16  salt             random per vault]
//
// `header_full` (34 bytes) — header_core followed by the per-write
// nonce:
//
//     [12  nonce            random per write]
//
// Then the ciphertext, then the 16-byte AEAD tag. The tag covers
// (header_core, ciphertext). The nonce is part of the file but NOT
// part of the AAD — putting it in AAD would
// be redundant because `ChaCha20Poly1305::decrypt_in_place_detached`
// already takes the nonce as its own argument and the AEAD
// construction binds it into the tag.
// The master key is HKDF-SHA-512-derived from the OPAQUE `export_key`
// and the per-vault salt. A 1-byte `suite_version` in the header is
// checked on unlock so a future OPAQUE suite rotation fails closed
// with [`VaultError::UnsupportedVersion`].
// Per-entry DEK indirection lives inside the AEAD payload
// ([`crate::contents::EntryEnvelope`]); see `vault/README.md`.
const MAGIC: &[u8; 4] = b"BSVL";
const FORMAT_VERSION: u8 = 4;
const HEADER_CORE_LEN: usize = 4 + 1 + 1 + SALT_LEN;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const HEADER_FULL_LEN: usize = HEADER_CORE_LEN + NONCE_LEN;

fn now_unix() -> i64 {
    Utc::now().timestamp()
}

/// Build the 22-byte `header_core` for a vault file: magic, format
/// version, suite version, salt. The exact byte string returned
/// here is what gets written to disk AND what gets passed as AAD
/// to the AEAD, so the encoding has to be deterministic.
fn header_core(suite_version: u8, salt: &[u8; SALT_LEN]) -> Vec<u8> {
    let mut header = Vec::with_capacity(HEADER_CORE_LEN);
    header.extend_from_slice(MAGIC);
    header.push(FORMAT_VERSION);
    header.push(suite_version);
    header.extend_from_slice(salt);
    header
}

fn gen_nonce() -> [u8; NONCE_LEN] {
    let mut n = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut n);
    n
}

fn unsupported_version_error(path: &Path) -> VaultError {
    VaultError::UnsupportedVersion {
        path: path.to_path_buf(),
    }
}

/// Seal `contents` with a pre-derived master key.
///
/// The master key is supplied by the caller — vault writes never
/// re-derive from the OPAQUE export_key; the derivation happens
/// exactly once, at [`Vault::unlock_master`] / [`Vault::create`]
/// time. The salt comes from the on-disk header; the nonce is
/// generated fresh per write.
fn seal_with_master(
    contents: &VaultContents,
    master: &[u8; KEY_LEN],
    salt: &[u8; SALT_LEN],
    suite_version: u8,
) -> Result<Vec<u8>, VaultError> {
    let payload = postcard::to_allocvec(contents).map_err(|e| VaultError::Codec(e.to_string()))?;

    let header_core = header_core(suite_version, salt);
    let nonce = gen_nonce();

    // Outer-file ChaCha20-Poly1305 AEAD. Per-entry sealing lives
    // inside `crate::contents`; this is the master-key seal of the
    // whole payload that protects the on-disk file. AAD =
    // header_core, so any tamper of magic, format version, suite
    // version, or salt fails the tag check at open time.
    let cipher = ChaCha20Poly1305::new(Key::from_slice(master));

    let mut buf = payload;
    let tag = cipher
        .encrypt_in_place_detached(Nonce::from_slice(&nonce), &header_core, &mut buf)
        .map_err(|e| VaultError::Integrity(format!("encrypt: {e}")))?;

    let mut out = Vec::with_capacity(HEADER_FULL_LEN + buf.len() + TAG_LEN);
    out.extend_from_slice(&header_core);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&buf);
    out.extend_from_slice(&tag);

    buf.zeroize();

    Ok(out)
}

/// Decoded view returned by `open_contents`. Carries the per-vault
/// salt + suite_version read back from the header so callers can
/// cache them on the unlocked state and avoid re-parsing on every
/// reseal.
struct OpenedVault {
    contents: VaultContents,
    salt: [u8; SALT_LEN],
    suite_version: u8,
}

/// Parse and authenticate a vault file under `master`. Returns
/// [`VaultError::UnsupportedVersion`] for any version byte other
/// than [`FORMAT_VERSION`].
fn open_contents(
    path: &Path,
    data: &[u8],
    master: &[u8; KEY_LEN],
) -> Result<OpenedVault, VaultError> {
    if data.len() < HEADER_FULL_LEN + TAG_LEN {
        return Err(VaultError::Integrity("file too short".to_string()));
    }
    if &data[..4] != MAGIC {
        return Err(VaultError::Integrity("bad magic bytes".to_string()));
    }
    let format_version = data[4];
    if format_version != FORMAT_VERSION {
        return Err(unsupported_version_error(path));
    }
    let suite_version = data[5];

    // `data.len() >= HEADER_FULL_LEN + TAG_LEN` above guarantees the salt
    // window exists in full, so indexing the fixed range is structurally
    // infallible here.
    let salt = std::array::from_fn(|idx| data[6 + idx]);

    let header_core_bytes = &data[..HEADER_CORE_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    // Same guard as above: once the minimum file length check passes the nonce
    // window is exactly `NONCE_LEN` bytes wide.
    nonce.copy_from_slice(&data[HEADER_CORE_LEN..HEADER_FULL_LEN]);
    let ciphertext_end = data.len() - TAG_LEN;
    let ciphertext = &data[HEADER_FULL_LEN..ciphertext_end];
    let tag = &data[ciphertext_end..];

    let cipher = ChaCha20Poly1305::new(Key::from_slice(master));
    let mut plaintext = ciphertext.to_vec();
    cipher
        .decrypt_in_place_detached(
            Nonce::from_slice(&nonce),
            header_core_bytes,
            &mut plaintext,
            chacha20poly1305::Tag::from_slice(tag),
        )
        .map_err(|_| {
            let message = format!(
                "{PREFIX} open_contents: AEAD authentication failed path={}",
                path.display()
            );
            warn!("{message}");
            VaultError::Auth
        })?;

    let contents =
        postcard::from_bytes(&plaintext).map_err(|e| VaultError::Codec(e.to_string()))?;

    plaintext.zeroize();

    Ok(OpenedVault {
        contents,
        salt,
        suite_version,
    })
}

/// Opaque holder for the v4 master key.
///
/// Wraps a `Zeroizing<[u8; 32]>` so the bytes are scrubbed on drop.
/// No `Clone`, no `Debug` that leaks bytes — same opacity discipline
/// as the `WrappingKey` shape from auth-broker (issue #136). The
/// only public operations are construction via
/// [`Vault::unlock_master`] / [`Vault::create`] (internal) and
/// being handed back into [`Vault::decrypt_entry`].
pub struct UnlockedMasterKey {
    bytes: Zeroizing<[u8; KEY_LEN]>,
}

impl UnlockedMasterKey {
    fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self {
            bytes: Zeroizing::new(bytes),
        }
    }

    /// Test-only constructor: build an [`UnlockedMasterKey`] from
    /// raw bytes the caller already holds. Used by the
    /// auth-broker's `tests/common::seed_synthetic_lease` fixture
    /// to install the same `CacheEntry` shape `/auth/check` would
    /// have produced after a real OPAQUE round-trip — see
    /// `auth-broker/tests/common/mod.rs`.
    ///
    /// Production code minting an `UnlockedMasterKey` must go
    /// through [`Vault::unlock_master`] / [`Vault::create`]; the
    /// constructor is gated on `cfg(any(test, feature =
    /// "test-support"))` so the binary build can't reach it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_master_bytes_for_test(bytes: [u8; KEY_LEN]) -> Self {
        Self::from_bytes(bytes)
    }

    fn as_array(&self) -> &[u8; KEY_LEN] {
        &self.bytes
    }

    /// Borrow the raw master-key bytes. Exposed so the auth-broker
    /// cache can hand the master to a fresh
    /// [`Vault::unlock_with_master`] call without round-tripping
    /// back through the OPAQUE export_key (which the cache
    /// deliberately doesn't retain — every cache entry holds the
    /// already-derived master and nothing else). Returning a borrow
    /// rather than an owned copy means the same `UnlockedMasterKey`
    /// can drive many fetches without ever being cloned into a
    /// second heap allocation.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..]
    }
}

impl std::fmt::Debug for UnlockedMasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Print the type only — the raw bytes would land in any
        // tracing `?key` site and that's the leak path we wrap the
        // key to avoid.
        f.debug_struct("UnlockedMasterKey").finish_non_exhaustive()
    }
}

struct UnlockedState {
    contents: VaultContents,
    /// HKDF-derived master key. Cached so each write hits
    /// [`seal_with_master`] and pays zero work re-deriving from
    /// the export_key. Wiped on drop via `Zeroizing`.
    master: Zeroizing<[u8; KEY_LEN]>,
    /// Per-vault salt from the file header. Held stable across
    /// writes — salt's purpose is per-vault uniqueness, not
    /// per-write freshness.
    salt: [u8; SALT_LEN],
    /// OPAQUE suite version recorded in the file header. Bound
    /// into the master-key derivation via HKDF `info` so a future
    /// suite rotation surfaces as a fresh master that doesn't
    /// open old files.
    suite_version: u8,
    /// Generation counter from `vault.botwork.gen` at the time this
    /// state was loaded. Every successful [`Vault::persist`] bumps
    /// the on-disk counter and updates this field so the next call
    /// stays in sync. If a concurrent writer changes the counter
    /// between unlock and persist, `persist` returns
    /// [`VaultError::Conflict`].
    generation: u64,
}

impl Drop for UnlockedState {
    fn drop(&mut self) {
        self.contents.zeroize_entries();
        // `master`: wrapped in Zeroizing<> so it scrubs itself.
        // `salt`: not secret (stored cleartext in the file header)
        // so doesn't need scrubbing.
    }
}

pub struct Vault {
    root: PathBuf,
    state: Option<UnlockedState>,
}

impl Vault {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            state: None,
        }
    }

    /// Create a fresh vault at `root` keyed off the supplied
    /// `export_key` bytes and `suite_version`.
    ///
    /// The `export_key` is OPAQUE-supplied per-tenant key material.
    /// We HKDF-derive the
    /// vault master key from `(export_key, salt, suite_version)`
    /// once at create time and cache it on the returned, unlocked
    /// `Vault` so subsequent writes don't have to round-trip back
    /// to the lease.
    pub fn create(
        root: impl Into<PathBuf>,
        export_key: &[u8],
        suite_version: u8,
    ) -> Result<Self, VaultError> {
        let root = root.into();

        if root.exists() {
            let is_empty = fs::read_dir(&root)?.next().is_none();
            if !is_empty {
                let message = format!(
                    "{PREFIX} create: vault already initialised root={}",
                    root.display()
                );
                warn!("{message}");
                return Err(VaultError::AlreadyInitialized(root));
            }
        }

        ensure_vault_root(&root)?;

        let now = now_unix();
        let contents = VaultContents::new(now);

        let salt = kdf::gen_salt();
        let master = kdf::derive_master_key(export_key, &salt, suite_version)?;
        let data = seal_with_master(&contents, &master, &salt, suite_version)?;
        atomic_write_private(&root.join("vault.botwork"), &data)?;

        // Establish an explicit gen-0 baseline on disk so every
        // reader/writer shares an unambiguous starting generation
        // rather than deriving it from the absent-file fallback.
        // Without this, two handles opened between `create` and the
        // first `persist` would both read generation 0 and both pass
        // the CAS check on their first write.
        init_generation(&root)?;

        let message = format!(
            "{PREFIX} created vault root={} suite_version={suite_version}",
            root.display()
        );
        info!("{message}");

        Ok(Self {
            root,
            state: Some(UnlockedState {
                contents,
                master,
                salt,
                suite_version,
                generation: 0,
            }),
        })
    }

    fn vault_file(&self) -> PathBuf {
        self.root.join("vault.botwork")
    }

    /// Unlock the vault with the OPAQUE-supplied `export_key` and
    /// `suite_version` for this tenant. Returns an opaque
    /// [`UnlockedMasterKey`] holder so the caller can hand it back
    /// to [`Self::decrypt_entry`] for per-entry decrypts without
    /// the master key leaking through `Debug` / `Clone` /
    /// `serialize()`.
    ///
    /// The vault's full `VaultContents` is still loaded into
    /// process memory by this call so subsequent CLI subcommands
    /// (`put-secret`, `list`, etc.) can mutate metadata cheaply;
    /// the per-entry DEK indirection means the loaded payload
    /// holds wrapped DEKs + per-entry ciphertexts, NOT plaintext
    /// values. See `vault/README.md` for the on-disk shape.
    pub fn unlock_master(
        &mut self,
        export_key: &[u8],
        suite_version: u8,
    ) -> Result<UnlockedMasterKey, VaultError> {
        let path = self.vault_file();
        debug!("{PREFIX} unlock root={}", self.root.display());
        if !path.exists() {
            let message = format!(
                "{PREFIX} unlock: not initialised root={}",
                self.root.display()
            );
            warn!("{message}");
            return Err(VaultError::NotInitialized(self.root.clone()));
        }
        let data = fs::read(&path)?;

        // Peek the salt + suite_version out of the header before we
        // attempt derivation so an unsupported-version file fires
        // [`VaultError::UnsupportedVersion`] *before* spending HKDF
        // cycles on bytes the derivation can't open.
        if data.len() < HEADER_FULL_LEN + TAG_LEN {
            let message = format!(
                "{PREFIX} unlock: file too short root={}",
                self.root.display()
            );
            warn!("{message}");
            return Err(VaultError::Integrity("file too short".to_string()));
        }
        if &data[..4] != MAGIC {
            let message = format!(
                "{PREFIX} unlock: bad magic bytes root={}",
                self.root.display()
            );
            warn!("{message}");
            return Err(VaultError::Integrity("bad magic bytes".to_string()));
        }
        let format_version = data[4];
        if format_version != FORMAT_VERSION {
            let message = format!(
                "{PREFIX} unlock: unsupported format version={format_version} root={}",
                self.root.display()
            );
            warn!("{message}");
            return Err(unsupported_version_error(&path));
        }
        let header_suite = data[5];
        if header_suite != suite_version {
            // The header's suite_version doesn't match the bytes the
            // caller supplied — typically because the OPAQUE suite
            // rotated and the supplied export_key is for a different
            // suite than the file was sealed under. Surface as an
            // unsupported-version error so callers fail before an
            // opaque AEAD authentication error.
            let message = format!(
                "{PREFIX} unlock: suite_version mismatch header={header_suite} \
                 caller={suite_version} root={}",
                self.root.display()
            );
            warn!("{message}");
            return Err(unsupported_version_error(&path));
        }
        // The minimum file-length guard above guarantees the full salt bytes
        // are present, so indexing the fixed range is structurally infallible.
        let salt = std::array::from_fn(|idx| data[6 + idx]);

        let master = kdf::derive_master_key(export_key, &salt, suite_version)?;
        let opened = open_contents(&path, &data, &master)?;
        let master_copy = *master;
        let generation = peek_generation(&self.root)?;
        let entry_count = opened.contents.entries.len();
        self.state = Some(UnlockedState {
            contents: opened.contents,
            master,
            salt: opened.salt,
            suite_version: opened.suite_version,
            generation,
        });
        let root_display = self.root.display().to_string();
        info!("{PREFIX} unlocked vault root={root_display} suite_version={suite_version} entries={entry_count}");
        Ok(UnlockedMasterKey::from_bytes(master_copy))
    }

    /// Convenience wrapper around [`Self::unlock_master`] that
    /// discards the returned [`UnlockedMasterKey`]. Subsequent
    /// calls to `put_secret` / `list_secrets` / etc. work off the
    /// cached state inside `Vault`.
    pub fn unlock(&mut self, export_key: &[u8], suite_version: u8) -> Result<(), VaultError> {
        let _ = self.unlock_master(export_key, suite_version)?;
        Ok(())
    }

    /// Re-open the vault file using an already-derived master key
    /// (typically one the auth-broker's cache derived once at
    /// `/auth/check` time and is now handing back in for a hot-path
    /// `/secrets/fetch`).
    ///
    /// Functionally equivalent to `unlock_master` except it skips
    /// the HKDF step — same envelope authentication, same loaded
    /// state. Use this when the caller already holds an
    /// [`UnlockedMasterKey`] and doesn't want to round-trip back to
    /// the OPAQUE export_key.
    pub fn open_with_master(&mut self, master: &UnlockedMasterKey) -> Result<(), VaultError> {
        let path = self.vault_file();
        debug!("{PREFIX} open_with_master root={}", self.root.display());
        if !path.exists() {
            let message = format!(
                "{PREFIX} open_with_master: not initialised root={}",
                self.root.display()
            );
            warn!("{message}");
            return Err(VaultError::NotInitialized(self.root.clone()));
        }
        let data = fs::read(&path)?;

        // Peek the header so an unsupported version surfaces
        // before we dive into the AEAD.
        if data.len() < HEADER_FULL_LEN + TAG_LEN {
            let message = format!(
                "{PREFIX} open_with_master: file too short root={}",
                self.root.display()
            );
            warn!("{message}");
            return Err(VaultError::Integrity("file too short".to_string()));
        }
        if &data[..4] != MAGIC {
            let message = format!(
                "{PREFIX} open_with_master: bad magic bytes root={}",
                self.root.display()
            );
            warn!("{message}");
            return Err(VaultError::Integrity("bad magic bytes".to_string()));
        }
        let format_version = data[4];
        if format_version != FORMAT_VERSION {
            let message = format!(
                "{PREFIX} open_with_master: unsupported format version={format_version} root={}",
                self.root.display()
            );
            warn!("{message}");
            return Err(unsupported_version_error(&path));
        }

        let opened = open_contents(&path, &data, master.as_array())?;
        // Mirror the master into the state so subsequent
        // write-path calls (`put_secret`, `delete_secret`, …) can
        // reseal without taking another `&UnlockedMasterKey` from
        // the caller.
        let mut bytes = [0u8; KEY_LEN];
        bytes.copy_from_slice(master.as_array());
        let generation = peek_generation(&self.root)?;
        let entry_count = opened.contents.entries.len();
        self.state = Some(UnlockedState {
            contents: opened.contents,
            master: Zeroizing::new(bytes),
            salt: opened.salt,
            suite_version: opened.suite_version,
            generation,
        });
        let message = format!(
            "{PREFIX} opened vault root={} entries={entry_count}",
            self.root.display()
        );
        debug!("{message}");
        Ok(())
    }

    pub fn lock(&mut self) {
        self.state = None;
    }

    pub fn is_unlocked(&self) -> bool {
        self.state.is_some()
    }

    fn persist(&mut self) -> Result<(), VaultError> {
        let s = self.state.as_ref().ok_or(VaultError::Locked)?;
        let expected_gen = s.generation;

        // Acquire the exclusive flock on the sidecar before touching
        // the vault file. This serialises concurrent writers (both
        // cross-thread within this process and cross-process).
        let (lock, disk_gen) = VaultLock::acquire(&self.root)?;
        if disk_gen != expected_gen {
            // Another writer has persisted since we loaded state.
            let message = format!(
                "{PREFIX} persist: write conflict root={} expected={expected_gen} found={disk_gen}",
                self.root.display()
            );
            warn!("{message}");
            return Err(VaultError::Conflict {
                expected: expected_gen,
                found: disk_gen,
            });
        }

        let data = seal_with_master(&s.contents, &s.master, &s.salt, s.suite_version)?;
        atomic_write_private(&self.vault_file(), &data)?;

        // Bump the on-disk counter and release the lock.
        lock.bump(disk_gen)?;

        // Keep the in-memory generation in sync so the next persist
        // on this instance sees the right expected value.
        if let Some(s) = self.state.as_mut() {
            s.generation = disk_gen.wrapping_add(1);
        }
        Ok(())
    }

    /// Insert / replace a secret. Generates a fresh per-entry DEK,
    /// seals the value under it, wraps the DEK under the master
    /// key, and persists the outer file. The per-entry DEK is
    /// rotated on every overwrite.
    pub fn put_secret(&mut self, key: SecretKey, entry: SecretEntry) -> Result<(), VaultError> {
        validate_service(&key.service)?;
        validate_name(&key.name)?;
        let now = now_unix();
        let now_utc = Utc::now();
        let envelope = {
            let s = self.state.as_ref().ok_or(VaultError::Locked)?;
            let existing = s.contents.entries.get(&key);
            let is_overwrite = existing.is_some();
            let created_at = existing.map(|e| e.meta.created_at).unwrap_or(now);
            let created_at_utc = existing.map(|e| e.meta.created_at_utc).unwrap_or(now_utc);
            let meta = EntryMeta {
                kind: entry.kind,
                created_at,
                updated_at: now,
                last_used_at: entry.last_used_at,
                tags: entry.tags.clone(),
                allowed_consumers: entry.allowed_consumers.clone(),
                created_at_utc,
                rotated_at_utc: now_utc,
            };
            debug!(
                "{PREFIX} put_secret service={} name={} kind={} overwrite={is_overwrite}",
                key.service, key.name, entry.kind
            );
            seal_entry(&s.master, meta, &entry.value)?
        };
        {
            let s = self.state.as_mut().ok_or(VaultError::Locked)?;
            s.contents.entries.insert(key, envelope);
            s.contents.updated_at = now;
        }
        self.persist()
    }

    /// Per-entry decrypt of the value bytes for `key` under the
    /// supplied master.
    ///
    /// The returned buffer is `Zeroizing` so it wipes itself when
    /// the caller drops it. This is the load-bearing API the
    /// per-secret-unlock property hangs off — a caller that fetches
    /// one secret recovers exactly that one secret's plaintext, not
    /// the whole payload.
    pub fn decrypt_entry(
        &self,
        master: &UnlockedMasterKey,
        key: &SecretKey,
    ) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        let s = self.state.as_ref().ok_or(VaultError::Locked)?;
        let envelope = s.contents.entries.get(key).ok_or_else(|| {
            debug!(
                "{PREFIX} decrypt_entry: not found service={} name={}",
                key.service, key.name
            );
            VaultError::SecretNotFound(key.service.clone(), key.name.clone())
        })?;
        debug!(
            "{PREFIX} decrypt_entry service={} name={}",
            key.service, key.name
        );
        open_entry(master.as_array(), envelope)
    }

    /// Metadata-only iterator over the unlocked vault. Does NOT
    /// trigger any per-entry decrypt. Useful for `list` /
    /// `verify`-style call sites.
    pub fn list_entries(&self) -> Result<Vec<(SecretKey, SecretMeta)>, VaultError> {
        let s = self.state.as_ref().ok_or(VaultError::Locked)?;
        let entries: Vec<_> = s
            .contents
            .entries
            .iter()
            .map(|(k, v)| (k.clone(), SecretMeta::from(&v.meta)))
            .collect();
        trace!("{PREFIX} list_entries count={}", entries.len());
        Ok(entries)
    }

    /// Convenience accessor used by the CLI. Decrypts one entry's
    /// value (using the cached master held on the unlocked state)
    /// and returns metadata + plaintext.
    ///
    /// Returns an owned [`DecryptedSecret`] whose `value` field is a
    /// fresh decrypt wrapped in `Zeroizing<Vec<u8>>`. The
    /// plaintext bytes do NOT live in the
    /// cache once the returned value is dropped — that's
    /// the per-secret-unlock property the auth-broker cache
    /// refactor depends on.
    pub fn get_secret(&self, key: &SecretKey) -> Result<DecryptedSecret, VaultError> {
        let s = self.state.as_ref().ok_or(VaultError::Locked)?;
        let envelope = s.contents.entries.get(key).ok_or_else(|| {
            debug!(
                "{PREFIX} get_secret: not found service={} name={}",
                key.service, key.name
            );
            VaultError::SecretNotFound(key.service.clone(), key.name.clone())
        })?;
        debug!(
            "{PREFIX} get_secret service={} name={}",
            key.service, key.name
        );
        let plaintext = open_entry(&s.master, envelope)?;
        Ok(DecryptedSecret {
            key: key.clone(),
            meta: SecretMeta::from(&envelope.meta),
            value: plaintext,
        })
    }

    pub fn delete_secret(&mut self, key: &SecretKey) -> Result<(), VaultError> {
        {
            let s = self.state.as_mut().ok_or(VaultError::Locked)?;
            if s.contents.entries.remove(key).is_none() {
                debug!(
                    "{PREFIX} delete_secret: not found service={} name={}",
                    key.service, key.name
                );
                return Err(VaultError::SecretNotFound(
                    key.service.clone(),
                    key.name.clone(),
                ));
            }
            s.contents.updated_at = now_unix();
        }
        debug!(
            "{PREFIX} delete_secret service={} name={}",
            key.service, key.name
        );
        self.persist()
    }

    pub fn list_secrets(&self) -> Result<Vec<(SecretKey, SecretMeta)>, VaultError> {
        self.list_entries()
    }

    /// Return `true` if a secret with this key exists in the unlocked
    /// vault (without decrypting its value). Used by the remote-write
    /// endpoint to decide whether an overwrite gate applies before the
    /// actual [`Self::put_secret`] call.
    pub fn has_secret(&self, key: &SecretKey) -> Result<bool, VaultError> {
        let s = self.state.as_ref().ok_or(VaultError::Locked)?;
        Ok(s.contents.entries.contains_key(key))
    }

    pub fn touch_last_used(&mut self, key: &SecretKey) -> Result<(), VaultError> {
        let now = now_unix();
        let now_utc = Utc::now();
        // Decrypt + reseal so the per-entry DEK rotates on
        // last-used touches. Cheaper than it sounds: the AEAD
        // round-trip on a single small value is microseconds.
        let resealed: EntryEnvelope = {
            let s = self.state.as_ref().ok_or(VaultError::Locked)?;
            let envelope =
                s.contents.entries.get(key).ok_or_else(|| {
                    VaultError::SecretNotFound(key.service.clone(), key.name.clone())
                })?;
            let plaintext = open_entry(&s.master, envelope)?;
            let meta = EntryMeta {
                last_used_at: Some(now),
                updated_at: now,
                rotated_at_utc: now_utc,
                ..envelope.meta.clone()
            };
            debug!(
                "{PREFIX} touch_last_used service={} name={}",
                key.service, key.name
            );
            seal_entry(&s.master, meta, &plaintext)?
        };
        {
            let s = self.state.as_mut().ok_or(VaultError::Locked)?;
            s.contents.entries.insert(key.clone(), resealed);
            s.contents.updated_at = now;
        }
        self.persist()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SecretKind;
    use tempfile::TempDir;

    const FAST_SUITE: u8 = 1;
    const OTHER_SUITE: u8 = 2;
    const EXPORT_KEY: &[u8; 64] =
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn now() -> i64 {
        Utc::now().timestamp()
    }

    fn make_entry(value: &[u8]) -> SecretEntry {
        SecretEntry {
            kind: SecretKind::ApiKey,
            value: value.to_vec(),
            created_at: now(),
            updated_at: now(),
            last_used_at: None,
            tags: vec!["env:test".to_string()],
            allowed_consumers: vec!["plugin".to_string()],
        }
    }

    fn make_key(name: &str) -> SecretKey {
        SecretKey {
            service: "svc".to_string(),
            name: name.to_string(),
        }
    }

    fn write_vault_bytes(root: &Path, data: &[u8]) {
        ensure_vault_root(root).unwrap();
        fs::write(root.join("vault.botwork"), data).unwrap();
    }

    fn create_and_lock(root: &Path) {
        let mut vault = Vault::create(root, EXPORT_KEY, FAST_SUITE).unwrap();
        vault.lock();
    }

    fn unlock_master_for_root(root: &Path) -> UnlockedMasterKey {
        let mut vault = Vault::new(root);
        vault.unlock_master(EXPORT_KEY, FAST_SUITE).unwrap()
    }

    fn seal_invalid_postcard_payload(path: &Path, suite_version: u8) -> Vec<u8> {
        let salt = kdf::gen_salt();
        let master = kdf::derive_master_key(EXPORT_KEY, &salt, suite_version).unwrap();
        let header = header_core(suite_version, &salt);
        let nonce = gen_nonce();
        let cipher = ChaCha20Poly1305::new(Key::from_slice(master.as_ref()));
        let mut plaintext = vec![0xff, 0xff, 0xff, 0xff];
        let tag = cipher
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), &header, &mut plaintext)
            .unwrap();

        let mut out = Vec::new();
        out.extend_from_slice(&header);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&plaintext);
        out.extend_from_slice(&tag);

        // Sanity-check that the file header still points at the path we claim
        // to be opening in the test.
        assert_eq!(path.file_name().unwrap(), "vault.botwork");
        out
    }

    #[test]
    fn create_rejects_non_empty_root() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("existing"), b"x").unwrap();

        let err = match Vault::create(&root, EXPORT_KEY, FAST_SUITE) {
            Ok(_) => panic!("expected AlreadyInitialized"),
            Err(err) => err,
        };
        assert!(
            matches!(err, VaultError::AlreadyInitialized(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn unlocked_master_debug_and_slice_hide_and_expose_expected_bytes() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        create_and_lock(&root);
        let master = unlock_master_for_root(&root);
        assert_eq!(master.as_slice().len(), KEY_LEN);
        assert_eq!(format!("{master:?}"), "UnlockedMasterKey { .. }");
    }

    #[test]
    fn open_contents_rejects_short_file() {
        let path = PathBuf::from("/tmp/short-vault.botwork");
        let err = match open_contents(&path, b"short", &[0u8; KEY_LEN]) {
            Ok(_) => panic!("expected short-file error"),
            Err(err) => err,
        };
        assert_eq!(err.to_string(), "integrity check failed: file too short");
    }

    #[test]
    fn open_contents_rejects_bad_magic_bytes() {
        let path = PathBuf::from("/tmp/bad-magic-vault.botwork");
        let err = match open_contents(&path, &[0u8; HEADER_FULL_LEN + TAG_LEN], &[0u8; KEY_LEN]) {
            Ok(_) => panic!("expected bad-magic error"),
            Err(err) => err,
        };
        assert_eq!(err.to_string(), "integrity check failed: bad magic bytes");
    }

    #[test]
    fn open_contents_rejects_unsupported_format_version() {
        let path = PathBuf::from("/tmp/bad-version-vault.botwork");
        let mut data = vec![0u8; HEADER_FULL_LEN + TAG_LEN];
        data[..4].copy_from_slice(MAGIC);
        data[4] = 9;
        let err = match open_contents(&path, &data, &[0u8; KEY_LEN]) {
            Ok(_) => panic!("expected unsupported-version error"),
            Err(err) => err,
        };
        assert!(
            matches!(err, VaultError::UnsupportedVersion { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn unlock_master_rejects_missing_vault_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        let err = Vault::new(&root)
            .unlock_master(EXPORT_KEY, FAST_SUITE)
            .unwrap_err();
        assert!(matches!(err, VaultError::NotInitialized(_)), "got {err:?}");
    }

    #[test]
    fn open_with_master_rejects_missing_vault_file() {
        let source_dir = TempDir::new().unwrap();
        let source_root = source_dir.path().join("vault");
        create_and_lock(&source_root);
        let master = unlock_master_for_root(&source_root);

        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        let err = Vault::new(&root).open_with_master(&master).unwrap_err();
        assert!(matches!(err, VaultError::NotInitialized(_)), "got {err:?}");
    }

    #[test]
    fn unlock_master_rejects_short_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        write_vault_bytes(&root, b"short");
        let err = Vault::new(&root)
            .unlock_master(EXPORT_KEY, FAST_SUITE)
            .unwrap_err();
        assert_eq!(err.to_string(), "integrity check failed: file too short");
    }

    #[test]
    fn open_with_master_rejects_short_file() {
        let source_dir = TempDir::new().unwrap();
        let source_root = source_dir.path().join("vault");
        create_and_lock(&source_root);
        let master = unlock_master_for_root(&source_root);

        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        write_vault_bytes(&root, b"short");
        let err = Vault::new(&root).open_with_master(&master).unwrap_err();
        assert_eq!(err.to_string(), "integrity check failed: file too short");
    }

    #[test]
    fn unlock_master_rejects_bad_magic_bytes() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        write_vault_bytes(&root, &[0u8; HEADER_FULL_LEN + TAG_LEN]);
        let err = Vault::new(&root)
            .unlock_master(EXPORT_KEY, FAST_SUITE)
            .unwrap_err();
        assert_eq!(err.to_string(), "integrity check failed: bad magic bytes");
    }

    #[test]
    fn open_with_master_rejects_bad_magic_bytes() {
        let source_dir = TempDir::new().unwrap();
        let source_root = source_dir.path().join("vault");
        create_and_lock(&source_root);
        let master = unlock_master_for_root(&source_root);

        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        write_vault_bytes(&root, &[0u8; HEADER_FULL_LEN + TAG_LEN]);
        let err = Vault::new(&root).open_with_master(&master).unwrap_err();
        assert_eq!(err.to_string(), "integrity check failed: bad magic bytes");
    }

    #[test]
    fn unlock_master_rejects_unsupported_format_version() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        create_and_lock(&root);
        let mut data = fs::read(root.join("vault.botwork")).unwrap();
        data[4] = 9;
        fs::write(root.join("vault.botwork"), data).unwrap();

        let err = Vault::new(&root)
            .unlock_master(EXPORT_KEY, FAST_SUITE)
            .unwrap_err();
        assert!(
            matches!(err, VaultError::UnsupportedVersion { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn open_with_master_rejects_unsupported_format_version() {
        let source_dir = TempDir::new().unwrap();
        let source_root = source_dir.path().join("vault");
        create_and_lock(&source_root);
        let master = unlock_master_for_root(&source_root);

        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        create_and_lock(&root);
        let mut data = fs::read(root.join("vault.botwork")).unwrap();
        data[4] = 9;
        fs::write(root.join("vault.botwork"), data).unwrap();

        let err = Vault::new(&root).open_with_master(&master).unwrap_err();
        assert!(
            matches!(err, VaultError::UnsupportedVersion { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn open_with_master_successfully_reuses_derived_master() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");

        let mut writer = Vault::create(&root, EXPORT_KEY, FAST_SUITE).unwrap();
        writer
            .put_secret(make_key("roundtrip"), make_entry(b"value"))
            .unwrap();
        writer.lock();

        let master = unlock_master_for_root(&root);
        let mut reopened = Vault::new(&root);
        reopened.open_with_master(&master).unwrap();
        assert!(reopened.is_unlocked());
        assert_eq!(reopened.list_entries().unwrap().len(), 1);
    }

    #[test]
    fn unlock_master_rejects_suite_version_mismatch() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        create_and_lock(&root);
        let err = Vault::new(&root)
            .unlock_master(EXPORT_KEY, OTHER_SUITE)
            .unwrap_err();
        assert!(
            matches!(err, VaultError::UnsupportedVersion { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn open_contents_rejects_invalid_postcard_payload() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("vault.botwork");
        let data = seal_invalid_postcard_payload(&path, FAST_SUITE);
        let salt = std::array::from_fn(|idx| data[6 + idx]);
        let master = kdf::derive_master_key(EXPORT_KEY, &salt, FAST_SUITE).unwrap();
        let err = match open_contents(&path, &data, &master) {
            Ok(_) => panic!("expected codec error"),
            Err(err) => err,
        };
        assert!(matches!(err, VaultError::Codec(_)), "got {err:?}");
    }

    #[test]
    fn persist_rejects_stale_generation_with_expected_and_found_values() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");

        let mut writer_a = Vault::create(&root, EXPORT_KEY, FAST_SUITE).unwrap();
        let mut writer_b = Vault::new(&root);
        writer_b.unlock(EXPORT_KEY, FAST_SUITE).unwrap();

        writer_a
            .put_secret(make_key("a"), make_entry(b"a"))
            .unwrap();

        let err = writer_b
            .put_secret(make_key("b"), make_entry(b"b"))
            .unwrap_err();
        assert!(matches!(
            err,
            VaultError::Conflict {
                expected: 0,
                found: 1
            }
        ));
    }

    #[test]
    fn secret_not_found_paths_return_structured_errors() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        create_and_lock(&root);
        let master = unlock_master_for_root(&root);

        let mut vault = Vault::new(&root);
        vault.open_with_master(&master).unwrap();
        let missing = make_key("missing");

        assert!(matches!(
            vault.decrypt_entry(&master, &missing).unwrap_err(),
            VaultError::SecretNotFound(_, _)
        ));
        match vault.get_secret(&missing) {
            Ok(_) => panic!("expected SecretNotFound"),
            Err(err) => assert!(matches!(err, VaultError::SecretNotFound(_, _))),
        }
        assert!(matches!(
            vault.delete_secret(&missing).unwrap_err(),
            VaultError::SecretNotFound(_, _)
        ));
        assert!(matches!(
            vault.touch_last_used(&missing).unwrap_err(),
            VaultError::SecretNotFound(_, _)
        ));
    }

    #[test]
    fn decrypt_entry_and_has_secret_succeed_for_existing_key() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        let mut writer = Vault::create(&root, EXPORT_KEY, FAST_SUITE).unwrap();
        let key = make_key("present");
        writer
            .put_secret(key.clone(), make_entry(b"value"))
            .unwrap();
        writer.lock();

        let master = unlock_master_for_root(&root);
        let mut vault = Vault::new(&root);
        vault.open_with_master(&master).unwrap();

        assert!(vault.has_secret(&key).unwrap());
        assert_eq!(
            vault.decrypt_entry(&master, &key).unwrap().as_slice(),
            b"value"
        );
        assert_eq!(vault.list_entries().unwrap().len(), 1);
    }

    #[test]
    fn locked_operations_fail_before_unlock() {
        let master = UnlockedMasterKey::from_master_bytes_for_test([0u8; KEY_LEN]);
        let mut vault = Vault::new(PathBuf::from("/tmp/locked-vault-for-unit-tests"));
        let key = make_key("locked");

        assert!(matches!(
            vault.list_entries().unwrap_err(),
            VaultError::Locked
        ));
        assert!(matches!(
            vault
                .put_secret(key.clone(), make_entry(b"value"))
                .unwrap_err(),
            VaultError::Locked
        ));
        assert!(matches!(
            vault.decrypt_entry(&master, &key).unwrap_err(),
            VaultError::Locked
        ));
        match vault.get_secret(&key) {
            Ok(_) => panic!("expected locked error"),
            Err(err) => assert!(matches!(err, VaultError::Locked)),
        }
        assert!(matches!(
            vault.delete_secret(&key).unwrap_err(),
            VaultError::Locked
        ));
        assert!(matches!(
            vault.has_secret(&key).unwrap_err(),
            VaultError::Locked
        ));
        assert!(matches!(
            vault.touch_last_used(&key).unwrap_err(),
            VaultError::Locked
        ));
    }
}
