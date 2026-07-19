#[cfg(unix)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use botwork_vault::{SecretEntry, SecretKey, SecretKind, Vault};
    use tempfile::TempDir;

    const FAST_SUITE: u8 = 1;
    const TEST_EXPORT_KEY: &[u8; 64] =
        b"deterministic-export-key-bytes-for-vault-on-disk-test-AAAAAAAAAA";

    #[test]
    fn single_file_and_permissions() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");

        Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();

        // `create` writes the sealed vault file plus the generation
        // sidecar (`vault.botwork.gen`) that backs the CAS / multi-writer
        // mechanism — see `vault/src/lock.rs`.
        let mut names: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["vault.botwork", "vault.botwork.gen"]);

        let root_meta = std::fs::metadata(&root).unwrap();
        assert_eq!(root_meta.permissions().mode() & 0o777, 0o700);

        let vault_meta = std::fs::metadata(root.join("vault.botwork")).unwrap();
        assert_eq!(vault_meta.permissions().mode() & 0o777, 0o600);

        // The generation sidecar holds no secret material but is kept
        // private anyway so it can't be tampered with by other local
        // users.
        let gen_meta = std::fs::metadata(root.join("vault.botwork.gen")).unwrap();
        assert_eq!(gen_meta.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn root_permissions_reasserted_on_write() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        let mut vault = Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();

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

        let root_meta = std::fs::metadata(&root).unwrap();
        assert_eq!(root_meta.permissions().mode() & 0o777, 0o700);
    }
}
