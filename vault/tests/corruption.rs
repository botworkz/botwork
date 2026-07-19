/// Property / corruption tests for the v4 vault decode path.
///
/// These tests complement the example-based tamper tests in `tamper.rs`
/// with corpus-driven, randomised corruption that is runnable under
/// `cargo test` without the nightly fuzz toolchain.  Every test asserts
/// that the open/unlock path returns a *structured* [`VaultError`]
/// variant on malformed input rather than panicking.
///
/// Run the suite:
///
/// ```text
/// BOTWORK_VAULT_FAST_KDF=1 cargo test -p botwork-vault --test corruption
/// ```
use std::fs;

use botwork_vault::{SecretEntry, SecretKey, SecretKind, Vault, VaultError};
use proptest::prelude::*;
use tempfile::TempDir;

const FAST_SUITE: u8 = 1;
const TEST_EXPORT_KEY: &[u8; 64] =
    b"deterministic-export-key-bytes-for-corruption-proptest-AAAAAAAAA";

// v4 on-disk layout constants (kept in sync with vault/src/vault.rs).
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const HEADER_CORE_LEN: usize = 4 + 1 + 1 + SALT_LEN; // 22
const HEADER_FULL_LEN: usize = HEADER_CORE_LEN + NONCE_LEN; // 34
const MIN_FILE_LEN: usize = HEADER_FULL_LEN + TAG_LEN; // 50

/// Create a valid v4 vault file with one secret and return its raw bytes.
fn valid_vault_bytes() -> (TempDir, Vec<u8>) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");
    let now = chrono::Utc::now().timestamp();
    let mut vault = Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();
    vault
        .put_secret(
            SecretKey {
                service: "proptest".to_string(),
                name: "seed".to_string(),
            },
            SecretEntry {
                kind: SecretKind::ApiKey,
                value: b"test-secret-value".to_vec(),
                created_at: now,
                updated_at: now,
                last_used_at: None,
                tags: vec!["env:test".to_string()],
                allowed_consumers: vec![],
            },
        )
        .unwrap();
    vault.lock();
    let path = root.join("vault.botwork");
    let bytes = fs::read(&path).unwrap();
    (dir, bytes)
}

/// Overwrite the vault file at `<root>/vault.botwork` with `data` and
/// attempt an unlock.  Returns an `Err(VaultError)` on failure, `Ok(())`
/// if the file happens to be valid (e.g. a zero-XOR flip is a no-op).
/// Never panics.
fn try_unlock_with_bytes(data: &[u8]) -> Result<(), VaultError> {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    let vault_path = root.join("vault.botwork");
    fs::write(&vault_path, data).unwrap();

    Vault::new(&root).unlock(TEST_EXPORT_KEY, FAST_SUITE)
}

// ---------------------------------------------------------------------------
// Truncation
// ---------------------------------------------------------------------------

proptest! {
    /// Truncating a valid vault to any length shorter than the minimum
    /// legal v4 size must never panic; it must surface as a
    /// structured error.
    #[test]
    fn truncated_vault_never_panics(
        // Truncate to anywhere from 0 to MIN_FILE_LEN - 1.
        truncate_to in 0usize..MIN_FILE_LEN,
    ) {
        let (_dir, bytes) = valid_vault_bytes();
        let truncated = &bytes[..truncate_to];
        let err = try_unlock_with_bytes(truncated).unwrap_err();
        prop_assert!(
            matches!(err, VaultError::Integrity(_) | VaultError::UnsupportedVersion { .. }),
            "unexpected error variant on truncated input: {err:?}",
        );
    }
}

// ---------------------------------------------------------------------------
// Single-byte flip across the whole file
// ---------------------------------------------------------------------------

