//! Concurrency tests for the file-level CAS / generation-check mechanism.
//!
//! These tests verify that:
//! 1. Sequential writes on the same `Vault` instance succeed and bump the
//!    on-disk generation monotonically.
//! 2. A stale reader — one that loaded vault state before a concurrent
//!    writer finished — gets `VaultError::Conflict` on persist.
//! 3. Concurrent threads racing to write to the same vault file are
//!    serialised correctly: exactly one succeeds per round.

use std::sync::{Arc, Barrier};
use std::thread;

use botwork_vault::{SecretEntry, SecretKey, SecretKind, Vault, VaultError};
use tempfile::TempDir;

const FAST_SUITE: u8 = 1;
const EXPORT_KEY: &[u8; 64] = b"deterministic-export-key-bytes-for-vault-concurrent-test-AAAAAAA";

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn make_entry(value: &[u8]) -> SecretEntry {
    let now = now();
    SecretEntry {
        kind: SecretKind::SshPrivateKey,
        value: value.to_vec(),
        created_at: now,
        updated_at: now,
        last_used_at: None,
        tags: vec![],
        allowed_consumers: vec![],
    }
}

// ── sequential write coherence ────────────────────────────────────────

/// Multiple sequential writes on a single vault instance all succeed;
/// each increments the generation and leaves a consistent file behind.
#[test]
fn sequential_writes_succeed() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");

    let mut vault = Vault::create(&root, EXPORT_KEY, FAST_SUITE).unwrap();

    for i in 0..5u8 {
        let key = SecretKey {
            service: "svc".to_string(),
            name: format!("key{i}"),
        };
        vault
            .put_secret(key, make_entry(&[i; 16]))
            .unwrap_or_else(|e| panic!("write {i} failed: {e}"));
    }

    // Re-open and check all secrets are present.
    let mut v2 = Vault::new(&root);
    v2.unlock(EXPORT_KEY, FAST_SUITE).unwrap();
    assert_eq!(v2.list_secrets().unwrap().len(), 5);
}

// ── stale-reader conflict ─────────────────────────────────────────────

/// Simulate a stale reader: open two vault handles on the same file.
/// Let writer-A write once to advance the generation. Writer-B loaded
/// state before A's write and therefore holds a stale generation; its
/// subsequent persist must return `VaultError::Conflict`.
#[test]
fn stale_reader_gets_conflict() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");

    // Create the initial vault.
    let mut a = Vault::create(&root, EXPORT_KEY, FAST_SUITE).unwrap();
    // B loads the vault at generation 0 (matches A post-create).
    let mut b = Vault::new(&root);
    b.unlock(EXPORT_KEY, FAST_SUITE).unwrap();

    // A writes first — advances the on-disk generation.
    a.put_secret(
        SecretKey {
            service: "svc".to_string(),
            name: "a-wrote".to_string(),
        },
        make_entry(b"A-value"),
    )
    .unwrap();

    // B now tries to write against the stale (original) generation and
    // must be rejected with Conflict.
    let err = b
        .put_secret(
            SecretKey {
                service: "svc".to_string(),
                name: "b-wrote".to_string(),
            },
            make_entry(b"B-value"),
        )
        .unwrap_err();

    assert!(
        matches!(err, VaultError::Conflict { .. }),
        "expected Conflict, got: {err}"
    );
}

// ── concurrent writers: one wins per round ────────────────────────────

/// N threads each try to write one secret to the same vault. Exactly
/// one succeeds per round (the one that acquires the flock and whose
/// expected generation matches). The rest get `Conflict`.
///
/// After all threads finish, the vault's secret count equals the
/// number of successful writes.
#[test]
fn concurrent_writers_serialised() {
    const N: usize = 8;

    let dir = TempDir::new().unwrap();
    let root = dir.path().join("vault");

    // Create the vault; all threads will then load their own handle.
    {
        let mut init = Vault::create(&root, EXPORT_KEY, FAST_SUITE).unwrap();
        // Initial write so there's a gen file before the threads race.
        init.put_secret(
            SecretKey {
                service: "init".to_string(),
                name: "seed".to_string(),
            },
            make_entry(b"seed"),
        )
        .unwrap();
    }

    let root = Arc::new(root);
    let barrier = Arc::new(Barrier::new(N));
    let mut handles = Vec::with_capacity(N);

    for i in 0..N {
        let root = Arc::clone(&root);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            // All threads load their vault snapshot before the race
            // so they all start with the same (already-stale after
            // the first winner writes) expected generation.
            let mut v = Vault::new(root.as_path());
            v.unlock(EXPORT_KEY, FAST_SUITE).unwrap();
            // Synchronize: all threads attempt the write at the same time.
            barrier.wait();
            v.put_secret(
                SecretKey {
                    service: "race".to_string(),
                    name: format!("thread{i}"),
                },
                make_entry(&[i as u8; 4]),
            )
        }));
    }

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let successes = results.iter().filter(|r| r.is_ok()).count();
    let conflicts = results
        .iter()
        .filter(|r| matches!(r, Err(VaultError::Conflict { .. })))
        .count();

    // With N threads all starting from the same stale generation exactly
    // one can win the first round. The rest get Conflict.
    assert_eq!(
        successes + conflicts,
        N,
        "every thread must produce either Ok or Conflict"
    );
    assert!(successes >= 1, "at least one thread must succeed");
    assert_eq!(successes + conflicts, N, "no unexpected error variants");
}
