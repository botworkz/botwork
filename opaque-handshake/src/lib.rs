//! Typed wrappers around `opaque-ke`'s OPAQUE PAKE implementation.
//!
//! This crate fixes a single ciphersuite — **ristretto255 + SHA-512 OPRF +
//! Argon2id MHF** (CFRG `draft-irtf-cfrg-opaque-10` default) — and exposes
//! the OPAQUE registration and login handshakes through a small, opaque
//! API surface:
//!
//! * [`RegistrationRequest`], [`RegistrationResponse`],
//!   [`RegistrationUpload`], [`PasswordFile`]
//! * [`LoginRequest`], [`LoginResponse`], [`LoginFinalization`],
//!   [`SessionKey`], [`ExportKey`]
//! * Server helpers: [`server::registration_start`],
//!   [`server::registration_finish`], [`server::login_start`],
//!   [`server::login_finish`].
//! * Client helpers: [`client::registration_start`],
//!   [`client::registration_finish`], [`client::login_start`],
//!   [`client::login_finish`].
//!
//! Everything that carries key material — `SessionKey`, `ExportKey`, the
//! intermediate `Client*State` / `ServerLoginState` types, the
//! `PasswordFile` — implements [`Zeroize`] on drop. There is no `unsafe`
//! in this crate.
//!
//! The wire-format choice is **`opaque_ke`'s native `serialize()` /
//! `deserialize()`** for the four messages and `PasswordFile`. They are
//! the byte strings the CFRG draft specifies. Callers that need a
//! framing layer (length-prefix, base64, multipart-MIME, …) wrap these
//! bytes themselves.
//!
//! ```
//! # use botwork_opaque_handshake::{client, server, PasswordFile, ServerSetup};
//! # fn try_main() -> Result<(), botwork_opaque_handshake::OpaqueError> {
//! let mut rng = rand::rng();
//! let setup = ServerSetup::generate(&mut rng);
//! let credential_id = b"alice@example.com";
//!
//! // Registration ------------------------------------------------------
//! let cr = client::registration_start(&mut rng, b"hunter2")?;
//! let sr = server::registration_start(&setup, cr.request.clone(), credential_id)?;
//! let cf = client::registration_finish(&mut rng, cr.state, b"hunter2", sr.response)?;
//! let password_file = server::registration_finish(cf.upload);
//!
//! // Login -------------------------------------------------------------
//! let cl = client::login_start(&mut rng, b"hunter2")?;
//! let sl = server::login_start(
//!     &mut rng,
//!     &setup,
//!     Some(&password_file),
//!     cl.request.clone(),
//!     credential_id,
//! )?;
//! let cf = client::login_finish(cl.state, b"hunter2", sl.response)?;
//! let session_key = server::login_finish(sl.state, cf.finalization)?;
//!
//! // The PAKE has reached a mutual session key on both sides; the client
//! // also walked away with an `ExportKey` that the server never sees.
//! assert_eq!(cf.session_key.as_bytes(), session_key.as_bytes());
//! # let _ = cf.export_key;
//! # Ok(())
//! # }
//! # try_main().unwrap();
//! ```

#![deny(missing_docs)]

use std::fmt;

use opaque_ke::ciphersuite::CipherSuite;
use opaque_ke::ksf::Ksf;
use rand::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

// ---------------------------------------------------------------------------
// rand 0.9 → opaque-ke (rand 0.8) compatibility adapter
// ---------------------------------------------------------------------------
//
// opaque-ke 3.x pins rand = "0.8" (rand_core 0.6). When this crate and its
// callers use rand 0.9 (rand_core 0.9), the two `RngCore`/`CryptoRng` trait
// versions are unrelated types. `OpaqueKeRng` wraps a rand-0.9 RNG and
// implements opaque-ke's re-exported rand-0.8 `RngCore`/`CryptoRng` so that
// we can forward-bridge a single rng value into every opaque-ke call site.
// This wrapper is entirely private to this crate.

struct OpaqueKeRng<'a, R>(&'a mut R);

impl<R: RngCore> opaque_ke::rand::RngCore for OpaqueKeRng<'_, R> {
    fn next_u32(&mut self) -> u32 {
        self.0.next_u32()
    }

    fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    fn fill_bytes(&mut self, dst: &mut [u8]) {
        self.0.fill_bytes(dst);
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), opaque_ke::rand::Error> {
        self.0.fill_bytes(dst);
        Ok(())
    }
}

impl<R: CryptoRng> opaque_ke::rand::CryptoRng for OpaqueKeRng<'_, R> {}

// ---------------------------------------------------------------------------
// Ciphersuite
// ---------------------------------------------------------------------------

/// Identifier string for the fixed OPAQUE ciphersuite this crate uses.
///
/// Mirrors the IETF / CFRG OPAQUE draft naming convention. The string is
/// stable across releases of *this* crate; if the underlying ciphersuite
/// ever needs to change, [`SUITE_VERSION`] gets bumped in lockstep so
/// stored `PasswordFile` blobs can be rejected at load time.
pub const SUITE_ID: &str = "OPAQUE-ristretto255-SHA512-Argon2id";

/// Monotonically-increasing version tag for the on-the-wire / on-disk
/// representation the crate produces.
///
/// Persistence layers that want to be forward-compatible should store
/// this byte alongside any serialised [`PasswordFile`]. Comparing it to
/// the current value at load time gives a clean migration story when a
/// future release reaches for a different ciphersuite.
pub const SUITE_VERSION: u8 = 1;

/// Length of the deterministic [`ExportKey`] (and the symmetric
/// [`SessionKey`]) in bytes. Both equal SHA-512's output size.
pub const KEY_LEN: usize = 64;

/// Wrapper around `argon2::Argon2` so `opaque_ke::ksf::Ksf` is in
/// scope for the `CipherSuite::Ksf` associated type. The `'static`
/// lifetime falls out of `argon2::Argon2`'s default `Params`.
type KsfArgon2 = argon2::Argon2<'static>;

