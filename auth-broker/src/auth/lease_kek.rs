//! `auth::lease_kek` — derive a per-lease KEK from the client's
//! bearer and use it to wrap/unwrap the OPAQUE SessionKey stored
//! in `lease.wrapped_export_key`.
//!
//! ## What this is for
//!
//! Every lease row carries `wrapped_export_key`, which is the
//! OPAQUE mutual SessionKey AEAD-sealed under a key the server can
//! reconstruct from request-time inputs. Previously the wrapping
//! key was server-global and lived in `Zeroizing<[u8; 32]>` for one
//! broker process lifetime — that meant (a) every broker restart
//! invalidated every active lease, and (b) an operator with full
//! server state could decrypt every active lease.
//!
//! This module replaces that design: the KEK is HKDF'd from the
//! bearer the client presents on every request. The bearer is
//! never on disk (the server stores only SHA-256(bearer) as the
//! lookup key), so an operator with postgres + vault disk cannot
//! derive any KEK. The bearer's loss is bounded to one lease's
//! lifetime; revocation works as today.
//!
//! ## Algorithm
//!
//! KEK derivation:
//!
//!   HKDF-SHA-512(
//!     ikm  = bearer_bytes,
//!     salt = b"auth-broker/lease-kek/v1",
//!     info = b"",
//!     L    = 32,
//!   )
//!
//! Wrapping layout:
//!
//!   [12  nonce]                  generated per wrap()
//!   [N   ciphertext]             length matches session_key length
//!   [16  tag]                    AEAD authentication tag
//!
//! The postgres `bytea` schema does not change; only the key source
//! does.

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use hkdf::Hkdf;
use rand::rngs::SysRng;
use rand::TryRng;
use sha2::Sha512;
use zeroize::Zeroizing;

pub const LEASE_KEK_LEN: usize = 32;
pub const LEASE_KEK_NONCE_LEN: usize = 12;
pub const LEASE_KEK_TAG_LEN: usize = 16;
pub const MIN_LEASE_WRAPPED_LEN: usize = LEASE_KEK_NONCE_LEN + LEASE_KEK_TAG_LEN;

const HKDF_SALT: &[u8] = b"auth-broker/lease-kek/v1";

#[derive(Debug, thiserror::Error)]
pub enum LeaseKekError {
    #[error("wrapped buffer is shorter than nonce + tag")]
    TooShort,
    #[error("AEAD decrypt failed")]
    Decrypt,
}

pub fn derive_lease_kek(bearer: &[u8]) -> Zeroizing<[u8; LEASE_KEK_LEN]> {
    let mut out = Zeroizing::new([0u8; LEASE_KEK_LEN]);
    let hk = Hkdf::<Sha512>::new(Some(HKDF_SALT), bearer);
    hk.expand(b"", out.as_mut())
        .expect("HKDF expand of 32 bytes is infallible");
    out
}

pub fn wrap_session_key(bearer: &[u8], session_key: &[u8]) -> Vec<u8> {
    let kek = derive_lease_kek(bearer);
    let cipher = ChaCha20Poly1305::new(<&Key>::from(&*kek));

    let mut nonce_bytes = [0u8; LEASE_KEK_NONCE_LEN];
    let mut rng = SysRng;
    rng.try_fill_bytes(&mut nonce_bytes)
        .expect("SysRng should be available");

    let mut buf = session_key.to_vec();
    let tag = cipher
        .encrypt_in_place_detached(&Nonce::from(nonce_bytes), &[], &mut buf)
        .expect("ChaCha20-Poly1305 encrypt is infallible for sub-2^32 inputs");

    let mut out = Vec::with_capacity(LEASE_KEK_NONCE_LEN + buf.len() + LEASE_KEK_TAG_LEN);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&buf);
    out.extend_from_slice(&tag);
    out
}

