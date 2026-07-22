use std::collections::BTreeMap;

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::error::VaultError;
use crate::kdf::{gen_dek, KEY_LEN};

/// 12-byte ChaCha20-Poly1305 nonce used per-entry inside the v4
/// payload. Each entry carries its own nonce; the nonce is
/// generated fresh from `OsRng` every time the entry's ciphertext
/// is sealed (including overwrites via `put_secret`).
pub const ENTRY_NONCE_LEN: usize = 12;

#[derive(Serialize, Deserialize, Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub struct SecretKey {
    pub service: String,
    pub name: String,
}

impl std::fmt::Display for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.service, self.name)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Eq, PartialEq, Debug)]
pub enum SecretKind {
    SshPrivateKey,
    SshPublicKey,
    ApiKey,
    OauthToken,
    Pem,
    Password,
    Opaque,
}

impl std::fmt::Display for SecretKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SecretKind::SshPrivateKey => "ssh-private-key",
            SecretKind::SshPublicKey => "ssh-public-key",
            SecretKind::ApiKey => "api-key",
            SecretKind::OauthToken => "oauth-token",
            SecretKind::Pem => "pem",
            SecretKind::Password => "password",
            SecretKind::Opaque => "opaque",
        })
    }
}

impl std::str::FromStr for SecretKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ssh-private-key" | "SshPrivateKey" => Ok(SecretKind::SshPrivateKey),
            "ssh-public-key" | "SshPublicKey" => Ok(SecretKind::SshPublicKey),
            "api-key" | "ApiKey" => Ok(SecretKind::ApiKey),
            "oauth-token" | "OauthToken" => Ok(SecretKind::OauthToken),
            "pem" | "Pem" => Ok(SecretKind::Pem),
            "password" | "Password" => Ok(SecretKind::Password),
            "opaque" | "Opaque" => Ok(SecretKind::Opaque),
            _ => Err(format!("unknown kind: {s}")),
        }
    }
}

/// Logical record of a single secret. This is the view callers see
/// after [`crate::Vault::decrypt_entry`] or after iterating the
/// payload returned by `unlock`. On disk the value bytes are sealed
/// per-entry under a freshly-generated DEK (see [`EntryEnvelope`]
/// below); the [`SecretEntry`] form is the post-decrypt projection.
#[derive(Serialize, Deserialize, Clone)]
pub struct SecretEntry {
    pub kind: SecretKind,
    pub value: Vec<u8>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: Option<i64>,
    pub tags: Vec<String>,
    pub allowed_consumers: Vec<String>,
}

impl Zeroize for SecretEntry {
    fn zeroize(&mut self) {
        self.value.zeroize();
    }
}

/// Metadata-only view of a secret. Returned by
/// [`crate::Vault::list_entries`] / [`crate::Vault::list_secrets`]
/// so a caller can enumerate without paying a per-entry decrypt
/// round-trip.
#[derive(Clone, Debug)]
pub struct SecretMeta {
    pub kind: SecretKind,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: Option<i64>,
    pub tags: Vec<String>,
    pub allowed_consumers: Vec<String>,
}

/// Decrypted secret payload returned by [`crate::Vault::get_secret`].
///
/// The secret value bytes are held in `Zeroizing<Vec<u8>>`, so they
/// are scrubbed when this value is dropped.
///
/// ```compile_fail
/// use botwork_vault::DecryptedSecret;
///
/// fn requires_clone<T: Clone>(_value: T) {}
///
/// fn check(secret: DecryptedSecret) {
///     requires_clone(secret);
/// }
/// ```
pub struct DecryptedSecret {
    pub key: SecretKey,
    pub meta: SecretMeta,
    pub value: Zeroizing<Vec<u8>>,
}

impl From<&SecretEntry> for SecretMeta {
    fn from(e: &SecretEntry) -> Self {
        SecretMeta {
            kind: e.kind,
            created_at: e.created_at,
            updated_at: e.updated_at,
            last_used_at: e.last_used_at,
            tags: e.tags.clone(),
            allowed_consumers: e.allowed_consumers.clone(),
        }
    }
}

