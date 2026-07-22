# botwork-opaque-handshake

Typed Rust wrappers around the [`opaque-ke`] OPAQUE PAKE implementation,
fixing a single ciphersuite — **ristretto255 + SHA-512 OPRF + Argon2id
MHF** (CFRG `draft-irtf-cfrg-opaque-16` default).

This is the foundational PAKE library for the `botwork-extra` auth
rollout described in #123 and #124. It is intentionally a **pure
library**: no callers, no persistence, no HTTP routes, no Tokio runtime,
no dependency on `botwork-vault` or `botwork-auth-broker`. The login
endpoints (1a.4), the lease store (1a.3), the `botwork-login` CLI
(1a.6), and `botwork-vault reinit-v4` (1b) are all separate PRs that
consume this crate's API.

## Ciphersuite

| Slot          | Choice                         |
|---------------|--------------------------------|
| OPRF group    | ristretto255 (SHA-512)         |
| Key exchange  | ristretto255 + 3DH (`TripleDh`) |
| Hash          | SHA-512 (implied by OPRF)      |
| KSF (MHF)     | Argon2id (RFC 9106 defaults)   |

Exposed as the public marker `OpaqueSuite` plus the `SUITE_ID` /
`SUITE_VERSION` constants so persistence layers can tag stored blobs.

## API surface

Wire / persistence types (all `serialize()` / `deserialize()` use
opaque-ke's canonical CFRG byte format):

| Direction              | Type                       |
|------------------------|----------------------------|
| client → server, reg   | `RegistrationRequest`      |
| server → client, reg   | `RegistrationResponse`     |
| client → server, reg   | `RegistrationUpload`       |
| server-side persistent | `PasswordFile`             |
| client → server, login | `LoginRequest`             |
| server → client, login | `LoginResponse`            |
| client → server, login | `LoginFinalization`        |
| both ends, derived     | `SessionKey`               |
| client-only, derived   | `ExportKey`                |
| server-side persistent | `ServerSetup`              |
| server-side ephemeral  | `ServerLoginState` (with `to_bytes` / `from_bytes` for two-RTT login over a stateless HTTP surface) |

Helpers (split by trust domain so callers can't accidentally hand server
state to the client and vice versa):

```rust
use botwork_opaque_handshake::{client, server, ServerSetup};

let mut rng = rand::rng();
let setup = ServerSetup::generate(&mut rng);
let credential_id = b"alice@example.com";

// Registration -----------------------------------------------------
let cr = client::registration_start(&mut rng, b"hunter2")?;
let sr = server::registration_start(&setup, cr.request.clone(), credential_id)?;
let cf = client::registration_finish(&mut rng, cr.state, b"hunter2", sr.response)?;
let password_file = server::registration_finish(cf.upload);

// Login ------------------------------------------------------------
let cl = client::login_start(&mut rng, b"hunter2")?;
let sl = server::login_start(
    &mut rng,
    &setup,
    Some(&password_file),
    cl.request.clone(),
    credential_id,
)?;
let cf = client::login_finish(cl.state, b"hunter2", sl.response)?;
let session_key = server::login_finish(sl.state, cf.finalization)?;

assert_eq!(cf.session_key.as_bytes(), session_key.as_bytes());
# Ok::<(), botwork_opaque_handshake::OpaqueError>(())
```

## Key material hygiene

- `#![forbid(unsafe_code)]`.
- `SessionKey`, `ExportKey`, `ClientRegistrationState`, `ClientLoginState`,
  `ServerLoginState`, `PasswordFile`, and `ServerSetup` all carry
  `Zeroize`-on-drop behaviour (either via opaque-ke's own
  `ZeroizeOnDrop` derives or through this crate's wrappers).
- `Debug` impls deliberately do **not** print key bytes.
- The `wrong password` flow surfaces as `OpaqueError::InvalidLogin` and
  nothing else — callers can pattern-match a single arm to render
  "incorrect credentials" without branching on opaque-ke's richer
  internal-error tree.

## Dependency note

The issue body asks for `opaque-ke = "2"`. In practice 2.x transitively
requires `voprf 0.4.0-pre.3`, which fails to build against a modern
rustc with `missing lifetime specifier` errors in
`voprf::serialization::take_ext`. 3.0.0 (Oct 2024) is the first
published release that builds end-to-end on the workspace's
toolchain (rustc 1.96, opaque-ke MSRV 1.74). The high-level
`CipherSuite` / `ServerSetup` / `ClientLogin` / `ServerLogin` /
`ClientRegistration` / `ServerRegistration` API is unchanged between
2.0.0 and 3.0.0 — 3.0.0 syncs to draft-16 (vs draft-10 in 2.0.0),
pulls the curve25519 + voprf + argon2 stack forward to their stable
releases, and bumps MSRV. The PR description tracks the bump as a
deliberate decision rather than a passive transitive bump.

## Tests

```
cargo test -p botwork-opaque-handshake
```

15 unit tests + 1 README doc-test:

- `registration_and_login_yield_matching_session_keys` — round-trip,
  mutual `SessionKey`, deterministic `ExportKey`.
- `wrong_password_fails_login_cleanly` — `OpaqueError::InvalidLogin`,
  no panic.
- `export_key_differs_per_password` — same setup, different password ⇒
  different export key.
- `password_file_round_trips_through_bytes`,
  `server_setup_round_trips_through_bytes`,
  `message_round_trips_through_bytes` — `serialize` /
  `deserialize` round-trip and the parsed forms drive successful logins.
- `malformed_message_bytes_fail_deserialize` — empty / truncated input
  fails with `OpaqueError::Serialization`.
- `missing_password_file_does_not_panic_and_fails_login` — exercises
  opaque-ke's `dummy` flow so unknown credentials have the same wire
  timing as wrong passwords.
- `session_and_export_keys_zeroize_on_drop` — `ZeroizeOnDrop` marker
  check.
- `debug_impls_do_not_leak_key_material` — `Debug` strings contain
  none of the raw bytes.
- `suite_constants_are_stable` — guards `SUITE_ID`, `SUITE_VERSION`,
  `KEY_LEN`.
- `server_login_state_round_trips_through_bytes` —
  `ServerLoginState::{to_bytes,from_bytes}` round-trips and the
  rehydrated state still derives the same mutual `SessionKey`.
- `server_login_state_to_bytes_is_zeroizing` — signature pin: the
  `Zeroizing<Vec<u8>>` return type is a type-level guarantee that
  serialised AKE state is wiped on drop.
- `server_login_state_from_bytes_rejects_garbage` — empty / short
  inputs surface as `OpaqueError::Serialization`.

## Out of scope

- `PasswordFile` / `ServerSetup` persistence (parking lot for 1a.3 /
  botworkz/botwork#141).
- HTTP route exposure (1a.4).
- `botwork-vault` integration (1b).
- Tenant identifier scheme (1a.4 will decide).

[`opaque-ke`]: https://docs.rs/opaque-ke
