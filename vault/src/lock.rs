//! File-level advisory locking and generation-counter CAS for the vault.
//!
//! # How it works
//!
//! A sidecar file — `vault.botwork.gen` — lives next to the sealed
//! `vault.botwork`. It holds a single little-endian `u64` that is
//! incremented on every successful persist. The counter is called the
//! *generation*.
//!
//! ## Read path
//!
//! [`peek_generation`] reads the counter without acquiring any lock.
//! It is called by [`Vault::unlock_master`] and
//! [`Vault::open_with_master`] to record the generation at the time
//! the vault contents were loaded into memory. The recorded value is
//! stored on [`UnlockedState`] as its *expected* generation.
//!
//! ## Write path (persist)
//!
//! 1. [`VaultLock::acquire`] opens the `.gen` file and acquires an
//!    **exclusive** `flock` on it. This serialises all writers — both
//!    concurrent threads within a single process and concurrent
//!    processes on the same host.
//! 2. The lock holder re-reads the generation from the `.gen` file.
//! 3. If the on-disk generation differs from the expected generation
//!    stored at unlock time, a [`VaultError::Conflict`] is returned
//!    before any write happens.
//! 4. If the generations match the vault file is written by the caller
//!    ([`atomic_write_private`]), then [`VaultLock::bump`] increments
//!    the counter and releases the lock.
//!
//! The `flock` serialises concurrent writes; the generation CAS
//! detects stale-read conflicts (readers that loaded state before a
//! previous writer finished).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::VaultError;

/// Name of the generation-counter sidecar file.
const GEN_FILE: &str = "vault.botwork.gen";

// ── helpers ───────────────────────────────────────────────────────────

fn gen_path(vault_root: &Path) -> std::path::PathBuf {
    vault_root.join(GEN_FILE)
}

fn read_gen_from(file: &mut File) -> Result<u64, VaultError> {
    file.seek(SeekFrom::Start(0))?;
    let mut buf = [0u8; 8];
    let n = file.read(&mut buf)?;
    match n {
        0 => Ok(0), // new / empty file: generation starts at 0
        8 => Ok(u64::from_le_bytes(buf)),
        _ => Err(VaultError::Integrity(
            "vault generation file is truncated".to_string(),
        )),
    }
}

/// Write `value` as the 8-byte little-endian generation counter to an
/// already-open, positioned-at-zero sidecar file. Truncates the file
/// to exactly 8 bytes first so a shorter/longer prior write can never
/// leave trailing bytes that [`read_gen_from`] would reject.
fn write_gen_to(file: &mut File, value: u64) -> Result<(), VaultError> {
    file.set_len(8)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&value.to_le_bytes())?;
    file.sync_all()?;
    Ok(())
}

// ── public API ────────────────────────────────────────────────────────

/// Read the current generation counter without holding any lock.
///
/// Called at unlock time to record the expected generation. The file
/// not existing is treated as generation 0 (vault was created without
/// CAS support or has not been written to yet).
pub fn peek_generation(vault_root: &Path) -> Result<u64, VaultError> {
    let path = gen_path(vault_root);
    match File::open(&path) {
        Ok(mut f) => read_gen_from(&mut f),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(VaultError::Io(e)),
    }
}

