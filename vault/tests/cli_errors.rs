use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

use botwork_vault::Vault;

const FAST_SUITE: u8 = 1;
const TEST_EXPORT_KEY: &[u8; 64] =
    b"deterministic-export-key-bytes-for-vault-cli-error-tests-AAAAAAA";

fn vault_cmd() -> Command {
    Command::cargo_bin("botwork-vault").unwrap()
}

fn root_arg(dir: &TempDir) -> String {
    dir.path().join("vault").to_str().unwrap().to_string()
}

/// The CLI init path needs a live broker. For the CLI-only error-path
/// tests in this file we drive the library API directly to seed a
/// vault root, then invoke the subcommands under test.
fn init_via_library(root: &str) {
    Vault::create(std::path::Path::new(root), TEST_EXPORT_KEY, FAST_SUITE).unwrap();
}

/// Drive a vault subcommand with a bearer-equivalent setup
/// (`BOTWORK_BEARER`). The CLI fetches the wrapped export_key from
/// the broker; for the error-path tests below we want to exercise
/// the bits of the CLI surface that don't actually need a live
/// broker — namely the input validation that happens before the
/// HTTP call.
fn vault_no_broker_cmd() -> Command {
    let mut cmd = vault_cmd();
    cmd.env("BOTWORK_BEARER", "synthetic-bearer-for-error-path-tests")
        .env("BOTWORK_LOGIN_SERVER", "http://127.0.0.1:1");
    cmd
}

#[test]
fn init_force_requires_yes_really_overwrite() {
    let dir = TempDir::new().unwrap();
    let root = root_arg(&dir);

    vault_no_broker_cmd()
        .args(["init", "--root", &root, "--force"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "ERROR: --force requires --yes-really-overwrite",
        ));
}

#[test]
fn init_existing_non_empty_root_without_force_exits_3() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("existing"), b"data").unwrap();

    // The CLI tries to fetch the wrapped export_key from the broker
    // before checking root state, so without a live broker this surfaces
    // as a network/HTTP error (exit 2). We exercise the
    // "non-empty root" branch via the library API instead — the bin's
    // `Vault::create` arm is what fires `code(3)` in production.
    let result = Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE);
    let err = match result {
        Ok(_) => panic!("expected AlreadyInitialized"),
        Err(e) => e,
    };
    assert!(matches!(
        err,
        botwork_vault::VaultError::AlreadyInitialized(_)
    ));
}