proptest! {
    /// Flipping any single byte in the valid vault must not panic.
    /// If the open path returns an error it must be a structured
    /// error variant (not some unexpected type).  A zero-XOR flip
    /// is a no-op so `Ok(())` is also a valid outcome.
    #[test]
    fn single_byte_flip_never_panics(
        flip_idx in 0usize..1024usize,
        flip_val in 0u8..=255u8,
    ) {
        let (_dir, mut bytes) = valid_vault_bytes();
        // Constrain the flip index to be within the actual file.
        let idx = flip_idx % bytes.len();
        bytes[idx] ^= flip_val;
        match try_unlock_with_bytes(&bytes) {
            Ok(()) => { /* no-op flip or collision — acceptable */ }
            Err(err) => {
                prop_assert!(
                    matches!(
                        err,
                        VaultError::Auth
                            | VaultError::Integrity(_)
                            | VaultError::UnsupportedVersion { .. }
                            | VaultError::Codec(_)
                    ),
                    "unexpected error variant on single-byte-flipped input: {err:?}",
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Region corruption
// ---------------------------------------------------------------------------

proptest! {
    /// Overwriting a contiguous region of the vault file with arbitrary
    /// bytes must not panic; every open attempt must return either
    /// `Ok(())` (rare) or a structured error.
    #[test]
    fn region_corruption_never_panics(
        region_start in 0usize..512usize,
        region_len  in 1usize..64usize,
        fill_byte   in 0u8..=255u8,
    ) {
        let (_dir, mut bytes) = valid_vault_bytes();
        let start = region_start.min(bytes.len().saturating_sub(1));
        let end   = (start + region_len).min(bytes.len());
        for b in &mut bytes[start..end] {
            *b = fill_byte;
        }
        match try_unlock_with_bytes(&bytes) {
            Ok(()) => { /* acceptable if the corruption happened to keep the vault valid */ }
            Err(err) => {
                prop_assert!(
                    matches!(
                        err,
                        VaultError::Auth
                            | VaultError::Integrity(_)
                            | VaultError::UnsupportedVersion { .. }
                            | VaultError::Codec(_)
                    ),
                    "unexpected error variant on region-corrupted input: {err:?}",
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Arbitrary byte-sequence inputs
// ---------------------------------------------------------------------------

proptest! {
    /// Feeding an arbitrary byte sequence (not derived from a valid vault)
    /// as the vault file must never panic.
    #[test]
    fn arbitrary_bytes_never_panic(data in proptest::collection::vec(0u8..=255, 0..512)) {
        match try_unlock_with_bytes(&data) {
            Ok(()) => { /* extremely unlikely but not impossible for a long enough input */ }
            Err(err) => {
                prop_assert!(
                    matches!(
                        err,
                        VaultError::Auth
                            | VaultError::Integrity(_)
                            | VaultError::UnsupportedVersion { .. }
                            | VaultError::Codec(_)
                            | VaultError::Io(_)
                    ),
                    "unexpected error variant on arbitrary input: {err:?}",
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Payload extension (appended bytes)
// ---------------------------------------------------------------------------

proptest! {
    /// Appending extra bytes to a valid vault file must not panic.
    #[test]
    fn appended_bytes_never_panic(
        extra in proptest::collection::vec(0u8..=255, 1..64),
    ) {
        let (_dir, mut bytes) = valid_vault_bytes();
        bytes.extend_from_slice(&extra);
        match try_unlock_with_bytes(&bytes) {
            Ok(()) => { /* no panic — acceptable */ }
            Err(err) => {
                prop_assert!(
                    matches!(
                        err,
                        VaultError::Auth
                            | VaultError::Integrity(_)
                            | VaultError::UnsupportedVersion { .. }
                            | VaultError::Codec(_)
                    ),
                    "unexpected error variant on extended input: {err:?}",
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-entry envelope corruption
// ---------------------------------------------------------------------------

proptest! {
    /// Feeding arbitrary bytes to the per-entry AEAD open path must
    /// return a structured error.
    #[test]
    fn arbitrary_entry_envelope_bytes_never_panic(
        wrapped_dek in proptest::collection::vec(0u8..=255, 0..128),
        ciphertext  in proptest::collection::vec(0u8..=255, 0..128),
        nonce_bytes in proptest::collection::vec(0u8..=255, 0..24),
    ) {
        use botwork_vault::contents::{open_entry, EntryEnvelope, EntryMeta, SecretKind};
        use chrono::Utc;

        // Build a syntactically plausible EntryEnvelope from fuzzed fields.
        // The nonce must be exactly 12 bytes; pad or truncate.
        let mut nonce = [0u8; 12];
        let copy_len = nonce_bytes.len().min(12);
        nonce[..copy_len].copy_from_slice(&nonce_bytes[..copy_len]);

        let now_utc = Utc::now();
        let envelope = EntryEnvelope {
            wrapped_dek,
            ciphertext,
            nonce,
            version: 1,
            meta: EntryMeta {
                kind: SecretKind::Opaque,
                created_at: 0,
                updated_at: 0,
                last_used_at: None,
                tags: vec![],
                allowed_consumers: vec![],
                created_at_utc: now_utc,
                rotated_at_utc: now_utc,
            },
        };

        let master = [0u8; 32];
        let result = open_entry(&master, &envelope);
        prop_assert!(
            result.is_err(),
            "open_entry on arbitrary envelope must return an error",
        );
        if let Err(ref err) = result {
            prop_assert!(
                matches!(err, VaultError::Auth | VaultError::Integrity(_)),
                "unexpected error variant from open_entry: {err:?}",
            );
        }
    }
}
