//! Secret-safety pin for vault library diagnostics.
//!
//! Installs a capturing tracing subscriber and exercises all
//! meaningful vault lifecycle operations, then asserts that no
//! secret material — export_key bytes, plaintext secret values,
//! or their base64 representations — appears in any log event,
//! at any level.
//!
//! Mirrors the `auth-broker/tests/log_redaction.rs` pattern.

use std::io;
use std::sync::{Arc, Mutex, Once, OnceLock};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use botwork_vault::{SecretEntry, SecretKey, SecretKind, Vault};
use tempfile::TempDir;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

// ── log capture infrastructure ────────────────────────────────────────────────

#[derive(Clone)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

struct SharedWriterGuard(Arc<Mutex<Vec<u8>>>);

impl<'a> MakeWriter<'a> for SharedWriter {
    type Writer = SharedWriterGuard;

    fn make_writer(&'a self) -> Self::Writer {
        SharedWriterGuard(self.0.clone())
    }
}

impl io::Write for SharedWriterGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn log_buffer() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| Arc::new(Mutex::new(Vec::new())))
        .clone()
}

fn init_logs() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let writer = SharedWriter(log_buffer());
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("trace"))
            .with_writer(writer)
            .with_target(false)
            .without_time()
            .with_ansi(false)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

fn clear_logs() {
    log_buffer().lock().unwrap().clear();
}

fn collected_logs() -> String {
    String::from_utf8_lossy(&log_buffer().lock().unwrap()).into_owned()
}

// Serialise all tests so the shared log buffer is unambiguous per test.
fn test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// ── test constants ─────────────────────────────────────────────────────────────

const SUITE: u8 = 1;
/// A 64-byte export_key that must NEVER appear in any log line.
const EXPORT_KEY: &[u8] = b"super-secret-export-key-bytes-vault-log-redaction-test-AAAAAAA!";
/// The plaintext secret value that must NEVER appear in any log line.
const SECRET_VALUE: &[u8] = b"ghp_SuperSecretTokenValue123456789";

// ── helpers ────────────────────────────────────────────────────────────────────

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn make_entry(value: &[u8]) -> (SecretKey, SecretEntry) {
    let key = SecretKey {
        service: "github.com".to_string(),
        name: "pat".to_string(),
    };
    let entry = SecretEntry {
        kind: SecretKind::ApiKey,
        value: value.to_vec(),
        created_at: now(),
        updated_at: now(),
        last_used_at: None,
        tags: vec![],
        allowed_consumers: vec!["plugin-a".to_string()],
    };
    (key, entry)
}

// ── tests ──────────────────────────────────────────────────────────────────────

/// Vault `create` + `put_secret` must not log the export_key, master key,
/// or the plaintext secret value at any level.
#[test]
fn create_and_put_do_not_leak_secrets() {
    let _guard = test_lock().lock().unwrap();
    init_logs();
    clear_logs();

    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");
    let mut vault = Vault::create(&root, EXPORT_KEY, SUITE).unwrap();

    let (key, entry) = make_entry(SECRET_VALUE);
    vault.put_secret(key, entry).unwrap();

    let logs = collected_logs();

    // Export key must not appear.
    assert!(
        !logs.contains(std::str::from_utf8(EXPORT_KEY).unwrap()),
        "export_key leaked in logs during create/put: {logs}"
    );
    // Plaintext secret value must not appear.
    assert!(
        !logs.contains(std::str::from_utf8(SECRET_VALUE).unwrap()),
        "plaintext secret leaked in logs during create/put: {logs}"
    );
    // Base64-encoded secret value must not appear.
    let b64 = URL_SAFE_NO_PAD.encode(SECRET_VALUE);
    assert!(
        !logs.contains(&b64),
        "base64 secret leaked in logs during create/put: {logs}"
    );

    // Lifecycle events SHOULD be present.
    assert!(
        logs.contains("[vault] created vault"),
        "expected create event missing in logs: {logs}"
    );
    assert!(
        logs.contains("[vault] put_secret service=github.com name=pat"),
        "expected put_secret event missing in logs: {logs}"
    );
}

