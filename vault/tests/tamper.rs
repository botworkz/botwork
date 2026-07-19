use std::fs;
use std::io::Write;
use std::path::Path;

use botwork_vault::{SecretEntry, SecretKey, SecretKind, Vault, VaultError};
use tempfile::{NamedTempFile, TempDir};

const FAST_SUITE: u8 = 1;
const TEST_EXPORT_KEY: &[u8; 64] =
    b"deterministic-export-key-bytes-for-vault-tamper-tests-AAAAAAAAAA";

// On-disk layout — kept in sync with the constants in vault/src/vault.rs.
//
//   [0..4)     magic "BSVL"
//   [4]        version 4
//   [5]        suite_version
//   [6..22)    16-byte salt
//   [22..34)   12-byte nonce
//   [34..N-16) ciphertext
//   [N-16..N)  16-byte AEAD tag
const HEADER_CORE_LEN: usize = 4 + 1 + 1 + botwork_vault::kdf::SALT_LEN;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const HEADER_FULL_LEN: usize = HEADER_CORE_LEN + NONCE_LEN;

fn setup_vault() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");
    let mut vault = Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();
    let now = chrono::Utc::now().timestamp();
    vault
        .put_secret(
            SecretKey {
                service: "github".to_string(),
                name: "pat".to_string(),
            },
            SecretEntry {
                kind: SecretKind::ApiKey,
                value: b"ghp_abc123".to_vec(),
                created_at: now,
                updated_at: now,
                last_used_at: None,
                tags: vec![],
                allowed_consumers: vec![],
            },
        )
        .unwrap();
    vault.lock();
    (dir, root)
}

fn atomic_overwrite(path: &Path, data: &[u8]) {
    let parent = path.parent().unwrap();
    let mut tmp = NamedTempFile::new_in(parent).unwrap();
    tmp.write_all(data).unwrap();
    tmp.as_file().sync_all().unwrap();
    tmp.persist(path).unwrap();
}

#[test]
fn tampered_ciphertext_fails_unlock() {
    let (_dir, root) = setup_vault();
    let path = root.join("vault.botwork");
    let mut data = fs::read(&path).unwrap();
    // Flip a byte in the ciphertext (anywhere between header_full and the tag).
    assert!(data.len() > HEADER_FULL_LEN + TAG_LEN);
    let mid = HEADER_FULL_LEN + (data.len() - HEADER_FULL_LEN - TAG_LEN) / 2;
    data[mid] ^= 0x01;
    atomic_overwrite(&path, &data);

    let mut reopened = Vault::new(&root);
    let err = reopened.unlock(TEST_EXPORT_KEY, FAST_SUITE).unwrap_err();
    assert!(matches!(err, VaultError::Auth | VaultError::Integrity(_)));
}

#[test]
fn tampered_tag_fails_unlock() {
    let (_dir, root) = setup_vault();
    let path = root.join("vault.botwork");
    let mut data = fs::read(&path).unwrap();
    let tag_start = data.len() - TAG_LEN;
    data[tag_start] ^= 0x01;
    atomic_overwrite(&path, &data);

    let mut reopened = Vault::new(&root);
    let err = reopened.unlock(TEST_EXPORT_KEY, FAST_SUITE).unwrap_err();
    assert!(matches!(err, VaultError::Auth | VaultError::Integrity(_)));
}

#[test]
fn tampered_nonce_fails_unlock() {
    let (_dir, root) = setup_vault();
    let path = root.join("vault.botwork");
    let mut data = fs::read(&path).unwrap();
    // The nonce sits between header_core and the ciphertext. It's NOT
    // covered by AAD, but it IS the AEAD nonce input — flipping it makes
    // the tag check fail because the cipher reproduces a different
    // keystream and a different MAC.
    data[HEADER_CORE_LEN] ^= 0x01;
    atomic_overwrite(&path, &data);

    let mut reopened = Vault::new(&root);
    let err = reopened.unlock(TEST_EXPORT_KEY, FAST_SUITE).unwrap_err();
    assert!(matches!(err, VaultError::Auth | VaultError::Integrity(_)));
}

#[test]
fn truncated_file_fails_with_file_too_short() {
    let (_dir, root) = setup_vault();
    let path = root.join("vault.botwork");
    let data = fs::read(&path).unwrap();
    // Truncate below the minimum legal v4 length (header + nonce + tag).
    atomic_overwrite(&path, &data[..HEADER_FULL_LEN + TAG_LEN - 1]);

    let mut reopened = Vault::new(&root);
    let err = reopened.unlock(TEST_EXPORT_KEY, FAST_SUITE).unwrap_err();
    match err {
        VaultError::Integrity(msg) => assert!(msg.contains("file too short"), "got: {msg}"),
        other => panic!("expected integrity error, got: {other:?}"),
    }
}

#[test]
fn bad_magic_fails_with_bad_magic_bytes() {
    let (_dir, root) = setup_vault();
    let path = root.join("vault.botwork");
    let mut data = fs::read(&path).unwrap();
    data[0] ^= 0x01;
    atomic_overwrite(&path, &data);

    let mut reopened = Vault::new(&root);
    let err = reopened.unlock(TEST_EXPORT_KEY, FAST_SUITE).unwrap_err();
    match err {
        VaultError::Integrity(msg) => assert!(msg.contains("bad magic bytes")),
        other => panic!("expected integrity error, got: {other:?}"),
    }
}

