# botwork-auth-broker

`botwork-auth-broker` is an Envoy `ext_authz` HTTP check service for
vault-backed authentication on botwork listeners.

Authentication is **OPAQUE + lease**. An operator runs
[`bw`](#bw-cli) once per tenant per device to
drive an OPAQUE login handshake; the server returns a fresh 32-byte
bearer; every subsequent Envoy `ext_authz` check validates that
bearer against a postgres `lease` row before minting a per-request
cap. The broker cannot decrypt any tenant's vault without a live
OPAQUE login from that tenant — that's the load-bearing property
the round-1a + 1b sequence built toward; see
[`SECURITY.md`'s round-1b section](./SECURITY.md#round-1b-complete).

[botwork-extra#146][issue-146] (round 1b of [botwork-extra#123][rfe-123])
deleted the legacy bearer-as-vault-password path along with the
v3 vault format. The broker requires `BOTWORK_DATABASE_URL` to boot.

[issue-146]: https://github.com/botworkz/botwork-extra/issues/146
[rfe-123]: https://github.com/botworkz/botwork-extra/issues/123

## Security model — read this before changing anything

This section is the authoritative statement of what the broker
guarantees and *why*. The rest of the README documents mechanics; this
section documents the invariants those mechanics exist to uphold. If a
change would violate an invariant below, it is wrong even if it
compiles and the tests pass. **Do not "optimise" the persistence or
caching without re-reading this.**

### The two invariants (inviolable)

- **INV-1 — The server can never decrypt a tenant's secrets at rest or
  outside an active, bearer-carrying request.** With no bearer in
  hand, everything the server holds (postgres rows + on-disk vault
  files) is inert ciphertext or one-way hashes. An operator with full
  read access to postgres *and* the vault disk, but without a live
  bearer, recovers **zero** plaintext.

- **INV-2 — A single bearer both authenticates *and* unlocks.** The
  client presents one token. That token alone is sufficient to
  authenticate the lease *and* to unseal the vault — no password
  re-entry, no second secret, no separate key-supply step. This holds
  **transparently across broker restarts**: a cold-started broker with
  an empty in-memory cache re-derives everything it needs from the
  bearer on the next request.

These two look like they should be in tension ("if the bearer alone
unlocks, the server must be holding something unlock-capable"). They
are **not** in tension, because the thing the server holds is sealed
*to the bearer* and the bearer is never persisted. See
[The bearer's dual role](#the-bearers-dual-role).

### The bearer's dual role

The bearer is 32 random bytes minted at `/auth/login/finish`. It is
never stored server-side. It drives two *independent* one-way
transforms:

| Transform | Output | Used for | Stored? |
| --- | --- | --- | --- |
| `SHA-256(bearer)` | `bearer_hash` | lease-row **lookup** (authentication) | yes — postgres `lease.bearer_hash` |
| `HKDF-SHA-512(bearer, salt="auth-broker/lease-kek/v1")` | KEK | **unwrapping** `wrapped_export_key` (decryption) | no — recomputed per request |

Because the two transforms are independent one-way functions, holding
`bearer_hash` reveals nothing about the KEK and vice versa. The stored
hash lets the server find your lease; only the *live* bearer can
produce the KEK that unseals it.

### What is persisted, and why it's safe

| Artifact | Location | Status without a live bearer |
| --- | --- | --- |
| **bearer (plaintext)** | — | **NEVER persisted.** Exists only in memory in-flight, and in the client's keyring (`~/.config/botspace/keyring/<tenant>.json`, mode 0600). |
| `bearer_hash` = `SHA-256(bearer)` | postgres `lease.bearer_hash` | **Inert.** One-way; lookup key only. Cannot be reversed to the bearer. |
| `wrapped_export_key` = `Encrypt(session_key; HKDF(bearer))` | postgres `lease.wrapped_export_key` | **Inert.** ChaCha20-Poly1305 ciphertext. No bearer → no KEK → cannot unwrap → no session_key → no master → no vault decryption. **Safe to store precisely because it is locked to the bearer, which is never stored.** |
| OPAQUE `password_file` | postgres `opaque_password_file` | **Inert.** OPAQUE verifier. The protocol guarantees the server never sees the client `ExportKey`; the verifier cannot derive it. |
| OPAQUE `ServerSetup` | disk | **Inert.** No vault key material. |
| `vault.botwork` | disk (`${BOTWORK_VAULT_ROOT}/<tenant>/`) | **Inert.** ChaCha20-Poly1305 sealed under `HKDF(session_key, vault-header-salt, suite_version)`. The `session_key` is not derivable from any on-disk or DB state. |
| unlocked **master key** | **memory only** | **Never persisted.** `UnlockedMasterKey` (`Zeroizing<[u8;32]>`); scrubbed on eviction; absent after restart (cache starts empty). |

### The reboot walkthrough (the canonical example)

This is the scenario that everyone gets wrong. Register, get a bearer,
store secrets, then the **broker process restarts**. The in-memory
cache (and the unlocked master) are gone. The client presents **only
its bearer** on the next request:

```text
bearer (from client keyring — the ONLY thing the client sends)
   │
   ├─ SHA-256(bearer) ──────────► SELECT * FROM lease WHERE bearer_hash = $1
   │                                (row survived the restart — it's in postgres)
   │
   └─ HKDF-SHA-512(bearer) = KEK
          │
          ▼
      ChaCha20Poly1305.Decrypt(lease.wrapped_export_key; KEK) = session_key
          │
          ▼
      HKDF(session_key, vault_header_salt, suite_version) = master
          │
          ▼
      re-insert CacheEntry { master, ... }  (in memory, TTL-bounded)
```

The master is **re-derived from the bearer**, never recovered from
disk. No password, no re-login, no second secret. That is INV-2. And
because every input on this path is gated on the live bearer, a broker
sitting cold with nothing but its postgres + disk can decrypt nothing.
That is INV-1.

### Boundaries — DO NOT

Each of these breaks an invariant. They are called out because they
are the "reasonable-looking optimisation" that quietly destroys the
security model:

- **DO NOT** introduce a server-held, env-injected, or KMS-backed key
  to wrap `wrapped_export_key` (or anything else that unseals a
  vault). If the *server* can unwrap without the bearer, INV-1 is
  gone. The whole point is that the wrapping key is *derived from the
  bearer* and never stored. (`main.rs` has a startup note pinning
  this — see [`SECURITY.md`](./SECURITY.md#wrapping-key-lifecycle).)
- **DO NOT** persist the vault master key, the unwrapped `session_key`/
  export_key, or any per-entry DEK to disk or postgres in a
  server-recoverable form. Breaks INV-1.
- **DO NOT** make the client re-supply key material (a wrapped key, a
  password, a second token) after a broker restart. The bearer alone
  must suffice. Breaks INV-2.
- **DO NOT** store the bearer plaintext in any row, log line, or
  metric. Only `SHA-256(bearer)` may be persisted. Breaks INV-1.
- **DO NOT** log the bearer, KEK, session_key, master key, or any
  decrypted secret value. Bearer strings and decrypted payloads are
  wrapped in `Zeroizing` for exactly this reason.
- **DO NOT** widen the cache to hold decrypted vault payloads or
  plaintext secret values. The cache holds the **master only**;
  `/secrets/fetch` decrypts per request and drops the plaintext at
  end-of-scope (per-secret unlock).

### Where password rotation lives

There is deliberately **no in-vault change-password flow** (the legacy
bearer-as-vault-password path was deleted in round 1b). Rotating a
tenant's password is an **OPAQUE re-registration**, which is tracked as
its own follow-up (see [Out of scope](#out-of-scope)). Do not
re-introduce an in-vault password setter — it would reintroduce
server-side key custody and break INV-1.

## Endpoint surface