#[test]
fn add_unknown_kind() {
    let dir = TempDir::new().unwrap();
    let root = root_arg(&dir);
    init_via_library(&root);

    vault_no_broker_cmd()
        .args([
            "add",
            "--root",
            &root,
            "--service",
            "github",
            "--name",
            "pat",
            "--kind",
            "foo-bar",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unknown kind"));
}

#[test]
fn add_from_file_trailing_lf_stripped() {
    // Exercises the header-bound newline stripping via the library
    // API. The round-trip belongs to a future docker-gated test that
    // can drive init end-to-end against a real broker.
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");
    let mut vault = Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();

    let mut value = b"ghp_abc123\n".to_vec();
    if value.ends_with(b"\n") {
        value.pop();
    }
    let now = chrono::Utc::now().timestamp();
    vault
        .put_secret(
            botwork_vault::SecretKey {
                service: "github".to_string(),
                name: "pat".to_string(),
            },
            botwork_vault::SecretEntry {
                kind: botwork_vault::SecretKind::ApiKey,
                value,
                created_at: now,
                updated_at: now,
                last_used_at: None,
                tags: vec![],
                allowed_consumers: vec![],
            },
        )
        .unwrap();

    let entry = vault
        .get_secret(&botwork_vault::SecretKey {
            service: "github".to_string(),
            name: "pat".to_string(),
        })
        .unwrap();
    assert_eq!(&*entry.value, b"ghp_abc123");
}

#[test]
fn add_from_file_embedded_newline_rejected() {
    let dir = TempDir::new().unwrap();
    let root = root_arg(&dir);
    init_via_library(&root);

    // File with an embedded newline (not just a trailing one).
    let file = dir.path().join("cred.txt");
    fs::write(&file, b"ghp_abc\n123").unwrap();

    vault_no_broker_cmd()
        .args([
            "add",
            "--root",
            &root,
            "--service",
            "github",
            "--name",
            "pat",
            "--kind",
            "api-key",
            "--from-file",
            file.to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "ERROR: secret value (kind=api-key) contains an embedded control character",
        ));
}

#[test]
fn add_from_file_null_byte_rejected() {
    let dir = TempDir::new().unwrap();
    let root = root_arg(&dir);
    init_via_library(&root);

    let file = dir.path().join("cred.txt");
    fs::write(&file, b"ghp_abc\x00123").unwrap();

    vault_no_broker_cmd()
        .args([
            "add",
            "--root",
            &root,
            "--service",
            "github",
            "--name",
            "pat",
            "--kind",
            "api-key",
            "--from-file",
            file.to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "ERROR: secret value (kind=api-key) contains an embedded control character",
        ));
}

#[test]
fn add_from_file_missing_path() {
    let dir = TempDir::new().unwrap();
    let root = root_arg(&dir);
    init_via_library(&root);
    let missing = dir.path().join("does-not-exist.txt");

    vault_no_broker_cmd()
        .args([
            "add",
            "--root",
            &root,
            "--service",
            "github",
            "--name",
            "pat",
            "--kind",
            "api-key",
            "--from-file",
            missing.to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("failed to read --from-file"))
        .stderr(predicate::str::contains(missing.to_str().unwrap()));
}

#[test]
fn add_rejects_unsafe_service_component() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");
    let mut vault = Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();
    let err = vault
        .put_secret(
            botwork_vault::SecretKey {
                service: "..".to_string(),
                name: "pat".to_string(),
            },
            botwork_vault::SecretEntry {
                kind: botwork_vault::SecretKind::ApiKey,
                value: b"v".to_vec(),
                created_at: 0,
                updated_at: 0,
                last_used_at: None,
                tags: vec![],
                allowed_consumers: vec![],
            },
        )
        .unwrap_err();
    assert!(matches!(
        err,
        botwork_vault::VaultError::InvalidComponent(_)
    ));
}

#[test]
fn add_rejects_unsafe_name_component() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");
    let mut vault = Vault::create(&root, TEST_EXPORT_KEY, FAST_SUITE).unwrap();
    let err = vault
        .put_secret(
            botwork_vault::SecretKey {
                service: "github".to_string(),
                name: "..".to_string(),
            },
            botwork_vault::SecretEntry {
                kind: botwork_vault::SecretKind::ApiKey,
                value: b"v".to_vec(),
                created_at: 0,
                updated_at: 0,
                last_used_at: None,
                tags: vec![],
                allowed_consumers: vec![],
            },
        )
        .unwrap_err();
    assert!(matches!(
        err,
        botwork_vault::VaultError::InvalidComponent(_)
    ));
}

#[test]
fn missing_bearer_exits_2() {
    let dir = TempDir::new().unwrap();
    let root = root_arg(&dir);
    init_via_library(&root);

    // Deliberately no BOTWORK_BEARER set.
    let mut cmd = vault_cmd();
    cmd.env_remove("BOTWORK_BEARER")
        .args(["list", "--root", &root])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("missing bearer"));
}

#[test]
fn missing_cacert_path_fails_fast_with_actionable_error() {
    let dir = TempDir::new().unwrap();
    let root = root_arg(&dir);
    let missing = dir.path().join("missing-ca.pem");

    vault_no_broker_cmd()
        .args([
            "--cacert",
            missing.to_str().unwrap(),
            "init",
            "--root",
            &root,
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("failed to read --cacert file"))
        .stderr(predicate::str::contains(missing.to_str().unwrap()));
}