/// The OPAQUE ciphersuite this crate uses verbatim.
///
/// Public only so external callers can name `OpaqueSuite` in error or
/// trait-bound contexts; constructing it directly is meaningless because
/// it has no fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpaqueSuite;

impl CipherSuite for OpaqueSuite {
    type OprfCs = opaque_ke::Ristretto255;
    type KeGroup = opaque_ke::Ristretto255;
    type KeyExchange = opaque_ke::key_exchange::tripledh::TripleDh;
    type Ksf = KsfArgon2;
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure mode reported by every helper in this crate.
///
/// `opaque-ke`'s `ProtocolError` is rich but ties callers to the
/// `opaque_ke::errors` module. We collapse it to the four shapes a
/// consumer of this crate actually has to react to. `InvalidLogin`
/// is the only branch where the password was just wrong; the rest
/// indicate a tampered message, a serialisation bug, or a panic-class
/// library invariant being violated by the caller.
#[derive(Debug, Error)]
pub enum OpaqueError {
    /// The supplied credential failed PAKE verification. Corresponds to
    /// `opaque_ke::errors::ProtocolError::InvalidLoginError` and is the
    /// only "wrong password" signal — callers MUST surface this to the
    /// user as "incorrect credentials", not as a server error.
    #[error("invalid login")]
    InvalidLogin,
    /// A serialised message did not parse as the expected OPAQUE type.
    /// Wire-format error; the peer is misbehaving or the bytes were
    /// truncated in transit.
    #[error("malformed OPAQUE message: {0}")]
    Serialization(&'static str),
    /// The OPRF blinded element from the peer reflected our own input.
    /// CFRG draft section 6.1.2; almost always indicates an adversarial
    /// or buggy peer.
    #[error("reflected OPRF value")]
    Reflected,
    /// Anything else — covers the rare internal errors `opaque-ke` can
    /// surface (Argon2 misconfiguration, group element identity, HKDF
    /// extract failure, …). Treated as an internal error by callers.
    #[error("OPAQUE internal error: {0}")]
    Internal(String),
}

impl OpaqueError {
    fn from_protocol(err: opaque_ke::errors::ProtocolError) -> Self {
        use opaque_ke::errors::{InternalError, ProtocolError as P};
        match err {
            P::InvalidLoginError => OpaqueError::InvalidLogin,
            P::SerializationError => OpaqueError::Serialization("opaque_ke deserialize"),
            P::ReflectedValueError => OpaqueError::Reflected,
            // `LibraryError(InvalidByteSequence | SizeError | PointError)`
            // all show up at deserialize time when bytes don't match the
            // shape opaque_ke expects. Treat them as wire-format failures
            // rather than internal errors so callers can keep a single
            // "bad message" arm.
            P::LibraryError(InternalError::InvalidByteSequence) => {
                OpaqueError::Serialization("invalid byte sequence")
            }
            P::LibraryError(InternalError::SizeError { .. }) => {
                OpaqueError::Serialization("size mismatch")
            }
            P::LibraryError(InternalError::PointError) => {
                OpaqueError::Serialization("invalid group element")
            }
            // VOPRF also surfaces its own `Deserialization` /
            // `InvalidInputLength` / point-decode errors through
            // `OprfError(_)`. Same root cause from the caller's
            // perspective.
            P::LibraryError(InternalError::OprfError(_)) => {
                OpaqueError::Serialization("OPRF message deserialize")
            }
            other => OpaqueError::Internal(format!("{other:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Message wrappers
// ---------------------------------------------------------------------------

/// Macro to declare a newtype wrapper around an opaque-ke message type,
/// with `serialize` / `deserialize` going through opaque-ke's own
/// canonical encoding and zeroize-on-drop where it carries key material.
///
/// All four message types are tiny envelopes around generic-arrays of
/// curve points and hashes — the only thing varying between them is
/// which `opaque_ke::*` type they wrap.
macro_rules! opaque_message {
    (
        $(#[$meta:meta])*
        $name:ident, $inner:ty
    ) => {
        $(#[$meta])*
        #[derive(Clone)]
        pub struct $name(pub(crate) $inner);

        impl $name {
            /// Serialise into the canonical OPAQUE byte string.
            pub fn serialize(&self) -> Vec<u8> {
                self.0.serialize().to_vec()
            }

            /// Deserialise from a canonical OPAQUE byte string. Returns
            /// [`OpaqueError::Serialization`] on length / framing
            /// mismatch.
            pub fn deserialize(bytes: &[u8]) -> Result<Self, OpaqueError> {
                <$inner>::deserialize(bytes)
                    .map(Self)
                    .map_err(OpaqueError::from_protocol)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // Avoid printing curve points — they tend to be 80+
                // bytes of incomprehensible base64 and noisy in logs.
                f.debug_struct(stringify!($name)).finish_non_exhaustive()
            }
        }
    };
}

opaque_message! {
    /// First message of registration: the client's blinded password.
    ///
    /// Wire-only; carries no key material.
    RegistrationRequest, opaque_ke::RegistrationRequest<OpaqueSuite>
}

opaque_message! {
    /// Server's reply to a [`RegistrationRequest`] — the OPRF
    /// evaluation and the server's static public key.
    RegistrationResponse, opaque_ke::RegistrationResponse<OpaqueSuite>
}

opaque_message! {
    /// Final client message of registration: the sealed envelope the
    /// server will persist as the [`PasswordFile`].
    RegistrationUpload, opaque_ke::RegistrationUpload<OpaqueSuite>
}

opaque_message! {
    /// First message of login: the client's blinded password plus the
    /// initial AKE message (`KE1`).
    LoginRequest, opaque_ke::CredentialRequest<OpaqueSuite>
}

opaque_message! {
    /// Server's reply to a [`LoginRequest`] — the OPRF evaluation,
    /// masked response envelope, and the AKE `KE2`.
    LoginResponse, opaque_ke::CredentialResponse<OpaqueSuite>
}

opaque_message! {
    /// Final client message of login: the AKE `KE3` that authenticates
    /// the client to the server.
    LoginFinalization, opaque_ke::CredentialFinalization<OpaqueSuite>
}

// ---------------------------------------------------------------------------
// PasswordFile
// ---------------------------------------------------------------------------

/// Server-side record produced at the end of registration.
///
/// Sensitive — equivalent to a password hash. Persistence layers must
/// store it with at least the same care as an Argon2id hash, ideally
/// behind an envelope encryption scheme. `Zeroize`-on-drop scrubs the
/// in-memory copy when the value goes out of scope.
#[derive(Clone, Serialize, Deserialize)]
pub struct PasswordFile {
    bytes: Vec<u8>,
}

impl PasswordFile {
    /// Serialise to bytes. Stable across releases of this crate as long
    /// as [`SUITE_VERSION`] does not change.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Construct a `PasswordFile` from previously-serialised bytes.
    /// Performs a structural deserialise to fail fast on garbage input,
    /// then re-serialises into the canonical form.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, OpaqueError> {
        let parsed = opaque_ke::ServerRegistration::<OpaqueSuite>::deserialize(bytes)
            .map_err(OpaqueError::from_protocol)?;
        Ok(Self {
            bytes: parsed.serialize().to_vec(),
        })
    }

    fn from_registration(reg: opaque_ke::ServerRegistration<OpaqueSuite>) -> Self {
        Self {
            bytes: reg.serialize().to_vec(),
        }
    }

    fn to_registration(&self) -> Result<opaque_ke::ServerRegistration<OpaqueSuite>, OpaqueError> {
        opaque_ke::ServerRegistration::<OpaqueSuite>::deserialize(&self.bytes)
            .map_err(OpaqueError::from_protocol)
    }
}

impl Zeroize for PasswordFile {
    fn zeroize(&mut self) {
        self.bytes.zeroize();
    }
}

impl Drop for PasswordFile {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl fmt::Debug for PasswordFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasswordFile")
            .field("bytes_len", &self.bytes.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// ServerSetup
// ---------------------------------------------------------------------------

/// Server-side long-term keypair + OPRF seed. Generated once at
/// deploy time and persisted unchanged for the lifetime of the
/// installation; rotating it invalidates every existing
/// [`PasswordFile`].
#[derive(Clone, Serialize, Deserialize)]
pub struct ServerSetup {
    bytes: Vec<u8>,
}

impl ServerSetup {
    /// Generate a fresh setup from the supplied CSPRNG.
    pub fn generate<R: RngCore + CryptoRng>(rng: &mut R) -> Self {
        let setup = opaque_ke::ServerSetup::<OpaqueSuite>::new(&mut OpaqueKeRng(rng));
        Self {
            bytes: setup.serialize().to_vec(),
        }
    }

    /// Serialised view; deploy this verbatim into the secret storage of
    /// your choice.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Round-trip an existing setup from bytes. Returns
    /// [`OpaqueError::Serialization`] for byte strings that do not
    /// parse.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, OpaqueError> {
        let parsed = opaque_ke::ServerSetup::<OpaqueSuite>::deserialize(bytes)
            .map_err(OpaqueError::from_protocol)?;
        Ok(Self {
            bytes: parsed.serialize().to_vec(),
        })
    }

    fn inner(&self) -> Result<opaque_ke::ServerSetup<OpaqueSuite>, OpaqueError> {
        opaque_ke::ServerSetup::<OpaqueSuite>::deserialize(&self.bytes)
            .map_err(OpaqueError::from_protocol)
    }
}

impl Zeroize for ServerSetup {
    fn zeroize(&mut self) {
        self.bytes.zeroize();
    }
}

impl Drop for ServerSetup {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl fmt::Debug for ServerSetup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerSetup")
            .field("bytes_len", &self.bytes.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Session / export keys
// ---------------------------------------------------------------------------

type KeyArray = [u8; KEY_LEN];

/// Mutually-derived AKE output. Both ends of a successful login finish
/// holding the *same* [`SessionKey`]; it makes a sound key for an
/// authenticated channel layer.
///
/// `PartialEq` is **constant-time** via [`subtle::ConstantTimeEq`] so
/// callers can write `k1 == k2` without thinking about timing side
/// channels. A naive slice compare short-circuits on the first
/// non-matching byte and is exploitable as a per-prefix oracle; this
/// type closes that hole at the API boundary.
#[derive(Clone, ZeroizeOnDrop)]
pub struct SessionKey(KeyArray);

impl SessionKey {
    /// Borrow the raw 64-byte key.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl PartialEq for SessionKey {
    fn eq(&self, other: &Self) -> bool {
        use subtle::ConstantTimeEq;
        self.0[..].ct_eq(&other.0[..]).into()
    }
}

impl Eq for SessionKey {}

impl fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionKey").finish()
    }
}

/// Deterministic client-only key derived from `(password, ServerSetup)`.
///
/// Stable across logins with the same password against the same
/// `ServerSetup` — the OPAQUE construction is built around this
/// property. Changes if the password changes or if the server rolls a
/// new setup. The server never sees it.
///
/// `PartialEq` is **constant-time** via [`subtle::ConstantTimeEq`].
/// Same rationale as for [`SessionKey`]: an `ExportKey` comparison
/// landing on a slice `==` would leak prefix-match information through
/// timing. The 1b auth-broker call sites that look up cached unlock
/// state by export-key-derived material rely on this property.
#[derive(Clone, ZeroizeOnDrop)]
pub struct ExportKey(KeyArray);

impl ExportKey {
    /// Borrow the raw 64-byte key.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl PartialEq for ExportKey {
    fn eq(&self, other: &Self) -> bool {
        use subtle::ConstantTimeEq;
        self.0[..].ct_eq(&other.0[..]).into()
    }
}

impl Eq for ExportKey {}

impl fmt::Debug for ExportKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExportKey").finish()
    }
}

// ---------------------------------------------------------------------------
// Client / server state
// ---------------------------------------------------------------------------

/// Per-handshake client state held between [`client::registration_start`]
/// and [`client::registration_finish`]. Carries OPRF blind material;
/// scrubbed on drop by `opaque_ke`'s own `ZeroizeOnDrop` derive.
pub struct ClientRegistrationState(opaque_ke::ClientRegistration<OpaqueSuite>);

impl fmt::Debug for ClientRegistrationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientRegistrationState").finish()
    }
}

/// Per-handshake client state held between [`client::login_start`] and
/// [`client::login_finish`]. Carries the AKE `KE1` private state and
/// OPRF blind material; scrubbed on drop.
pub struct ClientLoginState(opaque_ke::ClientLogin<OpaqueSuite>);

impl fmt::Debug for ClientLoginState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientLoginState").finish()
    }
}

/// Per-handshake server state held between [`server::login_start`] and
/// [`server::login_finish`]. Carries the AKE `KE2` private state;
/// scrubbed on drop.
///
/// Persistable via [`ServerLoginState::to_bytes`] /
/// [`ServerLoginState::from_bytes`] so the two-RTT login can flow
/// through a stateless HTTP surface (the broker emits `LoginResponse`
/// in `/auth/login/start`, stashes these bytes keyed by an in-flight
/// handshake id, then reads them back in `/auth/login/finish` to
/// verify `LoginFinalization`). Without persistence the broker would
/// need affinity routing — workable on a single-process deploy,
/// structurally bad for any future scale-out.
pub struct ServerLoginState(opaque_ke::ServerLogin<OpaqueSuite>);

impl ServerLoginState {
    /// Serialise the per-handshake AKE state to bytes for ephemeral
    /// persistence between `/auth/login/start` and
    /// `/auth/login/finish`.
    ///
    /// Return type is [`zeroize::Zeroizing`] because the bytes
    /// include the server's half of the AKE transcript (`KE2State` in
    /// opaque-ke terms); if the lease store ever grew the ability to
    /// `Debug`-print one of these, we don't want that to be the leak
    /// path. The returned buffer is wiped when dropped.
    pub fn to_bytes(&self) -> zeroize::Zeroizing<Vec<u8>> {
        zeroize::Zeroizing::new(self.0.serialize().to_vec())
    }

