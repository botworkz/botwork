#[cfg(unix)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use assert_cmd::Command;
    use predicates::prelude::*;
    use tempfile::TempDir;

    const ED25519_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI test-comment";
    const RSA_KEY: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAB rsa-comment";

    // Vault creation through the CLI requires a live broker; the
    // pubkey subcommands only touch the `public/` sidecar and don't
    // need the sealed vault file, so we create the vault root via the
    // library API in these tests instead of shelling out to the CLI.
    use botwork_vault::Vault;

    const FAST_SUITE: u8 = 1;
    const TEST_EXPORT_KEY: &[u8; 64] =
        b"deterministic-export-key-bytes-for-vault-pubkey-tests-AAAAAAAAAA";

    fn vault_cmd() -> Command {
        Command::cargo_bin("botwork-vault").unwrap()
    }

    fn root_str(dir: &TempDir) -> String {
        dir.path().join("vault").to_str().unwrap().to_string()
    }

    /// Create a vault root so the `public/` sidecar has something
    /// to attach to. The sidecar code does not touch the sealed vault file, so
    /// the master key derivation here is opaque to every subcommand
    /// under test.
    fn init_vault(root: &str) {
        Vault::create(std::path::Path::new(root), TEST_EXPORT_KEY, FAST_SUITE).unwrap();
    }

    /// Write a temporary key file and return its path as a String.
    fn write_key_file(dir: &TempDir, name: &str, content: &str) -> String {
        let path = dir.path().join(name);
        fs::write(&path, content).unwrap();
        path.to_str().unwrap().to_string()
    }

    // -----------------------------------------------------------------------
    // End-to-end: add → list --json → cat → delete
    // -----------------------------------------------------------------------

    #[test]
    fn pubkey_add_list_json_cat_delete() {
        let dir = TempDir::new().unwrap();
        let root = root_str(&dir);
        init_vault(&root);

        let key_file = write_key_file(&dir, "key.pub", ED25519_KEY);

        // add
        vault_cmd()
            .args([
                "pubkey",
                "add",
                "--root",
                &root,
                "--kind",
                "ssh",
                "--label",
                "mykey",
                "--from-file",
                &key_file,
            ])
            .assert()
            .code(0)
            .stdout(predicate::str::contains("stored mykey"));

        // list --json
        let list_out = vault_cmd()
            .args(["pubkey", "list", "--root", &root, "--kind", "ssh", "--json"])
            .assert()
            .code(0)
            .get_output()
            .stdout
            .clone();
        let list_str = String::from_utf8(list_out).unwrap();
        let items: Vec<serde_json::Value> = serde_json::from_str(list_str.trim()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["label"], "mykey");
        assert_eq!(items[0]["type"], "ssh-ed25519");
        assert_eq!(items[0]["comment"], "test-comment");

        // cat
        vault_cmd()
            .args(["pubkey", "cat", "--root", &root, "--kind", "ssh"])
            .assert()
            .code(0)
            .stdout(predicate::str::contains("ssh-ed25519"))
            .stdout(predicate::str::contains("test-comment"));

        // delete
        vault_cmd()
            .args([
                "pubkey", "delete", "--root", &root, "--kind", "ssh", "--label", "mykey",
            ])
            .assert()
            .code(0)
            .stdout(predicate::str::contains("deleted mykey"));

        // list should now be empty
        vault_cmd()
            .args(["pubkey", "list", "--root", &root, "--kind", "ssh", "--json"])
            .assert()
            .code(0)
            .stdout("[]\n");
    }

    // -----------------------------------------------------------------------
    // Permission bump: <root> becomes 0o701 after first add
    // -----------------------------------------------------------------------

    #[test]
    fn root_permissions_bumped_to_0o701_after_first_add() {
        let dir = TempDir::new().unwrap();
        let root_path = dir.path().join("vault");
        let root = root_path.to_str().unwrap();
        init_vault(root);

        // Verify root starts at 0700.
        let mode_before = fs::metadata(&root_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode_before, 0o700, "root should be 0700 after init");

        let key_file = write_key_file(&dir, "key.pub", ED25519_KEY);
        vault_cmd()
            .args([
                "pubkey",
                "add",
                "--root",
                root,
                "--kind",
                "ssh",
                "--label",
                "mykey",
                "--from-file",
                &key_file,
            ])
            .assert()
            .code(0);

        let mode_after = fs::metadata(&root_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode_after, 0o701,
            "root should be 0o701 after first pubkey add"
        );
    }

    // -----------------------------------------------------------------------
    // Unsupported kind
    // -----------------------------------------------------------------------

    #[test]
    fn pubkey_add_unsupported_kind_exits_2() {
        let dir = TempDir::new().unwrap();
        let root = root_str(&dir);
        init_vault(&root);

        let key_file = write_key_file(&dir, "key.pub", ED25519_KEY);
        vault_cmd()
            .args([
                "pubkey",
                "add",
                "--root",
                &root,
                "--kind",
                "tls",
                "--label",
                "mykey",
                "--from-file",
                &key_file,
            ])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("unsupported kind: tls"));
    }

    #[test]
    fn pubkey_list_unsupported_kind_exits_2() {
        let dir = TempDir::new().unwrap();
        let root = root_str(&dir);
        init_vault(&root);
        vault_cmd()
            .args(["pubkey", "list", "--root", &root, "--kind", "gpg"])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("unsupported kind: gpg"));
    }

    // -----------------------------------------------------------------------
    // Root not initialized
    // -----------------------------------------------------------------------

    #[test]
    fn pubkey_add_root_not_initialized_exits_2() {
        let dir = TempDir::new().unwrap();
        let root = root_str(&dir); // vault subdir doesn't exist yet
        let key_file = write_key_file(&dir, "key.pub", ED25519_KEY);
        vault_cmd()
            .args([
                "pubkey",
                "add",
                "--root",
                &root,
                "--kind",
                "ssh",
                "--label",
                "mykey",
                "--from-file",
                &key_file,
            ])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("vault root not initialized"));
    }

    #[test]
    fn pubkey_list_root_not_initialized_exits_2() {
        let dir = TempDir::new().unwrap();
        let root = root_str(&dir);
        vault_cmd()
            .args(["pubkey", "list", "--root", &root, "--kind", "ssh"])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("vault root not initialized"));
    }

    #[test]
    fn pubkey_cat_root_not_initialized_exits_2() {
        let dir = TempDir::new().unwrap();
        let root = root_str(&dir);
        vault_cmd()
            .args(["pubkey", "cat", "--root", &root, "--kind", "ssh"])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("vault root not initialized"));
    }

    #[test]
    fn pubkey_delete_root_not_initialized_exits_2() {
        let dir = TempDir::new().unwrap();
        let root = root_str(&dir);
        vault_cmd()
            .args([
                "pubkey", "delete", "--root", &root, "--kind", "ssh", "--label", "mykey",
            ])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("vault root not initialized"));
    }

    // -----------------------------------------------------------------------
    // Label already exists (exit 3)
    // -----------------------------------------------------------------------

    #[test]
    fn pubkey_add_duplicate_label_exits_3() {
        let dir = TempDir::new().unwrap();
        let root = root_str(&dir);
        init_vault(&root);
        let key_file = write_key_file(&dir, "key.pub", ED25519_KEY);

        vault_cmd()
            .args([
                "pubkey",
                "add",
                "--root",
                &root,
                "--kind",
                "ssh",
                "--label",
                "mykey",
                "--from-file",
                &key_file,
            ])
            .assert()
            .code(0);

        vault_cmd()
            .args([
                "pubkey",
                "add",
                "--root",
                &root,
                "--kind",
                "ssh",
                "--label",
                "mykey",
                "--from-file",
                &key_file,
            ])
            .assert()
            .code(3)
            .stderr(predicate::str::contains("label already exists: mykey"));
    }

    #[test]
    fn pubkey_add_force_overwrites_existing_label() {
        let dir = TempDir::new().unwrap();
        let root = root_str(&dir);
        init_vault(&root);
        let key_file1 = write_key_file(&dir, "key1.pub", ED25519_KEY);
        let key_file2 = write_key_file(&dir, "key2.pub", RSA_KEY);

        vault_cmd()
            .args([
                "pubkey",
                "add",
                "--root",
                &root,
                "--kind",
                "ssh",
                "--label",
                "mykey",
                "--from-file",
                &key_file1,
            ])
            .assert()
            .code(0);

        vault_cmd()
            .args([
                "pubkey",
                "add",
                "--root",
                &root,
                "--kind",
                "ssh",
                "--label",
                "mykey",
                "--from-file",
                &key_file2,
                "--force",
            ])
            .assert()
            .code(0);

        // Verify the key was replaced.
        vault_cmd()
            .args(["pubkey", "cat", "--root", &root, "--kind", "ssh"])
            .assert()
            .code(0)
            .stdout(predicate::str::contains("ssh-rsa"));
    }

    // -----------------------------------------------------------------------
    // Invalid key line (exit 2)
    // -----------------------------------------------------------------------

    #[test]
    fn pubkey_add_invalid_key_line_exits_2() {
        let dir = TempDir::new().unwrap();
        let root = root_str(&dir);
        init_vault(&root);
        let key_file = write_key_file(&dir, "bad.pub", "not-a-valid-key AAAA comment");

        vault_cmd()
            .args([
                "pubkey",
                "add",
                "--root",
                &root,
                "--kind",
                "ssh",
                "--label",
                "mykey",
                "--from-file",
                &key_file,
            ])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("not an OpenSSH public key"));
    }

    // -----------------------------------------------------------------------
    // No such label on delete (exit 3)
    // -----------------------------------------------------------------------

    #[test]
    fn pubkey_delete_missing_label_exits_3() {
        let dir = TempDir::new().unwrap();
        let root = root_str(&dir);
        init_vault(&root);

        vault_cmd()
            .args([
                "pubkey", "delete", "--root", &root, "--kind", "ssh", "--label", "missing",
            ])
            .assert()
            .code(3)
            .stderr(predicate::str::contains("no such label: missing"));
    }

    // -----------------------------------------------------------------------
    // list (no --json) emits TSV sorted by label
    // -----------------------------------------------------------------------

    #[test]
    fn pubkey_list_tsv_sorted() {
        let dir = TempDir::new().unwrap();
        let root = root_str(&dir);
        init_vault(&root);
        let ed_file = write_key_file(&dir, "ed.pub", ED25519_KEY);
        let rsa_file = write_key_file(&dir, "rsa.pub", RSA_KEY);

        vault_cmd()
            .args([
                "pubkey",
                "add",
                "--root",
                &root,
                "--kind",
                "ssh",
                "--label",
                "beta",
                "--from-file",
                &ed_file,
            ])
            .assert()
            .code(0);

        vault_cmd()
            .args([
                "pubkey",
                "add",
                "--root",
                &root,
                "--kind",
                "ssh",
                "--label",
                "alpha",
                "--from-file",
                &rsa_file,
            ])
            .assert()
            .code(0);

        let out = vault_cmd()
            .args(["pubkey", "list", "--root", &root, "--kind", "ssh"])
            .assert()
            .code(0)
            .get_output()
            .stdout
            .clone();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("alpha\t"), "alpha should be first");
        assert!(lines[1].starts_with("beta\t"), "beta should be second");
        assert!(lines[0].contains("ssh-rsa"));
        assert!(lines[1].contains("ssh-ed25519"));
    }

    // -----------------------------------------------------------------------
    // cat with no keys → exit 0 empty stdout
    // -----------------------------------------------------------------------

    #[test]
    fn pubkey_cat_empty_vault_exits_0_with_empty_stdout() {
        let dir = TempDir::new().unwrap();
        let root = root_str(&dir);
        init_vault(&root);

        vault_cmd()
            .args(["pubkey", "cat", "--root", &root, "--kind", "ssh"])
            .assert()
            .code(0)
            .stdout("");
    }

    // -----------------------------------------------------------------------
    // Public sidecar does not interfere with sealed vault writes
    // -----------------------------------------------------------------------

    #[test]
    fn vault_write_after_pubkey_add_preserves_0o701() {
        use botwork_vault::{PublicStore, SecretEntry, SecretKey, SecretKind, Vault};
        const FAST_SUITE: u8 = 1;
        const TEST_EXPORT_KEY: &[u8; 64] =
            b"deterministic-export-key-bytes-for-vault-pubkey-test-AAAAAAAAAAA";

        let dir = TempDir::new().unwrap();
        let root_path = dir.path().join("vault");
        Vault::create(&root_path, TEST_EXPORT_KEY, FAST_SUITE).unwrap();

        let store = PublicStore::new(&root_path);
        let ed_file = dir.path().join("ed.pub");
        fs::write(&ed_file, ED25519_KEY).unwrap();
        store.add_ssh("mykey", ED25519_KEY, false).unwrap();

        // Confirm root is 0701 after pubkey add.
        let mode = fs::metadata(&root_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o701);

        // Now do a sealed vault write.
        let mut vault = Vault::new(&root_path);
        vault.unlock(TEST_EXPORT_KEY, FAST_SUITE).unwrap();
        let now = chrono::Utc::now().timestamp();
        vault
            .put_secret(
                SecretKey {
                    service: "svc".to_string(),
                    name: "key".to_string(),
                },
                SecretEntry {
                    kind: SecretKind::ApiKey,
                    value: b"val".to_vec(),
                    created_at: now,
                    updated_at: now,
                    last_used_at: None,
                    tags: vec![],
                    allowed_consumers: vec![],
                },
            )
            .unwrap();

        // Root should still be 0701 because public/ dir exists.
        let mode_after = fs::metadata(&root_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode_after, 0o701,
            "vault write should preserve 0o701 when public/ sidecar exists"
        );
    }
}
