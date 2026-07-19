# botwork-vault

`botwork-vault` is a per-tenant encrypted secret vault: a single OPAQUE-key-sealed file
holds a structured payload of typed secrets (SSH keys, API tokens, certificates,
passwords, etc.) with associated metadata, each individually wrapped under its
own DEK so a memory dump after a single fetch leaks one secret rather than the
whole tenant's payload.

All crypto is handled by `chacha20poly1305` + `hkdf-sha512` (RustCrypto org,
audited primitives); the crate provides the on-disk layout, per-entry DEK
indirection, atomic writes, path validation, and a `clap` CLI on top.

The format requires a live OPAQUE lease to load.

---

## On-disk layout

```
<root>/
├── vault.botwork          # 0600; sealed v4 vault
├── vault.botwork.gen      # 0600; 8-byte little-endian u64 generation counter
└── public/               # plaintext public-key sidecar; 0755
    └── ssh/              # 0755
        └── <label>.pub   # 0644; one OpenSSH authorized_keys line per file
```

`<root>` is created with permissions `0700`; `vault.botwork` is `0600`. Writes use
an atomic tempfile-fsync-rename pattern so a crash never leaves a partial file.

When the public-key sidecar is first used (`pubkey add`), `<root>` is chmod'd
to `0701` (owner rwx + world execute-only, group nothing). This lets a peer
process (e.g. the manhole bastion sshd) traverse into `<root>/public/` via a
known path without being able to list `<root>` itself. Existing vaults that
never use the public store remain at `0700`.

### Multi-writer safety (CAS + flock)

The vault uses a two-layer concurrency mechanism to prevent lost writes when
multiple processes or threads write to the same vault file concurrently.

**Advisory exclusive lock (`vault.botwork.gen`).**
Every `persist()` call acquires an **exclusive `flock`** on the
`vault.botwork.gen` sidecar file before touching `vault.botwork`. The lock
is held for the entire read-check-write cycle and released only after the
generation counter is bumped. This serialises concurrent writers within a
process _and_ across processes on the same host (e.g. a CLI invocation
racing against the auth-broker).

**Generation CAS.**
`vault.botwork.gen` holds a single little-endian `u64` incremented on every
successful write. When a vault is opened (`unlock_master` /
`open_with_master`), the current generation is read and stored on the
in-memory state as the *expected* generation. On each `persist()`:

1. The lock is acquired.
2. The on-disk generation is re-read under the lock.
3. If it differs from the expected generation, `VaultError::Conflict` is
   returned before any write happens — a concurrent writer has advanced the
   file and the current in-memory state is stale.
4. If it matches, `vault.botwork` is written atomically (temp-rename), the
   counter is bumped, and the lock is released.

The in-memory expected generation is updated after each successful write so
that the same `Vault` handle can perform sequential mutations without
spurious conflicts.

**Caller responsibility.**
A `VaultError::Conflict` is not a logic error — it means a concurrent writer
won the race. Callers should reload the vault (call `unlock_master` /
`open_with_master` again) and retry their operation. The auth-broker maps
`Conflict` to `503 Service Unavailable` with the `vault_conflict` code so
the upstream HTTP client can detect and retry.

**What this does _not_ protect against.**
The lock is advisory (POSIX `flock`). Processes that do not call
`VaultLock::acquire` before writing bypass the mechanism entirely. All
vault writers in this codebase go through `Vault::persist`, so the
invariant holds in practice. NFS or distributed filesystems may not honour
`flock` reliably; the vault is designed for single-host deployment.

### `vault.botwork` binary format

Current format (v4).

```
[4 bytes   magic:            "BSVL"]
[1 byte    format version:   4]
[1 byte    suite version:    matches botwork-opaque-handshake::SUITE_VERSION]
[16 bytes  HKDF salt:        random per vault, stable across writes]
[12 bytes  ChaCha20 nonce:   random per write]
[N  bytes  ciphertext:       sealed PayloadV4]
[16 bytes  Poly1305 AEAD tag]
```

