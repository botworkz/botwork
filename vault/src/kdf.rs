use hkdf::Hkdf;
use rand::Rng;
use sha2::Sha512;
use tracing::{debug, error};
use zeroize::Zeroizing;

use crate::error::VaultError;
use crate::PREFIX;

/// Length of the salt embedded in the v4 vault header. The salt is
/// generated once at vault create time and persisted in the header;
/// it's not a secret, only a per-vault uniqueness input to HKDF.
pub const SALT_LEN: usize = 16;
/// Length of every 256-bit key the vault crate handles (master
/// key, per-entry DEK).
pub const KEY_LEN: usize = 32;

/// Domain separation tag fed to HKDF when deriving the v4 master
/// key from the OPAQUE-supplied `export_key` bytes. Hard-coded so
/// any future caller that wants to re-derive the same key from the
/// same bytes lands on the same value without a configuration
/// dance.
pub const MASTER_INFO: &[u8] = b"botwork-vault/v4/master-key";

/// Generate a fresh per-vault salt. Called exactly once at
/// `Vault::create` time; subsequent writes reuse the salt that
/// landed in the file header.
pub fn gen_salt() -> [u8; SALT_LEN] {
    let mut salt = [0u8; SALT_LEN];
    rand::rng().fill_bytes(&mut salt);
    salt
}

/// Derive the v4 vault master key from the OPAQUE-supplied
/// `export_key` bytes, the per-vault salt, and the on-disk
/// `suite_version`. Uses HKDF-SHA-512 because the OPAQUE crate in
/// this workspace already produces 64-byte `export_key` outputs
/// from a SHA-512-based PAKE — staying on the same hash means the
/// vault's master-key derivation can't desync from the source
/// entropy on a future suite bump.
///
/// The `suite_version` byte is mixed into the HKDF `info` so a
/// future suite rotation produces a different master key from the
/// same input bytes; that's how v4's `Vault::unlock_master` detects
/// "operator scp'd over the right vault but the OPAQUE suite moved
/// underneath" and refuses to load cleanly instead of pretending to
/// decrypt and then failing the AEAD tag.
///
/// Returned in `Zeroizing` so the bytes are scrubbed when the
/// caller drops the result.
pub fn derive_master_key(
    export_key: &[u8],
    salt: &[u8; SALT_LEN],
    suite_version: u8,
) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
    if export_key.is_empty() {
        error!("{PREFIX} kdf: empty export_key");
        return Err(VaultError::Integrity(
            "empty export_key supplied to derive_master_key".to_string(),
        ));
    }
    debug!("{PREFIX} kdf: deriving master key suite_version={suite_version}");
    let hk = Hkdf::<Sha512>::new(Some(salt), export_key);
    // Bind the suite version into `info` so a future suite bump
    // forces a fresh derivation.
    let mut info = Vec::with_capacity(MASTER_INFO.len() + 1);
    info.extend_from_slice(MASTER_INFO);
    info.push(suite_version);
    let mut out = [0u8; KEY_LEN];
    hk.expand(&info, &mut out)
        .map_err(|e| VaultError::Integrity(format!("hkdf expand: {e}")))?;
    Ok(Zeroizing::new(out))
}

/// Generate a fresh 32-byte DEK from `OsRng`. Each entry stored in
/// the v4 payload has its own DEK; wrapping under the master key
/// keeps a memory dump after a single fetch from leaking other
/// entries.
pub fn gen_dek() -> Zeroizing<[u8; KEY_LEN]> {
    let mut dek = [0u8; KEY_LEN];
    rand::rng().fill_bytes(&mut dek);
    Zeroizing::new(dek)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_master_key_is_deterministic_for_same_inputs() {
        let salt = [7u8; SALT_LEN];
        let ek = b"deterministic-export-key-bytes-pretend-64-bytes-long-AAAAAAAAAAAAAAAA";
        let a = derive_master_key(ek, &salt, 1).unwrap();
        let b = derive_master_key(ek, &salt, 1).unwrap();
        assert_eq!(a.as_ref(), b.as_ref());
    }

    #[test]
    fn derive_master_key_differs_per_salt() {
        let ek = b"some-export-key";
        let a = derive_master_key(ek, &[1u8; SALT_LEN], 1).unwrap();
        let b = derive_master_key(ek, &[2u8; SALT_LEN], 1).unwrap();
        assert_ne!(a.as_ref(), b.as_ref());
    }

    #[test]
    fn derive_master_key_differs_per_suite_version() {
        let ek = b"some-export-key";
        let salt = [3u8; SALT_LEN];
        let a = derive_master_key(ek, &salt, 1).unwrap();
        let b = derive_master_key(ek, &salt, 2).unwrap();
        assert_ne!(a.as_ref(), b.as_ref());
    }

    #[test]
    fn derive_master_key_differs_per_export_key() {
        let salt = [3u8; SALT_LEN];
        let a = derive_master_key(b"ek-A", &salt, 1).unwrap();
        let b = derive_master_key(b"ek-B", &salt, 1).unwrap();
        assert_ne!(a.as_ref(), b.as_ref());
    }

    #[test]
    fn derive_master_key_rejects_empty_export_key() {
        let err = derive_master_key(b"", &[0u8; SALT_LEN], 1).unwrap_err();
        match err {
            VaultError::Integrity(msg) => assert!(msg.contains("empty export_key"), "got {msg}"),
            other => panic!("expected Integrity, got {other:?}"),
        }
    }

    #[test]
    fn gen_dek_produces_distinct_bytes_on_each_call() {
        let a = gen_dek();
        let b = gen_dek();
        assert_ne!(a.as_ref(), b.as_ref());
    }
}