/// Per-entry envelope inside the v4 payload.
///
/// Each entry's value bytes are sealed under a freshly-generated
/// DEK; the DEK itself is wrapped under the v4 master key. Per-entry
/// unlock means a memory dump captured after a single fetch leaks
/// exactly that one entry's plaintext, not every entry in the vault.
///
/// Wire shape:
///
/// - `wrapped_dek` — `nonce(12) || ciphertext(32) || tag(16)` =
///   60 bytes. The inner 32 bytes are the raw DEK.
/// - `ciphertext` — `value` sealed under the DEK with `nonce` as
///   the per-entry AEAD nonce. AAD is the serialised metadata
///   below (everything *except* `ciphertext` itself), so any
///   tamper of the metadata invalidates the AEAD tag.
/// - `nonce` — freshly generated on every reseal; never reused.
/// - `version` — per-entry rotation tag. v4 ships as `1` for every
///   entry; the room exists so a future per-entry rotation API
///   (out of scope here — see issue body's "out of scope" list)
///   can advance entries individually without bumping the outer
///   vault format.
#[derive(Serialize, Deserialize, Clone)]
pub struct EntryEnvelope {
    /// DEK wrapped under the v4 master key. 60 bytes:
    /// `nonce(12) || ciphertext(32) || tag(16)`.
    pub wrapped_dek: Vec<u8>,
    /// Value bytes sealed under the DEK, plus 16-byte AEAD tag.
    pub ciphertext: Vec<u8>,
    /// Per-entry AEAD nonce for `ciphertext`.
    pub nonce: [u8; ENTRY_NONCE_LEN],
    /// Per-entry version. v4 ships as `1`.
    pub version: u8,
    /// Cleartext metadata — kind + timestamps + tags + allowed
    /// consumers. Carried alongside `ciphertext` so `list_entries`
    /// can serve metadata without a decrypt. Bound into the
    /// per-entry AEAD via AAD so tampering invalidates the tag.
    pub meta: EntryMeta,
}

impl Zeroize for EntryEnvelope {
    fn zeroize(&mut self) {
        self.wrapped_dek.zeroize();
        self.ciphertext.zeroize();
        self.nonce.zeroize();
        self.version.zeroize();
    }
}

/// Cleartext metadata stored per entry inside the v4 payload.
///
/// Mirrors the public [`SecretMeta`] but lives on the envelope so
/// `list_entries` can read it without unwrapping the DEK. AEAD AAD
/// covers a deterministic serialisation of this struct (the
/// `aad_bytes` helper below) so a tamper of any metadata field
/// invalidates the per-entry AEAD tag.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct EntryMeta {
    pub kind: SecretKind,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: Option<i64>,
    pub tags: Vec<String>,
    pub allowed_consumers: Vec<String>,
    /// Wall-clock at vault-create / put-secret time. Mirrors
    /// `created_at` but typed as a chrono `DateTime<Utc>` so the
    /// future per-entry rotation API can reason about envelope age
    /// without round-tripping through unix seconds.
    pub created_at_utc: DateTime<Utc>,
    /// Wall-clock at most-recent reseal. Same shape as `updated_at`
    /// but typed for the same reason.
    pub rotated_at_utc: DateTime<Utc>,
}

impl From<&EntryMeta> for SecretMeta {
    fn from(m: &EntryMeta) -> Self {
        SecretMeta {
            kind: m.kind,
            created_at: m.created_at,
            updated_at: m.updated_at,
            last_used_at: m.last_used_at,
            tags: m.tags.clone(),
            allowed_consumers: m.allowed_consumers.clone(),
        }
    }
}

/// v4 payload: the bytes the outer-file AEAD seals under the
/// master key.
///
/// Holds a `BTreeMap` of `SecretKey -> EntryEnvelope`. The outer
/// AEAD authenticates this whole serialised blob, so adding or
/// removing an entry forces a reseal of the outer payload, but
/// the per-entry DEK lives inside the envelope so an individual
/// entry can be re-wrapped (e.g. by a future rotation API)
/// without rotating the outer master key.
#[derive(Serialize, Deserialize)]
pub struct VaultContents {
    pub version: u32,
    pub created_at: i64,
    pub updated_at: i64,
    pub entries: BTreeMap<SecretKey, EntryEnvelope>,
}

impl VaultContents {
    pub fn new(now: i64) -> Self {
        VaultContents {
            version: 1,
            created_at: now,
            updated_at: now,
            entries: BTreeMap::new(),
        }
    }

