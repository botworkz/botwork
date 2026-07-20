use botwork_vault::{SecretEntry, SecretKey, SecretKind, Vault};
use tempfile::TempDir;

const FAST_SUITE: u8 = 1;
const TEST_EXPORT_KEY: &[u8; 64] =
    b"deterministic-export-key-bytes-for-vault-metadata-test-AAAAAAAAA";

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[test]
fn tags_and_consumers_and_last_used() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");
    let mut vault = Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();

    let key = SecretKey {
        service: "github".to_string(),
        name: "pat".to_string(),
    };
    let entry = SecretEntry {
        kind: SecretKind::ApiKey,
        value: b"ghp_abc123".to_vec(),
        created_at: now(),
        updated_at: now(),
        last_used_at: None,
        tags: vec!["consumer:github-mcp".to_string(), "env:prod".to_string()],
        allowed_consumers: vec!["github-mcp".to_string()],
    };
    vault.put_secret(key.clone(), entry).unwrap();

    vault.lock();
    vault.unlock(TEST_EXPORT_KEY, FAST_SUITE).unwrap();

    let e = vault.get_secret(&key).unwrap();
    assert_eq!(e.meta.tags, vec!["consumer:github-mcp", "env:prod"]);
    assert_eq!(e.meta.allowed_consumers, vec!["github-mcp"]);
    assert!(e.meta.last_used_at.is_none());

    vault.touch_last_used(&key).unwrap();
    let meta = vault.list_secrets().unwrap();
    assert!(meta[0].1.last_used_at.is_some());

    vault.lock();
    vault.unlock(TEST_EXPORT_KEY, FAST_SUITE).unwrap();
    let e2 = vault.get_secret(&key).unwrap();
    assert!(e2.meta.last_used_at.is_some());
}