/// Vault `unlock` + `get_secret` must not log the export_key or the
/// plaintext secret value.
#[test]
fn unlock_and_get_do_not_leak_secrets() {
    let _guard = test_lock().lock().unwrap();
    init_logs();
    clear_logs();

    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");

    // Seed a vault without capturing logs for the setup phase.
    {
        let mut vault = Vault::create(&root, EXPORT_KEY, SUITE).unwrap();
        let (key, entry) = make_entry(SECRET_VALUE);
        vault.put_secret(key, entry).unwrap();
    }

    // Now capture logs for the operations under test.
    clear_logs();
    let mut vault = Vault::new(&root);
    vault.unlock(EXPORT_KEY, SUITE).unwrap();
    let key = SecretKey {
        service: "github.com".to_string(),
        name: "pat".to_string(),
    };
    let secret = vault.get_secret(&key).unwrap();
    // Double-check the value is correct (so the test is meaningful).
    assert_eq!(&*secret.value, SECRET_VALUE);

    let logs = collected_logs();

    assert!(
        !logs.contains(std::str::from_utf8(EXPORT_KEY).unwrap()),
        "export_key leaked in logs during unlock/get: {logs}"
    );
    assert!(
        !logs.contains(std::str::from_utf8(SECRET_VALUE).unwrap()),
        "plaintext secret leaked in logs during unlock/get: {logs}"
    );
    let b64 = URL_SAFE_NO_PAD.encode(SECRET_VALUE);
    assert!(
        !logs.contains(&b64),
        "base64 secret leaked in logs during unlock/get: {logs}"
    );

    // Lifecycle events SHOULD be present.
    assert!(
        logs.contains("[vault] unlocked vault"),
        "expected unlock event missing in logs: {logs}"
    );
    assert!(
        logs.contains("[vault] get_secret service=github.com name=pat"),
        "expected get_secret event missing in logs: {logs}"
    );
}

/// `list_secrets` and `delete_secret` must not log any secret material.
#[test]
fn list_and_delete_do_not_leak_secrets() {
    let _guard = test_lock().lock().unwrap();
    init_logs();
    clear_logs();

    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");

    let mut vault = Vault::create(&root, EXPORT_KEY, SUITE).unwrap();
    let (key, entry) = make_entry(SECRET_VALUE);
    vault.put_secret(key.clone(), entry).unwrap();

    clear_logs();
    let entries = vault.list_secrets().unwrap();
    assert_eq!(entries.len(), 1);
    vault.delete_secret(&key).unwrap();

    let logs = collected_logs();

    assert!(
        !logs.contains(std::str::from_utf8(EXPORT_KEY).unwrap()),
        "export_key leaked in logs during list/delete: {logs}"
    );
    assert!(
        !logs.contains(std::str::from_utf8(SECRET_VALUE).unwrap()),
        "plaintext secret leaked in logs during list/delete: {logs}"
    );
    let b64 = URL_SAFE_NO_PAD.encode(SECRET_VALUE);
    assert!(
        !logs.contains(&b64),
        "base64 secret leaked in logs during list/delete: {logs}"
    );

    assert!(
        logs.contains("[vault] delete_secret service=github.com name=pat"),
        "expected delete_secret event missing in logs: {logs}"
    );
}

/// Supplying a wrong export_key (AEAD auth failure) must not log the
/// bad key bytes either.
#[test]
fn auth_failure_does_not_leak_wrong_key() {
    let _guard = test_lock().lock().unwrap();
    init_logs();
    clear_logs();

    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");
    Vault::create(&root, EXPORT_KEY, SUITE).unwrap();

    clear_logs();
    const WRONG_KEY: &[u8] = b"wrong-key-bytes-that-must-not-appear-in-any-log-output";
    let mut vault = Vault::new(&root);
    let result = vault.unlock(WRONG_KEY, SUITE);
    assert!(result.is_err(), "expected auth failure");

    let logs = collected_logs();
    assert!(
        !logs.contains(std::str::from_utf8(WRONG_KEY).unwrap()),
        "wrong key bytes leaked in logs on auth failure: {logs}"
    );
    assert!(
        !logs.contains(std::str::from_utf8(EXPORT_KEY).unwrap()),
        "original export_key leaked in logs on auth failure: {logs}"
    );
}