    /// Drop every envelope and zeroize the wrapping/cipher buffers
    /// they hold. Called on lock / drop so a locked vault leaks
    /// neither plaintext nor ciphertext-keyed-on-master.
    pub fn zeroize_entries(&mut self) {
        for v in self.entries.values_mut() {
            v.zeroize();
        }
        self.entries.clear();
    }
}

// ---------------------------------------------------------------------------
// Sealing helpers (per-entry DEK + value AEAD).
//
// The outer file AEAD lives in `vault.rs`; everything below works
// inside the v4 payload (master key → wrap DEK → seal value).
//
// Fixed-size crypto inputs (32-byte keys, 12-byte nonces, 16-byte
// tags) are converted into the AEAD's `Key` / `Nonce` / `Tag` types
// via the non-deprecated `From<&[u8; N]>` / `TryFrom<&[u8]>` paths
// rather than the now-deprecated `GenericArray::from_slice`. Slices
// whose length is guaranteed by the wire-format checks above are
// converted with `.try_into().expect(...)`, where the `expect` is
// structurally unreachable given those length guards.
// ---------------------------------------------------------------------------

fn gen_entry_nonce() -> [u8; ENTRY_NONCE_LEN] {
    let mut n = [0u8; ENTRY_NONCE_LEN];
    rand::rng().fill_bytes(&mut n);
    n
}

/// Wrap a freshly-generated 32-byte DEK under the master key.
/// Returns `nonce(12) || ciphertext(32) || tag(16)` = 60 bytes.
fn wrap_dek_under_master(
    master: &[u8; KEY_LEN],
    dek: &[u8; KEY_LEN],
) -> Result<Vec<u8>, VaultError> {
    let cipher = ChaCha20Poly1305::new(<&Key>::from(master));
    let mut nonce_bytes = [0u8; ENTRY_NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let mut buf = dek.to_vec();
    let tag = cipher
        .encrypt_in_place_detached(&Nonce::from(nonce_bytes), &[], &mut buf)
        .map_err(|e| VaultError::Integrity(format!("wrap dek: {e}")))?;
    let mut out = Vec::with_capacity(ENTRY_NONCE_LEN + KEY_LEN + 16);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&buf);
    out.extend_from_slice(&tag);
    buf.zeroize();
    Ok(out)
}

/// Inverse of [`wrap_dek_under_master`]. Returns
/// [`VaultError::Auth`] on any AEAD tag mismatch — same opacity
/// posture as the outer-file AEAD: callers can't distinguish "wrong
/// master key" from "tampered wrapped DEK".
fn unwrap_dek_under_master(
    master: &[u8; KEY_LEN],
    wrapped: &[u8],
) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
    const MIN_WRAPPED: usize = ENTRY_NONCE_LEN + KEY_LEN + 16;
    if wrapped.len() != MIN_WRAPPED {
        return Err(VaultError::Integrity(format!(
            "wrapped DEK length {} != expected {MIN_WRAPPED}",
            wrapped.len()
        )));
    }
    let nonce = &wrapped[..ENTRY_NONCE_LEN];
    let ct_end = ENTRY_NONCE_LEN + KEY_LEN;
    let ciphertext = &wrapped[ENTRY_NONCE_LEN..ct_end];
    let tag = &wrapped[ct_end..];
    let cipher = ChaCha20Poly1305::new(<&Key>::from(master));
    let mut buf = ciphertext.to_vec();
    cipher
        .decrypt_in_place_detached(
            // nonce is exactly ENTRY_NONCE_LEN bytes: structurally guaranteed
            // by the MIN_WRAPPED length check above.
            <&Nonce>::from(nonce),
            &[],
            &mut buf,
            // tag is exactly 16 bytes: structurally guaranteed by
            // the MIN_WRAPPED length check above.
            <&Tag>::from(tag),
        )
        .map_err(|_| VaultError::Auth)?;
    let mut dek = [0u8; KEY_LEN];
    dek.copy_from_slice(&buf);
    buf.zeroize();
    Ok(Zeroizing::new(dek))
}

