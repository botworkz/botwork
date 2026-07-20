use botwork_vault::{SecretEntry, SecretKey, SecretKind, Vault, VaultError};
use tempfile::TempDir;

const FAST_SUITE: u8 = 1;
const TEST_EXPORT_KEY: &[u8; 64] =
    b"deterministic-export-key-bytes-for-vault-roundtrip-test-AAAAAAAA";

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn make_entry() -> SecretEntry {
    SecretEntry {
        kind: SecretKind::Opaque,
        value: b"val".to_vec(),
        created_at: now(),
        updated_at: now(),
        last_used_at: None,
        tags: vec![],
        allowed_consumers: vec![],
    }
}

fn make_vault() -> (TempDir, Vault) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");
    let vault = Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();
    (dir, vault)
}

#[test]
fn valid_keys_accepted() {
    let (_dir, mut vault) = make_vault();
    let keys = [
        ("git", "id_ed25519"),
        ("git/ssh", "id_rsa"),
        ("github.com", "pat"),
        ("service-name", "key_1"),
    ];
    for (svc, name) in &keys {
        vault
            .put_secret(
                SecretKey {
                    service: svc.to_string(),
                    name: name.to_string(),
                },
                make_entry(),
            )
            .unwrap();
    }
}

#[test]
fn dot_component_rejected() {
    let (_dir, mut vault) = make_vault();
    let result = vault.put_secret(
        SecretKey {
            service: ".".to_string(),
            name: "key".to_string(),
        },
        make_entry(),
    );
    assert!(matches!(result, Err(VaultError::InvalidComponent(_))));
}

#[test]
fn dotdot_component_rejected() {
    let (_dir, mut vault) = make_vault();
    let result = vault.put_secret(
        SecretKey {
            service: "..".to_string(),
            name: "key".to_string(),
        },
        make_entry(),
    );
    assert!(matches!(result, Err(VaultError::InvalidComponent(_))));
}

#[test]
fn special_chars_rejected() {
    let (_dir, mut vault) = make_vault();
    let result = vault.put_secret(
        SecretKey {
            service: "git".to_string(),
            name: "my key".to_string(),
        },
        make_entry(),
    );
    assert!(matches!(result, Err(VaultError::InvalidComponent(_))));
}

#[test]
fn empty_service_rejected() {
    let (_dir, mut vault) = make_vault();
    let result = vault.put_secret(
        SecretKey {
            service: String::new(),
            name: "key".to_string(),
        },
        make_entry(),
    );
    assert!(matches!(result, Err(VaultError::InvalidComponent(_))));
}

#[test]
fn slash_in_name_rejected() {
    let (_dir, mut vault) = make_vault();
    let result = vault.put_secret(
        SecretKey {
            service: "git".to_string(),
            name: "a/b".to_string(),
        },
        make_entry(),
    );
    assert!(matches!(result, Err(VaultError::InvalidComponent(_))));
}