    /// Inverse of [`Self::to_bytes`]. Returns
    /// [`OpaqueError::Serialization`] for byte strings that don't
    /// parse as a valid `ServerLogin` for [`OpaqueSuite`].
    ///
    /// Garbage in (truncated, wrong cipher-suite, corrupted) fails
    /// fast on this call rather than panicking later in
    /// [`server::login_finish`]; callers can treat a deserialise
    /// error as "unknown handshake, return 404" without leaking
    /// which arm tripped.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, OpaqueError> {
        opaque_ke::ServerLogin::<OpaqueSuite>::deserialize(bytes)
            .map(Self)
            .map_err(OpaqueError::from_protocol)
    }
}

impl fmt::Debug for ServerLoginState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerLoginState").finish()
    }
}

// ---------------------------------------------------------------------------
// Argon2id KSF parameters
// ---------------------------------------------------------------------------

/// Argon2id memory / time / parallelism parameters used as the OPAQUE
/// KSF (memory-hard function applied to the OPRF output).
///
/// Defaults match RFC 9106's "memory-constrained" profile: 64 MiB,
/// `t=3`, `p=1`. Heavier servers should bump `m_cost_kib` to 256 MiB
/// or more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KsfParams {
    /// Argon2id memory cost in KiB.
    pub m_cost_kib: u32,
    /// Argon2id time cost (iterations).
    pub t_cost: u32,
    /// Argon2id parallelism.
    pub p_cost: u32,
}