/// Deterministic AAD bytes for the per-entry AEAD. Re-derived on
/// both seal and open paths from the in-memory [`EntryMeta`]; any
/// tamper of the on-disk metadata fields makes the recomputed AAD
/// differ and the tag check fails. `serde_json` is the lazy choice
/// for determinism: serde_json with a stable struct field order
/// produces the same bytes on both ends.
fn aad_bytes(meta: &EntryMeta) -> Result<Vec<u8>, VaultError> {
    serde_json::to_vec(meta).map_err(|e| VaultError::Codec(e.to_string()))
}

/// Build a fresh [`EntryEnvelope`] for `(meta, value)` under the
/// supplied master key. Generates a per-entry DEK, wraps it under
/// the master key, seals the value under the DEK, and binds the
/// metadata into the AEAD via AAD.
pub fn seal_entry(
    master: &[u8; KEY_LEN],
    meta: EntryMeta,
    value: &[u8],
) -> Result<EntryEnvelope, VaultError> {
    let dek = gen_dek();
    let wrapped_dek = wrap_dek_under_master(master, &dek)?;

    let nonce = gen_entry_nonce();
    let aad = aad_bytes(&meta)?;
    let cipher = ChaCha20Poly1305::new(<&Key>::from(&*dek));
    let mut buf = value.to_vec();
    let tag = cipher
        .encrypt_in_place_detached(&Nonce::from(nonce), &aad, &mut buf)
        .map_err(|e| VaultError::Integrity(format!("seal entry: {e}")))?;
    let mut ciphertext = Vec::with_capacity(buf.len() + 16);
    ciphertext.extend_from_slice(&buf);
    ciphertext.extend_from_slice(&tag);
    buf.zeroize();

    Ok(EntryEnvelope {
        wrapped_dek,
        ciphertext,
        nonce,
        version: 1,
        meta,
    })
}

