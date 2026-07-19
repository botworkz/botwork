//! Fuzz target: v4 vault decode path (`open_contents` / `postcard`).
//!
//! Exercises four code paths in a single target:
//!
//! 1. **Full file decode** — write the input bytes to a temp vault file
//!    and call [`Vault::unlock`].  This traverses the outer header checks,
//!    HKDF derivation, ChaCha20-Poly1305 AEAD, and postcard payload decode
//!    (`open_contents`).
//!
//! 2. **Direct postcard decode of `VaultContents`** — calls
//!    `postcard::from_bytes::<VaultContents>` directly without a key.
//!    This exercises the inner postcard/serde decode logic independently
//!    of the AEAD.
//!
//! 3. **Per-entry envelope decode + open** — deserialises an
//!    [`EntryEnvelope`] from the raw bytes with postcard, then calls
//!    [`open_entry`] with a fixed master key.  This exercises
//!    `unwrap_dek_under_master`, the per-entry AEAD, and the `aad_bytes`
//!    helper.
//!
//! 4. **Synthesised `EntryEnvelope`** — constructs an envelope directly
//!    from two halves of `data` (wrapped DEK + ciphertext) and calls
//!    [`open_entry`] with a fixed master key.  This lets the fuzzer vary
//!    the wrapped-DEK blob and per-entry ciphertext independently without
//!    needing a valid postcard encoding.
//!
//! The invariant: **no input may cause an abort or panic**.  Every
//! malformed input must return a structured [`VaultError`] (or a
//! `postcard::Error`); only `Ok` on Exercise 1 is allowed if the
//! libfuzzer engine is lucky enough to craft a validly-keyed vault.
//!
//! ## How to run
//!
//! See `vault/fuzz/README.md` for full instructions.

#![no_main]

use libfuzzer_sys::fuzz_target;

use botwork_vault::contents::{open_entry, EntryEnvelope, EntryMeta, SecretKind};
use botwork_vault::VaultContents;
use botwork_vault::Vault;

// Deterministic export key used for Exercise 1.  We always use the
// same key so the AEAD almost never passes on random input, which
// keeps the focus on the parser / header checks rather than the
// crypto.  The seed corpus (see `corpus/fuzz_open_contents/`) gives
// the fuzzer a starting point where the AEAD *does* pass so it can
// reach the postcard decode inside `open_contents`.
const EXPORT_KEY: &[u8; 64] =
    b"deterministic-export-key-bytes-for-fuzz-target-AAAAAAAAAAAAAAAAA";
const SUITE_VERSION: u8 = 1;

fuzz_target!(|data: &[u8]| {
    // ------------------------------------------------------------------
    // Exercise 1: full file decode via the public Vault::unlock API.
    // ------------------------------------------------------------------
    {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("vault");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("vault.botwork"), data).unwrap();
        // Discard the result — we only care that no panic occurs.
        let _ = Vault::new(&root).unlock(EXPORT_KEY, SUITE_VERSION);
    }

    // ------------------------------------------------------------------
    // Exercise 2: postcard decode of VaultContents directly (no key).
    // ------------------------------------------------------------------
    {
        let _ = postcard::from_bytes::<VaultContents>(data);
    }

    // ------------------------------------------------------------------
    // Exercise 3: postcard decode of EntryEnvelope + per-entry open.
    // ------------------------------------------------------------------
    {
        if let Ok(envelope) = postcard::from_bytes::<EntryEnvelope>(data) {
            let master = [0u8; 32];
            let _ = open_entry(&master, &envelope);
        }
    }

    // ------------------------------------------------------------------
    // Exercise 4: synthesised EntryEnvelope with data as individual
    // fields (exercises the wrapped-DEK / value AEAD independently).
    // ------------------------------------------------------------------
    {
        use chrono::Utc;

        // Use the first half of `data` as the wrapped DEK blob and the
        // second half as the ciphertext, so the fuzzer can vary both
        // independently.  When `data` is shorter than 12 bytes we still
        // exercise the minimum-length guard inside `unwrap_dek_under_master`.
        let mid = data.len() / 2;
        let wrapped_dek = data[..mid].to_vec();
        let ciphertext = data[mid..].to_vec();

        // Nonce: take the first 12 bytes of data (or zeros if shorter).
        let mut nonce = [0u8; 12];
        let copy_len = data.len().min(12);
        nonce[..copy_len].copy_from_slice(&data[..copy_len]);

        let now = Utc::now();
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
                created_at_utc: now,
                rotated_at_utc: now,
            },
        };

        let master = [0xAAu8; 32];
        let _ = open_entry(&master, &envelope);
    }
});