/// Initialise the generation-counter sidecar to an explicit `0`.
///
/// Called by [`Vault::create`] so every vault has an unambiguous
/// on-disk generation baseline from the moment it exists, rather than
/// relying on the "absent file == generation 0" fallback. This closes
/// the window where a handle opened between `create` and the first
/// `persist` would derive its baseline from an absent file while a
/// concurrent writer derived the same value independently.
///
/// The file is created with `0600` where the platform honours the
/// mode on `OpenOptions`; on other platforms the surrounding
/// `ensure_vault_root` `0700` directory already restricts access.
pub fn init_generation(vault_root: &Path) -> Result<(), VaultError> {
    let mut opts = OpenOptions::new();
    opts.create(true).write(true).read(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(gen_path(vault_root))?;
    file.lock().map_err(VaultError::Io)?;
    write_gen_to(&mut file, 0)?;
    file.unlock().map_err(VaultError::Io)?;
    Ok(())
}

/// An exclusive advisory lock held on the vault's generation-counter
/// sidecar file.
///
/// Acquire with [`VaultLock::acquire`]. After a successful vault file
/// write, call [`VaultLock::bump`] to increment the counter and
/// release the lock. Dropping without calling `bump` releases the
/// lock without modifying the counter (safe — the outer vault file
/// was not written in that case).
pub struct VaultLock {
    file: File,
}

impl VaultLock {
    /// Open the generation-counter sidecar file (creating it if
    /// absent), acquire an exclusive advisory lock, and return the
    /// current on-disk generation.
    ///
    /// The lock is held until [`Self::bump`] is called or the value
    /// is dropped.
    pub fn acquire(vault_root: &Path) -> Result<(Self, u64), VaultError> {
        let mut opts = OpenOptions::new();
        opts.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(gen_path(vault_root))?;
        file.lock().map_err(VaultError::Io)?;
        let gen = read_gen_from(&mut file)?;
        Ok((Self { file }, gen))
    }

    /// Write `gen + 1` back to the sidecar file and release the lock.
    ///
    /// Must be called exactly once after the vault file has been
    /// written successfully.
    ///
    /// Uses a checked increment: a `u64` generation will not
    /// realistically overflow, but a silent wrap back to `0` would
    /// manufacture a false CAS match on a security-sensitive counter,
    /// so overflow is surfaced as a structured error instead.
    pub fn bump(mut self, gen: u64) -> Result<(), VaultError> {
        let next = gen.checked_add(1).ok_or_else(|| {
            VaultError::Integrity("vault generation counter overflow".to_string())
        })?;
        write_gen_to(&mut self.file, next)?;
        self.file.unlock().map_err(VaultError::Io)?;
        Ok(())
    }
}

impl Drop for VaultLock {
    fn drop(&mut self) {
        // Best-effort unlock. If `bump` was already called this is a
        // no-op on most platforms. Ignore errors here — we cannot
        // surface them from Drop.
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_root() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        std::fs::create_dir_all(&root).unwrap();
        (dir, root)
    }

    #[test]
    fn peek_generation_missing_sidecar_is_zero() {
        let (_dir, root) = make_root();
        assert_eq!(peek_generation(&root).unwrap(), 0);
    }

    #[test]
    fn init_generation_writes_zero_and_bump_increments() {
        let (_dir, root) = make_root();
        init_generation(&root).unwrap();
        assert_eq!(peek_generation(&root).unwrap(), 0);

        let (lock, generation) = VaultLock::acquire(&root).unwrap();
        assert_eq!(generation, 0);
        lock.bump(generation).unwrap();
        assert_eq!(peek_generation(&root).unwrap(), 1);
    }

    #[test]
    fn peek_generation_rejects_truncated_sidecar() {
        let (_dir, root) = make_root();
        std::fs::write(root.join(GEN_FILE), [1, 2, 3]).unwrap();
        let err = peek_generation(&root).unwrap_err();
        assert!(matches!(err, VaultError::Integrity(_)), "got {err:?}");
    }

    #[test]
    fn peek_generation_surfaces_non_not_found_io_errors() {
        let dir = TempDir::new().unwrap();
        let root_as_file = dir.path().join("root-file");
        std::fs::write(&root_as_file, b"not a directory").unwrap();
        let err = peek_generation(&root_as_file).unwrap_err();
        assert!(matches!(err, VaultError::Io(_)), "got {err:?}");
    }

    #[test]
    fn init_generation_fails_when_sidecar_path_is_directory() {
        let (_dir, root) = make_root();
        std::fs::create_dir(root.join(GEN_FILE)).unwrap();
        let err = init_generation(&root).unwrap_err();
        assert!(matches!(err, VaultError::Io(_)), "got {err:?}");
    }

    #[test]
    fn bump_rejects_generation_overflow() {
        let (_dir, root) = make_root();
        let (lock, _) = VaultLock::acquire(&root).unwrap();
        let err = lock.bump(u64::MAX).unwrap_err();
        assert!(matches!(err, VaultError::Integrity(_)), "got {err:?}");
    }
}