/// Inverse of [`seal_entry`]: unwrap the DEK with the master key,
/// then open the value ciphertext with that DEK. Returns the
/// recovered value in `Zeroizing` so the caller can drop it and
/// have the bytes wiped.
pub fn open_entry(
    master: &[u8; KEY_LEN],
    envelope: &EntryEnvelope,
) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    let dek = unwrap_dek_under_master(master, &envelope.wrapped_dek)?;
    // Per-entry ciphertext is `value_ct || tag(16)`; refuse anything
    // shorter than the tag length to fail-fast instead of feeding a
    // negative-length slice into the AEAD.
    if envelope.ciphertext.len() < 16 {
        return Err(VaultError::Integrity(
            "entry ciphertext shorter than AEAD tag".to_string(),
        ));
    }
    let split = envelope.ciphertext.len() - 16;
    let (value_ct, tag) = envelope.ciphertext.split_at(split);

    let aad = aad_bytes(&envelope.meta)?;
    let cipher = ChaCha20Poly1305::new(<&Key>::from(&*dek));
    let mut buf = value_ct.to_vec();
    cipher
        .decrypt_in_place_detached(
            &Nonce::from(envelope.nonce),
            &aad,
            &mut buf,
            // tag is exactly 16 bytes: structurally guaranteed by
            // the `envelope.ciphertext.len() < 16` guard above.
            <&Tag>::from(tag),
        )
        .map_err(|_| VaultError::Auth)?;
    Ok(Zeroizing::new(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_secret_kinds() -> [(SecretKind, &'static str); 7] {
        [
            (SecretKind::SshPrivateKey, "ssh-private-key"),
            (SecretKind::SshPublicKey, "ssh-public-key"),
            (SecretKind::ApiKey, "api-key"),
            (SecretKind::OauthToken, "oauth-token"),
            (SecretKind::Pem, "pem"),
            (SecretKind::Password, "password"),
            (SecretKind::Opaque, "opaque"),
        ]
    }

    fn fixture_meta(now_secs: i64) -> EntryMeta {
        EntryMeta {
            kind: SecretKind::ApiKey,
            created_at: now_secs,
            updated_at: now_secs,
            last_used_at: None,
            tags: vec!["env:test".to_string()],
            allowed_consumers: vec!["plugin".to_string()],
            created_at_utc: chrono::DateTime::<Utc>::from_timestamp(now_secs, 0)
                .unwrap_or_else(Utc::now),
            rotated_at_utc: chrono::DateTime::<Utc>::from_timestamp(now_secs, 0)
                .unwrap_or_else(Utc::now),
        }
    }

    #[test]
    fn secret_key_display_joins_service_and_name() {
        let key = SecretKey {
            service: "svc".to_string(),
            name: "token".to_string(),
        };
        assert_eq!(key.to_string(), "svc/token");
    }

    #[test]
    fn secret_kind_display_and_parse_cover_all_variants() {
        for (kind, label) in all_secret_kinds() {
            assert_eq!(kind.to_string(), label);
            assert_eq!(label.parse::<SecretKind>().unwrap(), kind);
        }

        assert_eq!(
            "SshPrivateKey".parse::<SecretKind>().unwrap(),
            SecretKind::SshPrivateKey
        );
        assert_eq!(
            "SshPublicKey".parse::<SecretKind>().unwrap(),
            SecretKind::SshPublicKey
        );
        assert_eq!("ApiKey".parse::<SecretKind>().unwrap(), SecretKind::ApiKey);
        assert_eq!(
            "OauthToken".parse::<SecretKind>().unwrap(),
            SecretKind::OauthToken
        );
        assert_eq!("Pem".parse::<SecretKind>().unwrap(), SecretKind::Pem);
        assert_eq!(
            "Password".parse::<SecretKind>().unwrap(),
            SecretKind::Password
        );
        assert_eq!("Opaque".parse::<SecretKind>().unwrap(), SecretKind::Opaque);
        assert_eq!(
            "not-a-kind".parse::<SecretKind>().unwrap_err(),
            "unknown kind: not-a-kind"
        );
    }

    #[test]
    fn secret_entry_zeroize_scrubs_value() {
        let mut entry = SecretEntry {
            kind: SecretKind::Opaque,
            value: b"secret".to_vec(),
            created_at: 1,
            updated_at: 1,
            last_used_at: None,
            tags: vec![],
            allowed_consumers: vec![],
        };
        entry.zeroize();
        assert!(entry.value.is_empty());
    }

    #[test]
    fn entry_envelope_zeroize_scrubs_buffers() {
        let mut envelope = EntryEnvelope {
            wrapped_dek: vec![1, 2, 3],
            ciphertext: vec![4, 5, 6],
            nonce: [7; ENTRY_NONCE_LEN],
            version: 9,
            meta: fixture_meta(1),
        };
        envelope.zeroize();
        assert!(envelope.wrapped_dek.is_empty());
        assert!(envelope.ciphertext.is_empty());
        assert_eq!(envelope.nonce, [0; ENTRY_NONCE_LEN]);
        assert_eq!(envelope.version, 0);
    }

    #[test]
    fn vault_contents_zeroize_entries_clears_map() {
        let master = [0x11u8; KEY_LEN];
        let mut contents = VaultContents::new(10);
        contents.entries.insert(
            SecretKey {
                service: "svc".to_string(),
                name: "name".to_string(),
            },
            seal_entry(&master, fixture_meta(10), b"value").unwrap(),
        );
        contents.zeroize_entries();
        assert!(contents.entries.is_empty());
    }

    #[test]
    fn secret_meta_conversions_preserve_fields() {
        let entry = SecretEntry {
            kind: SecretKind::Password,
            value: b"secret".to_vec(),
            created_at: 11,
            updated_at: 12,
            last_used_at: Some(13),
            tags: vec!["a".to_string()],
            allowed_consumers: vec!["b".to_string()],
        };
        let from_entry = SecretMeta::from(&entry);
        assert_eq!(from_entry.kind, SecretKind::Password);
        assert_eq!(from_entry.created_at, 11);
        assert_eq!(from_entry.updated_at, 12);
        assert_eq!(from_entry.last_used_at, Some(13));
        assert_eq!(from_entry.tags, vec!["a"]);
        assert_eq!(from_entry.allowed_consumers, vec!["b"]);

        let meta = fixture_meta(42);
        let from_meta = SecretMeta::from(&meta);
        assert_eq!(from_meta.kind, SecretKind::ApiKey);
        assert_eq!(from_meta.created_at, 42);
        assert_eq!(from_meta.updated_at, 42);
        assert_eq!(from_meta.last_used_at, None);
        assert_eq!(from_meta.tags, vec!["env:test"]);
        assert_eq!(from_meta.allowed_consumers, vec!["plugin"]);
    }

    #[test]
    fn seal_and_open_round_trips() {
        let master = [0x42u8; KEY_LEN];
        let meta = fixture_meta(1000);
        let env = seal_entry(&master, meta, b"plaintext-value").unwrap();
        let opened = open_entry(&master, &env).unwrap();
        assert_eq!(opened.as_slice(), b"plaintext-value");
    }

    #[test]
    fn open_with_wrong_master_fails_as_auth() {
        let alice = [0x01u8; KEY_LEN];
        let bob = [0x02u8; KEY_LEN];
        let env = seal_entry(&alice, fixture_meta(1), b"v").unwrap();
        let err = open_entry(&bob, &env).unwrap_err();
        assert!(matches!(err, VaultError::Auth), "got {err:?}");
    }

    #[test]
    fn tampered_metadata_invalidates_per_entry_tag() {
        // Flipping a metadata field on the envelope must invalidate
        // the per-entry AEAD via the recomputed AAD. Lower-layer
        // mechanism behind the per-entry-DEK property: the metadata
        // is bound into the AEAD, not just sealed alongside.
        let master = [0xC0u8; KEY_LEN];
        let mut env = seal_entry(&master, fixture_meta(1), b"v").unwrap();
        env.meta.tags.push("forged".to_string());
        let err = open_entry(&master, &env).unwrap_err();
        assert!(matches!(err, VaultError::Auth), "got {err:?}");
    }

    #[test]
    fn each_seal_uses_a_fresh_dek_and_nonce() {
        let master = [0x09u8; KEY_LEN];
        let a = seal_entry(&master, fixture_meta(1), b"same-value").unwrap();
        let b = seal_entry(&master, fixture_meta(1), b"same-value").unwrap();
        assert_ne!(a.wrapped_dek, b.wrapped_dek, "wrapped DEK must differ");
        assert_ne!(a.nonce, b.nonce, "entry nonce must differ");
        // Ciphertexts also differ even though plaintexts match,
        // because both DEK and nonce changed.
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    #[test]
    fn wrap_unwrap_dek_round_trip() {
        let master = [0xAAu8; KEY_LEN];
        let dek = [0x77u8; KEY_LEN];
        let wrapped = wrap_dek_under_master(&master, &dek).unwrap();
        // 12-byte nonce + 32-byte ciphertext + 16-byte tag = 60.
        assert_eq!(wrapped.len(), ENTRY_NONCE_LEN + KEY_LEN + 16);
        let recovered = unwrap_dek_under_master(&master, &wrapped).unwrap();
        assert_eq!(recovered.as_ref(), &dek);
    }

    #[test]
    fn unwrap_dek_rejects_wrong_length() {
        let master = [0xAAu8; KEY_LEN];
        let err = unwrap_dek_under_master(&master, &[0u8; 10]).unwrap_err();
        assert!(matches!(err, VaultError::Integrity(_)), "got {err:?}");
    }

    #[test]
    fn unwrap_dek_rejects_tampering_as_auth() {
        let master = [0x33u8; KEY_LEN];
        let dek = [0x55u8; KEY_LEN];
        let mut wrapped = wrap_dek_under_master(&master, &dek).unwrap();
        wrapped[ENTRY_NONCE_LEN] ^= 0x80;
        let err = unwrap_dek_under_master(&master, &wrapped).unwrap_err();
        assert!(matches!(err, VaultError::Auth), "got {err:?}");
    }

    #[test]
    fn open_entry_rejects_short_ciphertext() {
        let master = [0x22u8; KEY_LEN];
        let mut env = seal_entry(&master, fixture_meta(7), b"value").unwrap();
        env.ciphertext.clear();
        let err = open_entry(&master, &env).unwrap_err();
        assert!(matches!(err, VaultError::Integrity(_)), "got {err:?}");
    }

    #[test]
    fn open_entry_rejects_tampered_wrapped_dek() {
        let master = [0x44u8; KEY_LEN];
        let mut env = seal_entry(&master, fixture_meta(8), b"value").unwrap();
        env.wrapped_dek[0] ^= 0x01;
        let err = open_entry(&master, &env).unwrap_err();
        assert!(matches!(err, VaultError::Auth), "got {err:?}");
    }
}