The 22-byte `header_core` (magic..salt) is passed verbatim to
`ChaCha20Poly1305::encrypt_in_place_detached` as **associated data**.
The AEAD tag therefore authenticates `(header_core, ciphertext)` —
tampering with the magic, the format version, the suite version, the
salt, or the ciphertext all fail the AEAD tag check at open time. The
nonce is part of the file but not part of the AAD (it's the AEAD's
own nonce input); flipping it ALSO fails the tag check, because the
cipher reproduces a different keystream and a different MAC.

### Suite-version binding

The `suite_version` byte mirrors the `opaque_password_file.suite_version`
column the OPAQUE password file carries on the broker side, and is
mixed into the HKDF `info` that derives the v4 master key from the
OPAQUE `export_key`. The consequence:

- A future OPAQUE suite rotation produces a different master key
  from the same input bytes, even with the same `export_key` material.
- `Vault::unlock_master` refuses to load a vault whose header
  `suite_version` doesn't match the caller's supplied value.

### `PayloadV4` (the bytes the outer AEAD seals)

```rust
struct PayloadV4 {
    version: u32,             // 1; reserved for future intra-payload bumps
    created_at: i64,
    updated_at: i64,
    entries: BTreeMap<SecretKey, EntryEnvelope>,
}

struct EntryEnvelope {
    wrapped_dek: Vec<u8>,     // nonce(12) || ciphertext(32) || tag(16) = 60 bytes
    ciphertext: Vec<u8>,      // value sealed under DEK + 16-byte tag
    nonce: [u8; 12],          // per-entry AEAD nonce
    version: u8,              // per-entry rotation tag (v4 ships as 1)
    meta: EntryMeta,          // cleartext metadata; bound into per-entry AAD
}

struct EntryMeta {
    kind: SecretKind,
    created_at: i64,
    updated_at: i64,
    last_used_at: Option<i64>,
    tags: Vec<String>,
    allowed_consumers: Vec<String>,
    created_at_utc: DateTime<Utc>,
    rotated_at_utc: DateTime<Utc>,
}
```

#### Per-entry DEK indirection

Each entry stored in `entries` has its own freshly-generated 32-byte
DEK. The DEK is wrapped under the v4 master key (yielding `wrapped_dek`);
the entry's value bytes are sealed under the DEK with `nonce`, with the
serialised `meta` bound in as AAD.

Per-secret unlock: a memory dump captured **after** fetching one
secret leaks that one secret's plaintext, not every entry in the
vault. The master key stays in process memory (the auth-broker's
[`UnlockedMasterKey`][unlocked-master-key] cache holds it inside a
`Zeroizing<[u8; 32]>`), but individual entry plaintexts only
materialise for the duration of a single
[`Vault::decrypt_entry`][decrypt-entry] call and are wiped when the
returned `Zeroizing<Vec<u8>>` is dropped.

The per-entry `version` field carries a rotation tag — v4 ships as
`1` for every entry. A future per-entry rotation API
(`Vault::rotate_entry`) can re-wrap one envelope under a fresh DEK
without bumping the outer vault format. The function isn't exposed
in v4 because no current caller needs it; the field reserves the
space.

[unlocked-master-key]: src/vault.rs
[decrypt-entry]: src/vault.rs

## Cryptography

The crate composes RustCrypto-org primitives directly. There is no
wrapper crate between us and the AEAD.