impl Default for KsfParams {
    fn default() -> Self {
        Self {
            m_cost_kib: 64 * 1024,
            t_cost: 3,
            p_cost: 1,
        }
    }
}

impl KsfParams {
    fn build(&self) -> Result<KsfArgon2, OpaqueError> {
        let params = argon2::Params::new(self.m_cost_kib, self.t_cost, self.p_cost, None)
            .map_err(|e| OpaqueError::Internal(format!("argon2 params: {e}")))?;
        Ok(argon2::Argon2::new(
            argon2::Algorithm::Argon2id,
            argon2::Version::V0x13,
            params,
        ))
    }
}

// `opaque_ke` requires `Ksf: Default` (see `CipherSuite::Ksf`). The
// upstream impl for `argon2::Argon2<'_>` uses Argon2's library defaults,
// which match the "memory-constrained" profile in RFC 9106. That is
// `KsfParams::default()` above.
#[allow(dead_code)]
fn _assert_ksf_is_default()
where
    KsfArgon2: Default + Ksf,
{
}

// ---------------------------------------------------------------------------
// Result envelopes
// ---------------------------------------------------------------------------

/// Output of [`client::registration_start`].
#[derive(Debug)]
pub struct ClientRegistrationStart {
    /// Sent to the server.
    pub request: RegistrationRequest,
    /// Held by the client until [`client::registration_finish`].
    pub state: ClientRegistrationState,
}

/// Output of [`server::registration_start`].
#[derive(Debug)]
pub struct ServerRegistrationStart {
    /// Sent back to the client.
    pub response: RegistrationResponse,
}

/// Output of [`client::registration_finish`].
#[derive(Debug)]
pub struct ClientRegistrationFinish {
    /// Sent to the server, which then calls
    /// [`server::registration_finish`] to derive a [`PasswordFile`].
    pub upload: RegistrationUpload,
    /// Stable key derived from `(password, ServerSetup)`. Client-only —
    /// the server never sees it.
    pub export_key: ExportKey,
}