pub fn unwrap_session_key(
    bearer: &[u8],
    wrapped: &[u8],
) -> Result<Zeroizing<Vec<u8>>, LeaseKekError> {
    if wrapped.len() < MIN_LEASE_WRAPPED_LEN {
        return Err(LeaseKekError::TooShort);
    }
    let (nonce_bytes, rest) = wrapped.split_at(LEASE_KEK_NONCE_LEN);
    let (ciphertext, tag) = rest.split_at(rest.len() - LEASE_KEK_TAG_LEN);

    let kek = derive_lease_kek(bearer);
    let cipher = ChaCha20Poly1305::new(<&Key>::from(&*kek));
    let mut buf = ciphertext.to_vec();
    cipher
        .decrypt_in_place_detached(
            <&Nonce>::from(nonce_bytes),
            &[],
            &mut buf,
            <&Tag>::from(tag),
        )
        .map_err(|_| LeaseKekError::Decrypt)?;

    Ok(Zeroizing::new(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_unwrap_round_trips() {
        let bearer = b"some-bearer";
        let payload = b"some-session-key-bytes-pretend-this-is-64-bytes-of-key-material-";
        let wrapped = wrap_session_key(bearer, payload);
        let unwrapped = unwrap_session_key(bearer, &wrapped).expect("round-trip must succeed");
        assert_eq!(unwrapped.as_slice(), &payload[..]);
    }

    #[test]
    fn wrap_produces_unique_ciphertexts_for_the_same_inputs() {
        let bearer = b"same-bearer";
        let payload = b"identical-input";
        let a = wrap_session_key(bearer, payload);
        let b = wrap_session_key(bearer, payload);
        assert_ne!(a, b);
    }

    #[test]
    fn unwrap_with_different_bearer_fails() {
        let wrapped = wrap_session_key(b"bearer-a", b"session-key");
        let err = unwrap_session_key(b"bearer-b", &wrapped).expect_err("wrong bearer must fail");
        assert!(matches!(err, LeaseKekError::Decrypt), "got {err:?}");
    }

    #[test]
    fn unwrap_with_tampered_ciphertext_fails() {
        let mut wrapped = wrap_session_key(b"bearer", b"session-key");
        let mid =
            LEASE_KEK_NONCE_LEN + (wrapped.len() - LEASE_KEK_NONCE_LEN - LEASE_KEK_TAG_LEN) / 2;
        wrapped[mid] ^= 0x01;
        let err = unwrap_session_key(b"bearer", &wrapped).expect_err("tampered ciphertext");
        assert!(matches!(err, LeaseKekError::Decrypt), "got {err:?}");
    }

    #[test]
    fn unwrap_rejects_short_input() {
        for len in 0..MIN_LEASE_WRAPPED_LEN {
            let err = unwrap_session_key(b"bearer", &vec![0u8; len]).expect_err("short buffer");
            assert!(
                matches!(err, LeaseKekError::TooShort),
                "got {err:?} at len={len}"
            );
        }
        let err = unwrap_session_key(b"bearer", &[0u8; MIN_LEASE_WRAPPED_LEN])
            .expect_err("nonce+tag without ciphertext");
        assert!(matches!(err, LeaseKekError::Decrypt), "got {err:?}");
    }

    #[test]
    fn unwrap_yields_zeroizing_buffer() {
        fn assert_zeroizing<T: zeroize::Zeroize>(_: Zeroizing<T>) {}
        let wrapped = wrap_session_key(b"bearer", b"x");
        assert_zeroizing(unwrap_session_key(b"bearer", &wrapped).unwrap());
    }

    #[test]
    fn derive_lease_kek_is_deterministic() {
        assert_eq!(
            derive_lease_kek(b"x").as_ref(),
            derive_lease_kek(b"x").as_ref()
        );
    }

    #[test]
    fn derive_lease_kek_differs_across_bearers() {
        assert_ne!(
            derive_lease_kek(b"a").as_ref(),
            derive_lease_kek(b"b").as_ref()
        );
    }
}