1. **Master-key derivation.** The OPAQUE-supplied `export_key` (64
   bytes of SHA-512-hash output, per
   [`botwork-opaque-handshake`'s ciphersuite][opaque-suite]) is fed
   into HKDF-SHA-512 along with the per-vault salt and the
   suite-version byte to produce a 32-byte master key.
   [`vault/src/kdf.rs::derive_master_key`][derive-master-key] is the
   one-and-only entry point — `Vault::create` and
   `Vault::unlock_master` both go through it.
2. **Per-entry DEK wrap.** A fresh 32-byte DEK is generated from
   `OsRng` per `put_secret` call (also on each `touch_last_used`
   reseal). Wrapped under the master key with ChaCha20-Poly1305,
   no AAD.
3. **Per-entry value seal.** Value bytes sealed under the DEK with
   ChaCha20-Poly1305, the entry's `EntryMeta` (serde_json-serialised
   for determinism) bound in as AAD.
4. **Outer file seal.** The whole `PayloadV4` is sealed under the
   master key with ChaCha20-Poly1305, the 22-byte `header_core`
   bound in as AAD.

Why these specifically:

- **HKDF-SHA-512** — the OPAQUE crate produces SHA-512-based
  outputs, so staying on the same hash means the vault's master-key
  derivation can't desync from the source entropy on a future suite
  bump. No Argon2id at this layer — OPAQUE has already done it
  client-side.
- **ChaCha20-Poly1305** — RFC 7539 AEAD; chosen over AES-256-GCM
  because it's nonce-misuse-failure-tolerant in software-only
  builds (no AES-NI dependency) and the `chacha20poly1305` crate is
  part of the NCC-audited RustCrypto AEADs corpus. Same primitive
  every other layer of this workspace uses.
- **Direct, not via a wrapper** — calling the AEAD directly with an
  explicit `&Nonce` per call removes
  nonce-reuse foot-guns in the type system.

[opaque-suite]: ../opaque-handshake/src/lib.rs
[derive-master-key]: src/kdf.rs

---

## Threat model

- **Offline disk attacker** — cannot recover plaintexts without the
  OPAQUE `export_key`, which is only computable by a live login
  against a still-registered tenant. There is no Argon2id
  brute-force surface inside the vault file any more; the entire
  cost shifted into the OPAQUE handshake (server-side OPRF +
  client-side Argon2id over the password).
- **Live-RCE while unlocked** — a process with the same privileges
  can read the cached master key plus any in-flight decrypted entry.
  Per-entry DEK indirection bounds the blast radius: a snapshot
  taken after a single `/secrets/fetch` round-trip surfaces one
  secret's bytes, not every entry. Wider mitigations live in OS
  process isolation and the auth-broker's cache TTL.
- **Consumer-access policy** — `allowed_consumers` metadata is
  *stored* in this crate but **not enforced here**. Enforcement
  lives in `botwork-auth-broker`, which inspects the metadata and
  decides whether to release a secret to a given plugin identity.
- **Public-key sidecar** — `<root>/public/` is **unencrypted by
  design**. Public keys carry no confidentiality requirement; only
  integrity matters (the files are owned and written by the same
  uid that owns the vault).

---

## Public API

```rust
Vault::new(root)                                  // uninitialised handle

Vault::create(root, export_key, suite_version)    // init a fresh v4 vault
                                                  //   (already unlocked)

vault.unlock(export_key, suite_version)           // load + open; cache the
                                                  //   derived master
vault.unlock_master(export_key, suite_version)    // ditto but returns an
                                                  //   opaque UnlockedMasterKey
                                                  //   the caller can hand to
                                                  //   decrypt_entry without
                                                  //   re-deriving
vault.open_with_master(&master)                   // re-open the file with an
                                                  //   already-derived master
                                                  //   (auth-broker hot path)
vault.lock()                                      // drop cached state
vault.is_unlocked() -> bool

vault.put_secret(key, entry)                      // generate DEK, seal value,
                                                  //   reseal outer file
vault.decrypt_entry(&master, &key)
   -> Result<Zeroizing<Vec<u8>>, VaultError>      // per-entry decrypt
vault.get_secret(&key) -> DecryptedSecret         // key + metadata + Zeroizing<Vec<u8>> value
vault.list_entries() -> Vec<(SecretKey, SecretMeta)>
vault.list_secrets() -> Vec<(SecretKey, SecretMeta)>   // alias for list_entries
vault.delete_secret(&key)
vault.touch_last_used(&key)                       // rotates the per-entry DEK
```

`UnlockedMasterKey` is `Zeroizing<[u8; 32]>` with `ZeroizeOnDrop`. No
`Clone`, no `Debug` that leaks bytes — same opacity discipline as the
`WrappingKey` shape from
[`auth-broker/src/auth/wrapping.rs`](../auth-broker/src/auth/wrapping.rs).

### `SecretKind` variants

`SshPrivateKey`, `SshPublicKey`, `ApiKey`, `OauthToken`, `Pem`,
`Password`, `Opaque`

---

## CLI

```
botwork-vault --version                           # print version and exit
botwork-vault -V                                  # same
botwork-vault [--server <URL>] [--cacert <PATH>] [--bearer-stdin] <subcommand>
botwork-vault init    --root <PATH> [--name <NAME>]
                      [--force --yes-really-overwrite]
botwork-vault verify  --root <PATH>
botwork-vault add     --root <PATH> --service <S> --name <N> --kind <KIND>
                      (--from-file <PATH> | --value-stdin)
                      [--tag <TAG>]... [--allow-consumer <ID>]...
                      [--overwrite]
botwork-vault put-secret …                # alias for `add`
botwork-vault get     --root <PATH> --service <S> --name <N> [--raw]
botwork-vault list    --root <PATH> [--json]
botwork-vault delete  --root <PATH> --service <S> --name <N>
botwork-vault pubkey add       --root <PATH> --kind ssh --label <L> --from-file <PATH> [--force]
botwork-vault pubkey list      --root <PATH> --kind ssh [--json]
botwork-vault pubkey delete    --root <PATH> --kind ssh --label <L>
botwork-vault pubkey cat       --root <PATH> --kind ssh
```

### Remote write/delete

`botwork-vault` now handles local file operations only. The canonical remote
write/delete surface is the `api` server (`/api/v1/secrets`) reached via Envoy
in `botworkz/botwork`.

### `--overwrite` flag (`add` / `put-secret`)

Without `--overwrite`, a `put-secret` against an existing local secret returns
an error rather than silently replacing the entry. Pass `--overwrite` to
replace an existing secret explicitly.

All secret-touching subcommands resolve a bearer from `$BOTWORK_BEARER`
or `--bearer-stdin`, fetch the wrapped export_key from the broker's
`GET /auth/lease/wrapped-export-key` endpoint, and feed the result into
`Vault::create` / `Vault::unlock`.

### `init`

The init flow is lease-driven. It:

1. Reads `$BOTWORK_BEARER` (populated by `eval "$(bw env --tenant <t>)"`)
   or `--bearer-stdin`.
2. Calls `GET /auth/lease/wrapped-export-key` on the broker (resolution
   order: `--server` > `$BOTWORK_LOGIN_SERVER` > built-in).
   TLS trust resolution for HTTPS broker URLs is:
   `--cacert` > `$SSL_CERT_FILE` > built-in roots.
3. Hands the returned bytes into `Vault::create`, which HKDF-derives
   the master key.
4. Refuses to clobber an existing file. `--force --yes-really-overwrite`
   is the explicit override path.

### `verify`

Loads the vault and lists entry count without mutating:

```console
$ botwork-vault verify --root /var/lib/botwork/vault/<tenant>
ok: /var/lib/botwork/vault/<tenant> (5 entries)
```

### Environment variables

- `BOTWORK_BEARER` — bearer token resolved by `bw env`.
  Required for any secret-touching subcommand.
- `BOTWORK_LOGIN_SERVER` — auth-broker base URL. Falls back to
  `http://127.0.0.1:9100` if unset and not overridden by `--server`.
- `SSL_CERT_FILE` — PEM CA certificate bundle path used when `--cacert`
  is not passed.

`BOTWORK_VAULT_FAST_KDF` is test-only and used by workspace tests.

---

## Windows

Windows is currently **not supported**. The atomic-write and
permission-setting code is `#[cfg(unix)]` only.