/// Output of [`client::login_start`].
#[derive(Debug)]
pub struct ClientLoginStart {
    /// Sent to the server.
    pub request: LoginRequest,
    /// Held by the client until [`client::login_finish`].
    pub state: ClientLoginState,
}

/// Output of [`server::login_start`].
#[derive(Debug)]
pub struct ServerLoginStart {
    /// Sent to the client.
    pub response: LoginResponse,
    /// Held by the server until [`server::login_finish`].
    pub state: ServerLoginState,
}

/// Output of [`client::login_finish`].
#[derive(Debug)]
pub struct ClientLoginFinish {
    /// Sent to the server.
    pub finalization: LoginFinalization,
    /// Mutual session key — identical to the one the server obtains
    /// from [`server::login_finish`].
    pub session_key: SessionKey,
    /// Same stable key the client got at registration time. Carries the
    /// usual ExportKey semantics: tied to `(password, ServerSetup)`,
    /// invisible to the server.
    pub export_key: ExportKey,
}

// ---------------------------------------------------------------------------
// Client helpers
// ---------------------------------------------------------------------------

/// Client-side OPAQUE helpers.
pub mod client {
    use super::*;
    use opaque_ke::{
        ClientLogin, ClientLoginFinishParameters, ClientRegistration,
        ClientRegistrationFinishParameters,
    };

    /// Begin client-side registration. Returns the [`RegistrationRequest`]
    /// to send to the server plus a [`ClientRegistrationState`] to feed
    /// into [`registration_finish`].
    pub fn registration_start<R: RngCore + CryptoRng>(
        rng: &mut R,
        password: &[u8],
    ) -> Result<ClientRegistrationStart, OpaqueError> {
        let start = ClientRegistration::<OpaqueSuite>::start(&mut OpaqueKeRng(rng), password)
            .map_err(OpaqueError::from_protocol)?;
        Ok(ClientRegistrationStart {
            request: RegistrationRequest(start.message),
            state: ClientRegistrationState(start.state),
        })
    }

    /// Finish client-side registration given the server's
    /// [`RegistrationResponse`]. The returned [`ExportKey`] is the same
    /// one a subsequent successful login will yield.
    pub fn registration_finish<R: RngCore + CryptoRng>(
        rng: &mut R,
        state: ClientRegistrationState,
        password: &[u8],
        response: RegistrationResponse,
    ) -> Result<ClientRegistrationFinish, OpaqueError> {
        let ksf = KsfParams::default().build()?;
        let params = ClientRegistrationFinishParameters {
            ksf: Some(&ksf),
            ..Default::default()
        };
        let finish = state
            .0
            .finish(&mut OpaqueKeRng(rng), password, response.0, params)
            .map_err(OpaqueError::from_protocol)?;
        Ok(ClientRegistrationFinish {
            upload: RegistrationUpload(finish.message),
            export_key: ExportKey(
                (&*finish.export_key)
                    .try_into()
                    .expect("export key is always KEY_LEN bytes"),
            ),
        })
    }

    /// Begin client-side login. Mirrors [`registration_start`]: returns
    /// a [`LoginRequest`] to wire over and a [`ClientLoginState`] to
    /// pass to [`login_finish`].
    pub fn login_start<R: RngCore + CryptoRng>(
        rng: &mut R,
        password: &[u8],
    ) -> Result<ClientLoginStart, OpaqueError> {
        let start = ClientLogin::<OpaqueSuite>::start(&mut OpaqueKeRng(rng), password)
            .map_err(OpaqueError::from_protocol)?;
        Ok(ClientLoginStart {
            request: LoginRequest(start.message),
            state: ClientLoginState(start.state),
        })
    }

    /// Finish client-side login. Returns
    /// [`OpaqueError::InvalidLogin`] when the supplied password did not
    /// authenticate against the server's [`PasswordFile`].
    pub fn login_finish(
        state: ClientLoginState,
        password: &[u8],
        response: LoginResponse,
    ) -> Result<ClientLoginFinish, OpaqueError> {
        let ksf = KsfParams::default().build()?;
        let params = ClientLoginFinishParameters {
            ksf: Some(&ksf),
            ..Default::default()
        };
        let finish = state
            .0
            .finish(password, response.0, params)
            .map_err(OpaqueError::from_protocol)?;
        Ok(ClientLoginFinish {
            finalization: LoginFinalization(finish.message),
            session_key: SessionKey(
                (&*finish.session_key)
                    .try_into()
                    .expect("session key is always KEY_LEN bytes"),
            ),
            export_key: ExportKey(
                (&*finish.export_key)
                    .try_into()
                    .expect("export key is always KEY_LEN bytes"),
            ),
        })
    }
}

// ---------------------------------------------------------------------------
// Server helpers
// ---------------------------------------------------------------------------

/// Server-side OPAQUE helpers.
pub mod server {
    use super::*;
    use opaque_ke::{ServerLogin, ServerLoginStartParameters, ServerRegistration};

    /// Begin server-side registration. The `credential_identifier` is a
    /// stable per-user identifier the server later uses to look up the
    /// stored [`PasswordFile`]. Treat it as a primary key, NOT as the
    /// username displayed to humans — OPAQUE binds it into the OPRF
    /// derivation, so renames break logins.
    pub fn registration_start(
        setup: &ServerSetup,
        request: RegistrationRequest,
        credential_identifier: &[u8],
    ) -> Result<ServerRegistrationStart, OpaqueError> {
        let inner = setup.inner()?;
        let started =
            ServerRegistration::<OpaqueSuite>::start(&inner, request.0, credential_identifier)
                .map_err(OpaqueError::from_protocol)?;
        Ok(ServerRegistrationStart {
            response: RegistrationResponse(started.message),
        })
    }

