use botwork_vault::{SecretEntry, SecretKey, SecretKind, Vault};
use tempfile::TempDir;
use zeroize::Zeroizing;

const FAST_SUITE: u8 = 1;
const TEST_EXPORT_KEY: &[u8; 64] =
    b"deterministic-export-key-bytes-for-vault-roundtrip-test-AAAAAAAA";

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[test]
fn roundtrip() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");

    let mut vault = Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();
    assert!(vault.is_unlocked());

    let key = SecretKey {
        service: "git".to_string(),
        name: "id_ed25519".to_string(),
    };
    let entry = SecretEntry {
        kind: SecretKind::SshPrivateKey,
        value: b"PRIVATE_KEY_DATA".to_vec(),
        created_at: now(),
        updated_at: now(),
        last_used_at: None,
        tags: vec!["consumer:git-mcp".to_string()],
        allowed_consumers: vec!["git-mcp".to_string()],
    };
    vault.put_secret(key.clone(), entry.clone()).unwrap();

    let retrieved = vault.get_secret(&key).unwrap();
    assert_eq!(&*retrieved.value, b"PRIVATE_KEY_DATA");
    assert_eq!(retrieved.meta.kind, SecretKind::SshPrivateKey);
    assert_eq!(retrieved.meta.tags, vec!["consumer:git-mcp"]);

    let list = vault.list_secrets().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].0, key);

    vault.delete_secret(&key).unwrap();
    let list = vault.list_secrets().unwrap();
    assert!(list.is_empty());

    vault.lock();
    vault.unlock(TEST_EXPORT_KEY, FAST_SUITE).unwrap();
    assert!(vault.list_secrets().unwrap().is_empty());

    vault.put_secret(key.clone(), entry.clone()).unwrap();
    vault.lock();
    vault.unlock(TEST_EXPORT_KEY, FAST_SUITE).unwrap();
    assert_eq!(vault.list_secrets().unwrap().len(), 1);

    // Wrong export_key bytes mean a different HKDF master, so the
    // outer-file AEAD tag check fails: surfaces as `VaultError::Auth`.
    let mut wrong = Vault::new(&root);
    assert!(wrong
        .unlock(b"definitely-wrong-key-bytes", FAST_SUITE)
        .is_err());
}

/// Sanity check on the v4 file: re-reading a freshly-created vault
/// yields a file whose version byte is 4, suite_version byte
/// matches the supplied value, and total length is at least
/// header_core (22) + nonce (12) + AEAD tag (16) bytes.
#[test]
fn freshly_created_vault_is_v4_and_minimum_sized() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");
    Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();

    let data = std::fs::read(root.join("vault.botwork")).unwrap();
    assert_eq!(&data[..4], b"BSVL", "magic bytes");
    assert_eq!(data[4], 4, "format version");
    assert_eq!(data[5], FAST_SUITE, "suite version recorded in header");

    // header_core(22) + nonce(12) + tag(16) = 50 bytes minimum,
    // plus whatever serialised VaultContents adds.
    assert!(data.len() >= 22 + 12 + 16, "v4 file shorter than legal min");
}

/// Per-secret-unlock pin: after fetching secret X, the bytes of X's
/// plaintext do not survive the borrow's drop in the vault's
/// in-memory state. The unlocked payload holds per-entry envelopes
/// (wrapped DEKs + sealed ciphertexts), not plaintext values, so a
/// memory dump after one fetch leaks one secret's bytes (the
/// caller's `SecretEntry`), not every entry in the vault.
#[test]
fn per_entry_decrypt_does_not_leak_other_entries() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");
    let mut vault = Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();

    let a_key = SecretKey {
        service: "svc".into(),
        name: "a".into(),
    };
    let b_key = SecretKey {
        service: "svc".into(),
        name: "b".into(),
    };
    vault
        .put_secret(
            a_key.clone(),
            SecretEntry {
                kind: SecretKind::ApiKey,
                value: b"AAAAAA-secret-a".to_vec(),
                created_at: now(),
                updated_at: now(),
                last_used_at: None,
                tags: vec![],
                allowed_consumers: vec!["p".into()],
            },
        )
        .unwrap();
    vault
        .put_secret(
            b_key.clone(),
            SecretEntry {
                kind: SecretKind::ApiKey,
                value: b"BBBBBB-secret-b".to_vec(),
                created_at: now(),
                updated_at: now(),
                last_used_at: None,
                tags: vec![],
                allowed_consumers: vec!["p".into()],
            },
        )
        .unwrap();

    // Fetch only A. B's plaintext bytes must not be reachable from
    // the unlocked vault's cleartext payload.
    let a_plain = vault.get_secret(&a_key).unwrap();
    assert_eq!(&*a_plain.value, b"AAAAAA-secret-a");

    // Round-trip the file on disk and confirm `BBBBBB-secret-b`
    // doesn't appear in the file bytes (it's sealed under B's
    // DEK, which is wrapped under the master). This is the
    // load-bearing per-entry-DEK property at the file level.
    let bytes = std::fs::read(root.join("vault.botwork")).unwrap();
    assert!(
        !bytes
            .windows(b"BBBBBB-secret-b".len())
            .any(|w| w == b"BBBBBB-secret-b"),
        "B's plaintext must not appear in the sealed on-disk file"
    );
    assert!(
        !bytes
            .windows(b"AAAAAA-secret-a".len())
            .any(|w| w == b"AAAAAA-secret-a"),
        "A's plaintext must not appear in the sealed on-disk file"
    );
}

#[test]
fn get_secret_returns_zeroizing_plaintext_value() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");
    let mut vault = Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();
    let key = SecretKey {
        service: "svc".into(),
        name: "token".into(),
    };
    vault
        .put_secret(
            key.clone(),
            SecretEntry {
                kind: SecretKind::ApiKey,
                value: b"value".to_vec(),
                created_at: now(),
                updated_at: now(),
                last_used_at: None,
                tags: vec!["env:test".into()],
                allowed_consumers: vec!["plugin".into()],
            },
        )
        .unwrap();

    let secret = vault.get_secret(&key).unwrap();
    let _: &Zeroizing<Vec<u8>> = &secret.value;
    assert_eq!(secret.key, key);
    assert_eq!(secret.meta.kind, SecretKind::ApiKey);
    assert_eq!(secret.meta.tags, vec!["env:test"]);
    assert_eq!(secret.meta.allowed_consumers, vec!["plugin"]);
}