#[test]
fn bad_version_fails_with_unsupported_format() {
    let (_dir, root) = setup_vault();
    let path = root.join("vault.botwork");
    let mut data = fs::read(&path).unwrap();
    data[4] = 0xFF;
    atomic_overwrite(&path, &data);

    let mut reopened = Vault::new(&root);
    let err = reopened.unlock(TEST_EXPORT_KEY, FAST_SUITE).unwrap_err();
    match err {
        VaultError::UnsupportedVersion { path: p } => assert_eq!(p, path),
        other => panic!("expected UnsupportedVersion, got: {other:?}"),
    }
}

#[test]
fn non_v4_format_byte_is_rejected() {
    let (_dir, root) = setup_vault();
    let path = root.join("vault.botwork");
    let mut data = fs::read(&path).unwrap();
    data[4] = 0x03; // Non-v4
    atomic_overwrite(&path, &data);

    let mut reopened = Vault::new(&root);
    let err = reopened.unlock(TEST_EXPORT_KEY, FAST_SUITE).unwrap_err();
    match err {
        VaultError::UnsupportedVersion { path: p } => {
            assert_eq!(p, path, "error must carry the originating vault path");
        }
        other => panic!("expected UnsupportedVersion, got: {other:?}"),
    }
}

#[test]
fn another_non_v4_format_byte_is_rejected() {
    let (_dir, root) = setup_vault();
    let path = root.join("vault.botwork");
    let mut data = fs::read(&path).unwrap();
    data[4] = 0x02;
    atomic_overwrite(&path, &data);

    let mut reopened = Vault::new(&root);
    let err = reopened.unlock(TEST_EXPORT_KEY, FAST_SUITE).unwrap_err();
    assert!(matches!(err, VaultError::UnsupportedVersion { .. }));
}

#[test]
fn tampered_suite_byte_fails_unlock() {
    // The suite byte is part of header_core (AAD), so a flip
    // would invalidate the AEAD tag *and* mismatch the caller's
    // supplied suite_version. Either path surfaces as a refused
    // open; both are acceptable here. The dedicated "supplied
    // suite mismatches header suite" arm is exercised below in
    // `caller_suite_mismatch_returns_unsupported_version`.
    let (_dir, root) = setup_vault();
    let path = root.join("vault.botwork");
    let mut data = fs::read(&path).unwrap();
    data[5] ^= 0x01;
    atomic_overwrite(&path, &data);

    let mut reopened = Vault::new(&root);
    let err = reopened.unlock(TEST_EXPORT_KEY, FAST_SUITE).unwrap_err();
    assert!(
        matches!(
            err,
            VaultError::Auth | VaultError::UnsupportedVersion { .. }
        ),
        "got: {err:?}"
    );
}

#[test]
fn caller_suite_mismatch_returns_unsupported_version() {
    // Untampered vault file, but the caller hands us a suite_version
    // that doesn't match the byte in the header.
    let (_dir, root) = setup_vault();
    let path = root.join("vault.botwork");
    let mut reopened = Vault::new(&root);
    let err = reopened
        .unlock(TEST_EXPORT_KEY, FAST_SUITE.wrapping_add(1))
        .unwrap_err();
    match err {
        VaultError::UnsupportedVersion { path: p } => assert_eq!(p, path),
        other => panic!("expected UnsupportedVersion, got: {other:?}"),
    }
}

#[test]
fn tampered_header_salt_fails_unlock() {
    let (_dir, root) = setup_vault();
    let path = root.join("vault.botwork");
    let mut data = fs::read(&path).unwrap();
    // First byte of salt sits at byte 6.
    let salt_start = 4 + 1 + 1;
    data[salt_start] ^= 0x01;
    atomic_overwrite(&path, &data);

    let mut reopened = Vault::new(&root);
    let err = reopened.unlock(TEST_EXPORT_KEY, FAST_SUITE).unwrap_err();
    assert!(matches!(err, VaultError::Auth | VaultError::Integrity(_)));
}

#[test]
fn aead_auth_failure_returns_vault_error_auth() {
    // Flip a byte strictly inside the ciphertext (bytes [HEADER_FULL_LEN, data.len()-TAG_LEN))
    // while leaving magic, format version, suite, salt, and nonce untampered.
    // This exercises the AEAD authentication failure branch in `open_contents`
    // and asserts it surfaces as `VaultError::Auth` (not `Integrity`).
    let (_dir, root) = setup_vault();
    let path = root.join("vault.botwork");
    let mut data = fs::read(&path).unwrap();
    assert!(
        data.len() > HEADER_FULL_LEN + TAG_LEN,
        "vault file too small to have a ciphertext body"
    );
    // Flip the very first ciphertext byte (byte index HEADER_FULL_LEN).
    data[HEADER_FULL_LEN] ^= 0xFF;
    atomic_overwrite(&path, &data);

    let mut reopened = Vault::new(&root);
    let err = reopened.unlock(TEST_EXPORT_KEY, FAST_SUITE).unwrap_err();
    assert!(
        matches!(err, VaultError::Auth),
        "tampered ciphertext must yield VaultError::Auth, got: {err:?}"
    );
}