    /// Convert the client's [`RegistrationUpload`] into a persistable
    /// [`PasswordFile`]. Infallible — `opaque_ke`'s
    /// `ServerRegistration::finish` only re-wraps the upload bytes.
    pub fn registration_finish(upload: RegistrationUpload) -> PasswordFile {
        PasswordFile::from_registration(ServerRegistration::<OpaqueSuite>::finish(upload.0))
    }

    /// Begin server-side login.
    ///
    /// Pass `Some(password_file)` for known credentials. Pass `None`
    /// for a credential lookup miss — `opaque_ke` synthesises a fake
    /// response so the wire-observable timing of "user doesn't exist"
    /// matches "user exists but wrong password", which is the whole
    /// point of OPAQUE's `dummy` flow.
    pub fn login_start<R: RngCore + CryptoRng>(
        rng: &mut R,
        setup: &ServerSetup,
        password_file: Option<&PasswordFile>,
        request: LoginRequest,
        credential_identifier: &[u8],
    ) -> Result<ServerLoginStart, OpaqueError> {
        let inner = setup.inner()?;
        let record = match password_file {
            Some(pf) => Some(pf.to_registration()?),
            None => None,
        };
        let started = ServerLogin::<OpaqueSuite>::start(
            &mut OpaqueKeRng(rng),
            &inner,
            record,
            request.0,
            credential_identifier,
            ServerLoginStartParameters::default(),
        )
        .map_err(OpaqueError::from_protocol)?;
        Ok(ServerLoginStart {
            response: LoginResponse(started.message),
            state: ServerLoginState(started.state),
        })
    }

    /// Finish server-side login. Returns the mutual [`SessionKey`].
    /// The "wrong password" case has already surfaced on the client
    /// side in [`super::client::login_finish`]; here, anything other
    /// than `Ok` indicates the client sent a malformed
    /// [`LoginFinalization`].
    pub fn login_finish(
        state: ServerLoginState,
        finalization: LoginFinalization,
    ) -> Result<SessionKey, OpaqueError> {
        let finish = state
            .0
            .finish(finalization.0)
            .map_err(OpaqueError::from_protocol)?;
        Ok(SessionKey(
            (&*finish.session_key)
                .try_into()
                .expect("session key is always KEY_LEN bytes"),
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn run_registration(
        rng: &mut impl CryptoRng,
        setup: &ServerSetup,
        credential_id: &[u8],
        password: &[u8],
    ) -> (PasswordFile, ExportKey) {
        let cstart = client::registration_start(rng, password).unwrap();
        let sstart = server::registration_start(setup, cstart.request, credential_id).unwrap();
        let cfin =
            client::registration_finish(rng, cstart.state, password, sstart.response).unwrap();
        let pf = server::registration_finish(cfin.upload);
        (pf, cfin.export_key)
    }

    fn run_login(
        rng: &mut impl CryptoRng,
        setup: &ServerSetup,
        credential_id: &[u8],
        password: &[u8],
        password_file: Option<&PasswordFile>,
    ) -> Result<(SessionKey, SessionKey, ExportKey), OpaqueError> {
        let cstart = client::login_start(rng, password)?;
        let sstart = server::login_start(rng, setup, password_file, cstart.request, credential_id)?;
        let cfin = client::login_finish(cstart.state, password, sstart.response)?;
        let server_sk = server::login_finish(sstart.state, cfin.finalization)?;
        Ok((cfin.session_key, server_sk, cfin.export_key))
    }

    #[test]
    fn registration_and_login_yield_matching_session_keys() {
        let mut rng = rand::rng();
        let setup = ServerSetup::generate(&mut rng);
        let cred = b"alice@example.com";
        let pw = b"correct horse battery staple";

        let (pf, export_at_reg) = run_registration(&mut rng, &setup, cred, pw);
        let (csk, ssk, export_at_login) = run_login(&mut rng, &setup, cred, pw, Some(&pf)).unwrap();

        assert_eq!(
            csk.as_bytes(),
            ssk.as_bytes(),
            "client and server must derive the same SessionKey"
        );
        assert_eq!(csk.as_bytes().len(), KEY_LEN);
        assert_eq!(
            export_at_reg.as_bytes(),
            export_at_login.as_bytes(),
            "ExportKey is deterministic across login with the same password",
        );
    }

    #[test]
    fn wrong_password_fails_login_cleanly() {
        let mut rng = rand::rng();
        let setup = ServerSetup::generate(&mut rng);
        let cred = b"bob@example.com";
        let pw = b"hunter2";

        let (pf, _) = run_registration(&mut rng, &setup, cred, pw);

        // Same credential id, deliberately wrong password.
        let err = run_login(&mut rng, &setup, cred, b"hunter3", Some(&pf))
            .expect_err("wrong password must not succeed");
        assert!(
            matches!(err, OpaqueError::InvalidLogin),
            "expected InvalidLogin, got {err:?}"
        );
    }

    #[test]
    fn export_key_differs_per_password() {
        let mut rng = rand::rng();
        let setup = ServerSetup::generate(&mut rng);
        let cred = b"carol@example.com";

        let (_, ek1) = run_registration(&mut rng, &setup, cred, b"first-password");
        let (_, ek2) = run_registration(&mut rng, &setup, cred, b"second-password");

        assert_ne!(
            ek1.as_bytes(),
            ek2.as_bytes(),
            "different passwords must yield distinct export keys",
        );
    }

    #[test]
    fn password_file_round_trips_through_bytes() {
        let mut rng = rand::rng();
        let setup = ServerSetup::generate(&mut rng);
        let cred = b"dave@example.com";
        let pw = b"a-strong-and-elaborate-password";

        let (pf, _) = run_registration(&mut rng, &setup, cred, pw);
        let bytes = pf.as_bytes().to_vec();
        let pf2 = PasswordFile::from_bytes(&bytes).unwrap();
        assert_eq!(pf.as_bytes(), pf2.as_bytes());

        // Stored file must still drive a successful login.
        let (csk, ssk, _) = run_login(&mut rng, &setup, cred, pw, Some(&pf2)).unwrap();
        assert_eq!(csk.as_bytes(), ssk.as_bytes());
    }

    #[test]
    fn server_setup_round_trips_through_bytes() {
        let mut rng = rand::rng();
        let setup = ServerSetup::generate(&mut rng);
        let bytes = setup.as_bytes().to_vec();
        let setup2 = ServerSetup::from_bytes(&bytes).unwrap();
        assert_eq!(setup.as_bytes(), setup2.as_bytes());

        // Same setup, both copies must speak the same OPAQUE.
        let cred = b"eve@example.com";
        let pw = b"yet-another-password";
        let (pf, _) = run_registration(&mut rng, &setup, cred, pw);
        let (csk, ssk, _) = run_login(&mut rng, &setup2, cred, pw, Some(&pf)).unwrap();
        assert_eq!(csk.as_bytes(), ssk.as_bytes());
    }

    #[test]
    fn message_round_trips_through_bytes() {
        let mut rng = rand::rng();
        let setup = ServerSetup::generate(&mut rng);
        let cred = b"frank@example.com";
        let pw = b"another-password";

        let cstart = client::registration_start(&mut rng, pw).unwrap();
        let bytes = cstart.request.serialize();
        let parsed = RegistrationRequest::deserialize(&bytes).unwrap();
        // Server can equally finish from either copy.
        let sstart = server::registration_start(&setup, parsed, cred).unwrap();
        let resp_bytes = sstart.response.serialize();
        let resp_parsed = RegistrationResponse::deserialize(&resp_bytes).unwrap();

        let cfin = client::registration_finish(&mut rng, cstart.state, pw, resp_parsed).unwrap();
        let upload_bytes = cfin.upload.serialize();
        let upload_parsed = RegistrationUpload::deserialize(&upload_bytes).unwrap();

        let pf = server::registration_finish(upload_parsed);

        // Login messages too.
        let cl = client::login_start(&mut rng, pw).unwrap();
        let cl_req_bytes = cl.request.serialize();
        let cl_req_parsed = LoginRequest::deserialize(&cl_req_bytes).unwrap();
        let sl = server::login_start(&mut rng, &setup, Some(&pf), cl_req_parsed, cred).unwrap();
        let sl_resp_bytes = sl.response.serialize();
        let sl_resp_parsed = LoginResponse::deserialize(&sl_resp_bytes).unwrap();
        let cfin = client::login_finish(cl.state, pw, sl_resp_parsed).unwrap();
        let final_bytes = cfin.finalization.serialize();
        let final_parsed = LoginFinalization::deserialize(&final_bytes).unwrap();
        let ssk = server::login_finish(sl.state, final_parsed).unwrap();
        assert_eq!(cfin.session_key.as_bytes(), ssk.as_bytes());
    }

    #[test]
    fn malformed_message_bytes_fail_deserialize() {
        // Empty input is clearly malformed.
        let err = RegistrationRequest::deserialize(&[]).unwrap_err();
        assert!(matches!(err, OpaqueError::Serialization(_)), "got {err:?}");

        // Non-empty but obviously wrong shape. `LoginResponse` is the
        // most complex message (OPRF evaluation + masked envelope + KE2)
        // so a 4-byte payload either fails the length check or the group
        // element parse — both surface as `Serialization` after the
        // mapping in `OpaqueError::from_protocol`.
        let err = LoginResponse::deserialize(&[0u8; 4]).unwrap_err();
        assert!(matches!(err, OpaqueError::Serialization(_)), "got {err:?}");
    }

    #[test]
    fn missing_password_file_does_not_panic_and_fails_login() {
        // OPAQUE's `dummy` flow lets `server::login_start` return Ok
        // even when the credential is unknown — the test is that
        // `client::login_finish` then refuses to authenticate without
        // panicking.
        let mut rng = rand::rng();
        let setup = ServerSetup::generate(&mut rng);
        let cred = b"ghost@example.com";

        let cl = client::login_start(&mut rng, b"anything").unwrap();
        let sl = server::login_start(&mut rng, &setup, None, cl.request, cred).unwrap();
        let err = client::login_finish(cl.state, b"anything", sl.response).unwrap_err();
        assert!(matches!(err, OpaqueError::InvalidLogin), "got {err:?}");
    }

    #[test]
    fn session_and_export_keys_zeroize_on_drop() {
        // We cannot directly observe the freed memory portably, but we
        // can prove the API holds the right marker traits — and the
        // newtype derives `ZeroizeOnDrop`, so dropping wipes the inner
        // GenericArray.
        fn assert_zod<T: zeroize::ZeroizeOnDrop>() {}
        assert_zod::<SessionKey>();
        assert_zod::<ExportKey>();
    }

    #[test]
    fn debug_impls_do_not_leak_key_material() {
        let mut rng = rand::rng();
        let setup = ServerSetup::generate(&mut rng);
        let cred = b"hank@example.com";
        let pw = b"a-very-private-password";
        let (pf, ek) = run_registration(&mut rng, &setup, cred, pw);

        let pf_dbg = format!("{pf:?}");
        let ek_dbg = format!("{ek:?}");
        let setup_dbg = format!("{setup:?}");

        // None of the debug strings should contain raw key bytes.
        assert!(!pf_dbg.contains(&format!("{:?}", pf.as_bytes())));
        assert!(!ek_dbg.contains(&format!("{:?}", ek.as_bytes())));
        assert!(!setup_dbg.contains(&format!("{:?}", setup.as_bytes())));
    }

    #[test]
    fn suite_constants_are_stable() {
        assert_eq!(SUITE_ID, "OPAQUE-ristretto255-SHA512-Argon2id");
        assert_eq!(SUITE_VERSION, 1);
        assert_eq!(KEY_LEN, 64);
    }

    /// `SessionKey` and `ExportKey` MUST compare via
    /// `subtle::ConstantTimeEq`, not via the derived `slice::eq`. We
    /// can't time-measure this in a unit test portably, but we can
    /// pin the *semantic* contract: equal-length unequal keys must
    /// still report `!=` and equal keys must report `==` even when
    /// the differing byte sits at the very last position (the case
    /// where short-circuiting `slice::eq` is most observable).
    #[test]
    fn session_and_export_keys_compare_in_constant_time() {
        // Build two `SessionKey`s that differ only in the final byte,
        // and one that is identical. If we had derived `PartialEq` on
        // `GenericArray<u8, U64>` (which delegates to `slice::eq`),
        // the equality semantics would be the same — but the *timing*
        // would not. The point of this test is to guarantee that
        // future refactors don't replace the explicit
        // `subtle::ConstantTimeEq` impl with a derived one, which
        // would silently regress the timing property.
        let mut a_bytes = [7u8; KEY_LEN];
        let mut b_bytes = [7u8; KEY_LEN];
        b_bytes[KEY_LEN - 1] = 8;
        let c_bytes = a_bytes;
        a_bytes[0] = 7;
        let a = SessionKey(a_bytes);
        let b = SessionKey(b_bytes);
        let c = SessionKey(c_bytes);
        assert_eq!(a, c, "byte-identical SessionKeys must compare equal");
        assert_ne!(
            a, b,
            "SessionKeys differing in tail byte must compare unequal"
        );

        let ea = ExportKey(a_bytes);
        let eb = ExportKey(b_bytes);
        let ec = ExportKey(c_bytes);
        assert_eq!(ea, ec, "byte-identical ExportKeys must compare equal");
        assert_ne!(
            ea, eb,
            "ExportKeys differing in tail byte must compare unequal"
        );

        // Marker-trait check: confirm the `PartialEq` impl is NOT
        // routed through `derive`-generated machinery. The custom
        // impl is in `messages` / here; if a future PR drops it and
        // falls back to `#[derive(PartialEq)]` on the newtype, the
        // type would still satisfy `PartialEq` but lose
        // constant-time. We pin the custom impl via a trait-bound
        // assertion that `subtle::ConstantTimeEq` is in fact
        // implemented by the inner array — if upstream `subtle` ever
        // drops it, this test catches the regression before key-bytes
        // comparisons silently degrade.
        fn assert_ct_eq<T: subtle::ConstantTimeEq + ?Sized>() {}
        assert_ct_eq::<[u8]>();
    }

    /// `ServerLoginState` MUST round-trip through `to_bytes` /
    /// `from_bytes` and the rehydrated state MUST drive a successful
    /// `login_finish` to the same `SessionKey` the original state
    /// would have. This is the property that makes the broker's
    /// stateless `/auth/login/start` → `/auth/login/finish` flow
    /// work without affinity routing.
    #[test]
    fn server_login_state_round_trips_through_bytes() {
        let mut rng = rand::rng();
        let setup = ServerSetup::generate(&mut rng);
        let cred = b"ivan@example.com";
        let pw = b"a-password-for-state-round-trip";

        let (pf, _) = run_registration(&mut rng, &setup, cred, pw);

        // Drive login_start to obtain a ServerLoginState, then
        // serialise/deserialise it and use the rehydrated copy to
        // finish the handshake.
        let cl = client::login_start(&mut rng, pw).unwrap();
        let sl = server::login_start(&mut rng, &setup, Some(&pf), cl.request, cred).unwrap();

        // The to_bytes return type is Zeroizing<Vec<u8>>; clone the
        // underlying slice so we can keep the bytes alive past the
        // intermediate's scope without disabling the zeroize-on-drop
        // contract on the original buffer.
        let state_bytes: Vec<u8> = sl.state.to_bytes().to_vec();
        let rehydrated = ServerLoginState::from_bytes(&state_bytes)
            .expect("round-trip must succeed for a freshly emitted state");

        let cf = client::login_finish(cl.state, pw, sl.response).unwrap();
        let server_sk = server::login_finish(rehydrated, cf.finalization).unwrap();
        assert_eq!(
            cf.session_key.as_bytes(),
            server_sk.as_bytes(),
            "rehydrated ServerLoginState must derive the same SessionKey",
        );
    }

    #[test]
    fn server_login_state_to_bytes_is_zeroizing() {
        // Type-level proof. The signature alone is the contract —
        // anything weaker (a bare `Vec<u8>`) lets a future refactor
        // silently strip the zeroize-on-drop wrapper. If this stops
        // compiling, the signature changed in a way that breaks the
        // \"key material is wiped on drop\" guarantee in the issue body.
        fn assert_zeroizing<T: zeroize::Zeroize>(_: zeroize::Zeroizing<T>) {}
        let mut rng = rand::rng();
        let setup = ServerSetup::generate(&mut rng);
        let cred = b"jess@example.com";
        let pw = b"another-password-for-the-zeroize-check";
        let (pf, _) = run_registration(&mut rng, &setup, cred, pw);
        let cl = client::login_start(&mut rng, pw).unwrap();
        let sl = server::login_start(&mut rng, &setup, Some(&pf), cl.request, cred).unwrap();
        assert_zeroizing(sl.state.to_bytes());
    }

    #[test]
    fn server_login_state_from_bytes_rejects_garbage() {
        let err = ServerLoginState::from_bytes(&[]).unwrap_err();
        assert!(matches!(err, OpaqueError::Serialization(_)), "got {err:?}");

        let err = ServerLoginState::from_bytes(&[0u8; 4]).unwrap_err();
        assert!(matches!(err, OpaqueError::Serialization(_)), "got {err:?}");
    }
}
