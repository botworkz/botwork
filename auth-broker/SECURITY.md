# Security notes — `botwork-auth-broker`

This file documents the in-memory security posture of `botwork-auth-broker`
and the deliberate residual risks following:

- the round-1a hardening sprint (issue #126, Tier 2), which landed in #137;
- round 1a of [botwork-extra#123][rfe-123] (issue #133), which landed the
  OPAQUE login / lease store on top;
- **round 1b of [botwork-extra#123][rfe-123] (issue #146)**, which is the
  one this document captures: dropping the legacy bearer-as-vault-password
  path, bumping the vault on-disk format from v3 → v4, and wiring the
  v4 master key off the OPAQUE-derived `export_key`.

It is *not* a security boundary on its own — it is a record of the design
decisions, the residual exposure, and the regression-net that pins each
decision. It is read together with the threat-model section in
[`README.md`](./README.md) — that one describes the *good* properties; this
one is honest about the gaps and the choices behind them.

[rfe-123]: https://github.com/botworkz/botwork-extra/issues/123

## Round 1b complete

This document captures the security posture at the **end of round 1b**
of [botwork-extra#123][rfe-123]. The full RFE is now complete except
for the deferred items called out below.

### What landed in round 1b

| PR / Issue | Contribution                                                                                |
|-----------|---------------------------------------------------------------------------------------------|
| #146      | Vault format v3 → v4, OPAQUE-derived master key, per-entry DEK indirection                  |
| #146      | Auth-broker: legacy bearer-as-vault-password path deleted, cache shape collapsed to master-only |
| #146      | Auth-broker: `GET /auth/lease/wrapped-export-key` for CLI vault-unlock flow                  |
| #146      | Auth-broker: `CapEntry::lease_id` collapsed from `Option<Uuid>` to `Uuid`                    |
| #146      | `botwork-vault init --from-lease` CLI subcommand + migration runbook                          |

### What's still deferred

| Workstream                                                            | Where                                            |
|-----------------------------------------------------------------------|--------------------------------------------------|
| Admin endpoint for lease revocation / listing                          | its own issue (auth model + audit shape design)  |
| Server-side lease-key persistence                                     | abandoned; bearer-derived KEK replaces it        |
| OPAQUE re-registration / change-password flow                         | its own issue (bulk-revoke flow + UI design)     |
| Per-entry rotation API (`Vault::rotate_entry`)                        | parking lot; v4 envelope reserves the version byte |
| Multi-tenant migration tooling (`botwork-vault migrate-v3-to-v4`)     | parking lot; manual runbook is the v0 answer      |
| Janitor for expired / revoked lease rows                              | parking lot                                      |
| Per-tenant `max_lease` policy storage (today hardcoded 30 d)         | parking lot                                      |
| TEE / enclave deployment (Tier 3)                                     | deferred                                         |
| HTTP `/metrics` Prometheus exporter                                   | follow-up PR                                     |
| `mlock` on by default                                                 | feature-flagged follow-up                        |
| `bw refresh` subcommand for sliding lease renewal          | parking lot                                       |

### The round-1b contract, in one paragraph

The bearer on the wire is a fresh 32-byte random value minted at the
end of an OPAQUE login handshake. The server stores only
SHA-256(bearer) in postgres as the lease lookup key; the plaintext
bearer never lands in a row. The OPAQUE mutual SessionKey is sealed
in `lease.wrapped_export_key` under a KEK derived from that bearer via
HKDF-SHA-512 with salt `b"auth-broker/lease-kek/v1"` and empty
`info`. **On a valid lease the broker also opens the tenant's v4 vault
file by HKDF-deriving the master key from the recovered OPAQUE
SessionKey**; that means the broker can decrypt secrets only while a
live request carries the matching bearer. Caps minted carry the
`lease_id` they came from, so an admin lease-revoke (or a future
password-change-time bulk revoke) can drop the whole cap cohort in
one `evict_caps_for_lease(state, lease_id)` call. The legacy
bearer-as-vault-password path is **gone**. The v3 vault format is
**gone**. Both deletions are pinned by tests; see the round-1b
contract enumeration in `tests/cap_lease_cohort.rs`,
`tests/opaque_e2e.rs`, and `tests/restart_preserves_leases.rs`.

## Threat model summary

- **TLS** is assumed to terminate upstream (typically Envoy / the
  `botworkz/space` listener). Bearer tokens travel as
  `Authorization: Bearer <token>` headers under that TLS.
- The browser-facing `/api/auth/*` surface may carry the exact same
  lease bearer in the `botwork_cap` cookie. Header and cookie validate
  identically; the header wins when both are present.
- **Tenant identity** comes from `x-envoy-original-path`, so the broker
  must only be reachable via Envoy. In the supported container deploy
  the broker port is **never published** to the host — the docker
  network namespace is the trust boundary, not the bind address.
- **In-memory state** holds:
  - the in-flight OPAQUE pending-handshake map
  - the unlocked-master-key cache (one `UnlockedMasterKey` per
    `(tenant, bearer)` hash — no decrypted vault payload, no
    plaintext secret values)
  - per-cap `(cache_key, namespace, plugin, lease_id)` records that
    survive for 60 s

## Cookie transport security

`/api/auth/login` returns the lease bearer in two equivalent
transports:

- JSON body field `bearer` for explicit clients
- `Set-Cookie: botwork_cap=...` for browsers

The cookie contract is deliberately narrow:

- `HttpOnly` so browser JS cannot read the bearer
- `SameSite=Lax` so the login/logout flow still works while reducing
  ambient cross-site sends
- `Path=/` so one lease spans tenant UI and `/api/*`
- `Secure` when the listener advertises `x-forwarded-proto=https`
- expiry equal to the lease row's `expires_at`

`POST /api/auth/logout` clears the cookie (`Max-Age=0`) and revokes the
underlying lease, so cookie deletion is not the only line of defence.
Server-side validation remains identical for bearer and cookie: both
decode to the same 32-byte lease bearer, both hit the same
`bearer_hash` lookup, and both inherit the same `expired_lease` /
`revoked_lease` semantics.

## Reserved-name parsing as defence in depth

The Phase 2 URL grammar reserves tenant names `admin`, `api`, `auth`,
`static`, `stats`, and `logs`, and only treats
`^[A-Za-z0-9_-]{1,63}$` as tenant / workspace / plugin space. That
means names outside the regex (for example `@@*`, dotted names, and
other system-looking prefixes) never enter tenant space at all.

This is a defence-in-depth check, not the only guard:

1. the listener route table in `botworkz/space` decides which upstream
   receives `/api/*`, static assets, and other system paths;
2. auth-broker re-checks the tenant segment before treating it as a
   tenant-scoped request;
3. tenant-creation validation in the API layer can then reuse the same
   public constants exported by this crate.

Rejecting reserved names here prevents route-table collisions from
silently turning into cross-tenant auth decisions if another layer is
misconfigured.

## The load-bearing round-1b property

After round 1b, the **broker cannot decrypt any tenant's vault
without a live OPAQUE login from that tenant**. This is the
property the entire round-1a + 1b sequence was built toward.

Concretely:

1. The v4 vault master key is HKDF-derived from the OPAQUE
   `export_key` plus the per-vault salt plus the `suite_version`
   byte. The `export_key` is computed inside the OPAQUE handshake
   from the mutual SessionKey; the broker unwraps it only when a
   live request presents the matching bearer.
2. Broker restart does not destroy any lease decryption material,
   because the KEK is re-derived from the request bearer on demand.
3. The v3 reader is **gone**. There is no fallback path that opens
   the file without an OPAQUE-derived master.

The acceptance test
`tests/opaque_e2e.rs::full_register_login_init_put_check_fetch_round_trip`
exercises this end-to-end: spawn postgres, register a tenant,
login, fetch the wrapped export_key, create a v4 vault, put a
secret, check, fetch — every step gated by a live lease.

## In-memory state, post-#146

For every successful `/auth/check`, the broker holds:

| state                                                     | lifetime                                                                    |
| --------------------------------------------------------- | --------------------------------------------------------------------------- |
| `CacheEntry { tenant, vault_root, master, suite_version, … }` | until idle TTL or absolute TTL elapses, or broker restart                  |
| Bearer string (`Zeroizing<String>`)                       | for the duration of the request handler — scrubbed on Drop                  |
| Cap id (`[u8; 32]`)                                       | 60 s (`CAP_TTL`) or until the underlying cache entry is evicted             |
| `FetchSecret.value_b64` (`Zeroizing<String>`)             | until axum has serialised the response and dropped the body — then scrubbed |
| Per-secret decrypt buffer (`Zeroizing<Vec<u8>>`)          | end-of-loop-iteration; one secret at a time, never the whole payload       |

`AppState.auth` (always populated in round 1b):

| state                                              | lifetime                                                                    |
| -------------------------------------------------- | --------------------------------------------------------------------------- |
| `PendingMap` (`Zeroizing<Vec<u8>>` per entry)      | up to `PENDING_TTL` (60 s) between `/auth/login/start` and `/auth/login/finish` |
| `ServerSetup`                                      | persisted at `${BOTWORK_VAULT_ROOT}/opaque_server_setup`, mode `0600`        |
| `CapEntry.lease_id: Uuid`                          | bound for the cap's 60 s TTL; required field (round 1b cutover collapsed `Option<Uuid>`) |

**The cache no longer holds an unlocked `Vault` or a decrypted
payload.** The round-1a → 1b cutover is the single most important
shape change in this PR: the v3 cache held the full decrypted
`VaultContents` of the tenant for the idle TTL window; the v4
cache holds the `UnlockedMasterKey` (32 bytes) plus the vault root
path plus the suite_version. `/secrets/fetch` instantiates a fresh
`Vault` per fetch, calls `Vault::open_with_master`, and runs
`Vault::decrypt_entry` per matched entry. A memory dump captured
after one fetch leaks that one secret's bytes, not every entry in
the vault — this is the per-secret-unlock property the
[`with_locked_cache`][with-locked-cache] hook was designed to
satisfy.

[with-locked-cache]: src/cache.rs

## Zeroize audit

Sweep of `auth-broker/src` for types that hold (transitively) plaintext
key material, vault contents, bearers, or unlocked secrets.

| Type / field                                              | Status post-#146                                           |
| --------------------------------------------------------- | ---------------------------------------------------------- |
| `extract_bearer` → `Option<Zeroizing<String>>`            | ✅ wrapped; scrubbed on drop at the call site              |
| `FetchSecret.value_b64: Zeroizing<String>`                | ✅ wrapped; scrubbed once axum drops the response body     |
| `CacheEntry.master: UnlockedMasterKey`                    | ✅ `Zeroizing<[u8; 32]>`; opaque holder, no `Clone`/`Debug` leak |
| `CacheEntry.vault_root: PathBuf`                          | not secret — filesystem path                               |
| `CacheEntry.suite_version: u8`                            | not secret                                                 |
| `CapEntry.lease_id: Uuid`                                 | not secret — UUID v4 referencing a row in `lease`          |
| `cache_key(tenant, bearer)` digest output `[u8; 32]`      | not secret — output of BLAKE2b-256 hash, used as map key   |
| `PendingEntry.state_bytes: Zeroizing<Vec<u8>>`            | ✅ wrapped; wiped on `take()` / `sweep()`                  |
| `Bearer.bytes: Zeroizing<[u8; 32]>`                       | ✅ wrapped                                                 |
| `ValidatedLease.export_key: Zeroizing<Vec<u8>>`           | ✅ wrapped                                                 |
| Per-secret decrypt buffer in `secrets/fetch` hot path     | ✅ `Zeroizing<Vec<u8>>` from `Vault::decrypt_entry`        |
| Authorization header copy in axum internal buffers        | out of broker's control — axum/hyper own these             |

`#[deny(unsafe_code)]` is set at the crate root (`src/lib.rs`).

### Audit findings (post-#146)

- **No `String`/`Vec<u8>` of plaintext key material survives a handler
  return.** The bearer extracted from `Authorization` is wrapped in
  `Zeroizing<String>` and dropped at the end of `check()`; the
  base64-encoded secret value is wrapped in `Zeroizing<String>` and
  dropped after `Json(payload).into_response()` writes the wire bytes;
  the raw decrypted entry value is wrapped in `Zeroizing<Vec<u8>>`
  inside the per-secret loop.
- **The cache no longer holds the decrypted payload.** This is the
  shape change round 1b was designed to land. The
  `with_locked_cache` test introspection hook can now verify "no
  plaintext for entry X is in the cache" by string-scanning the
  cache entry image — see
  `tests/opaque_e2e.rs::full_register_login_init_put_check_fetch_round_trip`
  for the assertion.
- **Drop order is correct.** `CacheEntry` is a flat record;
  dropping it scrubs the master via `Zeroizing` and drops the
  vault-root path. Per-fetch the `Vault` handle is constructed
  fresh, opened with the cached master, used, and dropped at end
  of fetch — its internal `UnlockedState` zeroizes the wrapped
  DEKs / per-entry envelopes it loaded.

### Follow-ups recorded by this audit

1. **`mlock` of the cache backing store** — gated behind a future
   feature flag, off by default. The portability story (cgroup
   limits, non-root capabilities, mlock failure handling) needs
   design first.
2. **`serde_json::Value` audit** — none on the hot path today; the
   broker only emits structured JSON outwards. If a future call
   site ingests JSON containing secrets, wrap it in
   `Zeroizing<Vec<u8>>`.

## Per-lease unlock secret: SessionKey, not ExportKey

The `lease.wrapped_export_key` column name is OPAQUE-flavoured but the
value stored there is the OPAQUE **mutual SessionKey** from the
login handshake, *not* the client's ExportKey. The reason is
structural:

- OPAQUE's `ExportKey` is **client-only** — the server never observes
  it. That is one of the load-bearing properties of the protocol:
  the server can't decrypt the client's vault even with a complete
  postgres snapshot of `opaque_password_file` rows.
- The mutual `SessionKey`, by contrast, is computed identically on
  both sides at the end of `server_login_finish` /
  `client_login_finish`. It is the value the server is *allowed* to
  hold and persist as the per-lease unlock secret.

The schema column name is preserved verbatim because the *contract*
the column serves ("per-lease unlock secret bound to postgres only
via bearer-derived wrapping") is unchanged. Renaming would force a
cross-repo schema migration for cosmetics only.

### The round-1b twist: SessionKey is also the v4 vault master input

In round 1a the SessionKey was the unlock secret for the lease only.
In round 1b it's *also* the HKDF input that derives the v4 vault
master key (via `vault::kdf::derive_master_key`). The chain is:

```
OPAQUE login → SessionKey (mutual)
              → wrapped under HKDF-SHA-512(bearer, salt="auth-broker/lease-kek/v1"), stored in lease.wrapped_export_key
              → /auth/check unwraps + hands to vault::Vault::unlock_master
              → HKDF-SHA-512(unwrapped_SessionKey, salt, suite_version)
              → 32-byte v4 master key, cached on CacheEntry
              → per-secret decrypt on /secrets/fetch
```

The cache binds the master to a specific vault file shape (the
salt + suite_version are part of the HKDF info). Two consequences:

1. Scp'ing one tenant's vault file into another tenant's directory
   doesn't unlock — the salt is per-vault.
2. A future OPAQUE suite rotation drops every existing v4 vault
   without a re-init: the suite_version byte mismatch surfaces as
   `VaultError::UnsupportedVersion` and the migration runbook
   re-fires.

## Wrapping-key lifecycle

The `lease.wrapped_export_key` column stores the per-lease unlock secret
sealed under a **per-lease KEK derived from the bearer itself**:

```text
HKDF-SHA-512(
  ikm  = bearer_bytes,
  salt = b"auth-broker/lease-kek/v1",
  info = b"",
  L    = 32,
)
```

The wrapping primitive remains ChaCha20-Poly1305 with the same on-disk
layout as before:

```text
[12 nonce][N ciphertext][16 tag]
```

`N` matches the wrapped SessionKey length (64 bytes for the current
OPAQUE suite).

### Threat model

- **Cold disk + postgres dump alone → nothing.** The database holds
  `bearer_hash = SHA-256(bearer)` and the wrapped bytes, but not the
  bearer itself. SHA-256 is one-way here; without the bearer an attacker
  cannot derive the KEK and cannot unwrap the SessionKey. Vault files on
  disk stay sealed.
- **Live root on the broker → bearer-in-flight is interceptable.** The
  bearer appears on each authenticated request. A live operator who can
  observe request headers can capture it and derive that lease's KEK.
  This is the same posture as any session-token design and is not a
  property this PR changes.
- **Broker restart → no lease invalidation.** The server has no
  process-local secret to regenerate. As long as the client still holds
  the bearer in its keyring, the restarted broker can re-derive the KEK
  and recover the SessionKey.
- **Lost bearer (logout, expiry, revoke) → that lease is gone.** Once
  the bearer is discarded, the corresponding `wrapped_export_key` is
  permanently undecryptable. This is the desired lifecycle boundary.

### What this design is NOT

- **Not an admin/unlock endpoint.** There is no operator-held seed, no
  `/admin/unlock`, and no persistent server-side secret that mass-decrypts
  every lease.
- **Not OPAQUE-password-recoverable.** The server still cannot derive the
  client-only OPAQUE `ExportKey` from stored server-side data; it only
  persists the mutual SessionKey wrapped under the bearer-derived KEK.
- **Not a defence against a live operator already on the box.** A root
  operator can still capture bearers in flight on live requests. This
  design narrows the blast radius of disk compromise; it does not make a
  compromised running broker harmless.

### Cutover behaviour

Every lease minted before this change is permanently undecryptable after
deploy, because its row was sealed under the deleted server-side design
and the broker no longer has that key material. Users must run
`bw login` again to mint a fresh lease. Existing vault files
on disk are unaffected: the same OPAQUE SessionKey bytes still derive
the same vault master after re-login.

## OPAQUE `ServerSetup` persistence

`load_or_generate_server_setup` writes the OPAQUE `ServerSetup`
verbatim to `${BOTWORK_VAULT_ROOT}/opaque_server_setup`. Properties:

- Mode `0600`, owned by the broker's user (1100 in the container
  deployment).
- Generated once per broker installation; never rotated (rotating
  invalidates every `opaque_password_file` row, i.e. every tenant
  has to re-register).
- Bytes are the `opaque-ke` canonical serialization. Anyone with
  the file plus access to a tenant's `opaque_password_file` row
  can drive arbitrary login handshakes against that tenant's
  password — that's by construction; the file is the server's half
  of the long-term keypair.

Treat the `ServerSetup` file as a per-deployment root secret. The
vault-root directory it lives in is already protected at the same
tier (it holds `vault.botwork` for every tenant).

### Catastrophic loss

If `opaque_server_setup` is lost or corrupted, **every registered
tenant's OPAQUE verifier becomes permanently unusable**. The broker
cannot reconstruct the `ServerSetup` from any other stored data, and
there is no recovery path short of:

1. Replacing the file with a backup copy (see below), or
2. Re-running `botwork-vault init` for **every tenant from scratch**
   (all existing vault files and OPAQUE verifier rows become unusable;
   tenants must re-register and re-populate their vaults).

This is intentional: the `ServerSetup` is the server's OPAQUE
long-term private key. Its loss is equivalent to losing a CA private
key — the scope of impact is installation-wide.

### Backup procedure

Back up `${BOTWORK_VAULT_ROOT}/opaque_server_setup` to offline,
encrypted storage immediately after first boot (when the broker first
writes the file) and after any scheduled rotation.

Recommended steps:

```sh
# 1. Copy to a staging location, preserving permissions.
install -m 0600 \
    "${BOTWORK_VAULT_ROOT}/opaque_server_setup" \
    /tmp/opaque_server_setup.bak

# 2. Encrypt before transferring off-host (example with age).
age -r <recipient-pubkey> \
    -o opaque_server_setup.age \
    /tmp/opaque_server_setup.bak

# 3. Move to offline / cold storage (USB key, encrypted S3, vault).
#    Shred the plaintext staging copy.
shred -u /tmp/opaque_server_setup.bak
```

Store the backup alongside the database backup so that the pair
is always in sync (an `opaque_server_setup` from one deploy does
not pair with `opaque_password_file` rows from a different deploy).

### Recovery procedure

If the live file is missing or corrupt and a backup exists:

```sh
# 1. Restore the backup to the broker's vault root.
#    Replace BROKER_USER:BROKER_GROUP with the broker's runtime
#    user and group (UID 1100 / GID 1100 in the default container
#    image; adjust for your deployment).
install -o BROKER_USER -g BROKER_GROUP -m 0600 \
    opaque_server_setup.age_decrypted \
    "${BOTWORK_VAULT_ROOT}/opaque_server_setup"

# 2. Restart the broker — it will read the restored file rather than
#    generating a fresh one (the file-exists branch in
#    load_or_generate_server_setup).
```

If no backup exists, every tenant must re-register. The existing
`opaque_password_file` rows and `vault.botwork` files are no longer
usable; remove or archive them before re-running `init` to avoid
confusion:

```sh
# Archive unusable per-tenant data (adjust paths as needed).
mv "${BOTWORK_VAULT_ROOT}" "${BOTWORK_VAULT_ROOT}.lost-$(date +%s)"
mkdir -p "${BOTWORK_VAULT_ROOT}"

# For each tenant:
botwork-vault init --tenant <name>
# Tenant must then re-run bw to obtain a fresh lease.
```

## Enumeration resistance — `/auth/login/*` dummy flow

`/auth/login/start` deliberately drives the OPAQUE `dummy` flow for:

1. Tenants that don't exist in the `tenant` table.
2. Tenants that exist but haven't gone through OPAQUE registration
   (no `opaque_password_file` row).

In both cases the wire response shape (`login_response` byte length,
status code, response timing) matches a real tenant's. The 401 only
surfaces at `/auth/login/finish` when the client's `login_finalization`
fails OPAQUE verification.

This is tested in `tests/opaque_dummy.rs` — if a future refactor
short-circuits one of those arms with a 404, that test trips.

## Constant-time comparisons

`SessionKey::eq` and `ExportKey::eq` in
[`botwork-opaque-handshake`][opaque-handshake] route through
`subtle::ConstantTimeEq`. The admin API key is also compared
in-process via `subtle::ConstantTimeEq`. The lease-path uses SHA-256 of
the bearer as the lookup key against the unique index on `bearer_hash`
— postgres itself does the comparison, and the index is over the *full*
digest, so prefix-timing attacks against the broker do not affect
lookup behaviour. The broker therefore avoids application-layer prefix
comparisons on lease bearer material.

[opaque-handshake]: ../opaque-handshake/src/lib.rs

## Cap-mint and lease-cohort revocation

Every cap minted by `/auth/check` carries `lease_id: Uuid` (no longer
`Option<Uuid>` — round 1b cutover collapsed the optional shape; the
legacy bearer-as-vault-password path that minted `lease_id = None`
caps is **gone**). Every cap is a member of exactly one lease cohort.

The [`cache::evict_caps_for_lease(state, lease_id)`][cache-rs]
helper drops every cap whose `lease_id` matches; it's the typed
seam for two upcoming flows:

1. **Admin lease revocation** — `/admin/api/v1/leases/{id}`
   revoke endpoint (round 1a admin follow-up) will mark
   `revoked_at` and then call this helper.
2. **OPAQUE re-registration** — a future password-change flow will
   call it once per lease in the set of leases it bulk-revokes for
   a tenant.

Both call sites are out of scope for #146; the helper exists so
they have a single typed function to hold the cohort-eviction
semantics. The contract is pinned end-to-end against a real
postgres in `tests/opaque_e2e.rs` and at the data-structure level
in `tests/cap_lease_cohort.rs`. The grep-gated test
`tests/cap_lease_cohort.rs::no_lease_id_none_in_broker_src` pins
zero occurrences of the literal `lease_id: None` in
`auth-broker/src/`; a future refactor that re-introduces the
optional shape trips it before it reaches production.

[cache-rs]: src/cache.rs

## Per-secret unlock (landed in round 1b)

The round-1a SECURITY.md described per-secret unlock as a follow-up
gated on vault PR #134. **It's landed** in round 1b as part of #146:

- `CacheEntry.vault: Vault` → `CacheEntry.master: UnlockedMasterKey`
  + `vault_root: PathBuf` + `suite_version: u8`. The cache no
  longer holds the decrypted `VaultContents`.
- `/secrets/fetch` instantiates a fresh `Vault::new(&entry.vault_root)`,
  opens it with `Vault::open_with_master(&entry.master)`, then loops
  `vault.decrypt_entry(&entry.master, &key)` per matched entry. Each
  decrypt returns a `Zeroizing<Vec<u8>>` that scrubs itself at end
  of loop iteration.
- The `value_b64` field on the response is also wrapped in
  `Zeroizing<String>` so the base64-encoded form lives only until
  axum has written the response.

**A memory dump captured after one `/secrets/fetch` round-trip
leaks exactly that one secret's bytes, not every entry in the
vault.** That's the property the previous SECURITY.md called
out as deferred; in round 1b it's pinned by
`tests/opaque_e2e.rs::full_register_login_init_put_check_fetch_round_trip`
and at the vault-crate level by
`vault/tests/roundtrip.rs::per_entry_decrypt_does_not_leak_other_entries`.

## TTL knobs

Idle and absolute TTLs are configurable per tenant. See `README.md`
"Cache behavior" for the operator-facing summary. The mechanism
lives in `src/config.rs`; the salient points:

- **Defaults** (`IDLE_TTL = 5 min`, `ABSOLUTE_TTL = 8 h`) are unchanged
  by #146 — operators only have to think about TTL when they want
  to tighten it.
- **Per-tenant overrides** via
  `BOTWORK_AUTH_BROKER_TENANT_IDLE_<TENANT>` /
  `BOTWORK_AUTH_BROKER_TENANT_ABS_<TENANT>`.
- **Operator floors** via
  `BOTWORK_AUTH_BROKER_MIN_IDLE_SECS` /
  `BOTWORK_AUTH_BROKER_MIN_ABS_SECS`. A per-tenant override below
  the floor is silently raised at config-load time and logged at
  `warn!`. Floors exist so a deploy can ratchet *down* a permissive
  default without auditing every per-tenant override.
- **Snapshot on insert** — the per-tenant idle TTL is stamped onto
  every `CacheEntry` at insert time. A config change after broker
  startup does not retroactively extend already-cached entries.

### What floors do and do not affect

Floors apply to **future cache inserts**, not to currently-cached
entries. The operational consequence: a panic-button "tighten
everything now" flow is `restart the broker`, not `change the env
var`. Restart drops the whole cache and forces every active tenant
through a fresh `bw → /auth/check` that picks up the new
floor.

This composes cleanly with the bearer-derived KEK lifecycle: the
restart does **not** invalidate active leases, but the first request
that rehydrates cache state does pick up the new floor. Operators keep
one clear "panic button" for cache policy without destroying leases.

## Drop hygiene

- `CacheEntry` drops:
  - `tenant: String` — not secret
  - `vault_root: PathBuf` — not secret
  - `master: UnlockedMasterKey` — scrubs itself on drop
  - `suite_version: u8` — not secret
  - `expires_at`, `last_used`, `created_at`, `idle_ttl` — not secret
- `extract_bearer` returns `Zeroizing<String>`. The `String` allocation
  is scrubbed on drop, including all panic-unwind paths inside the
  handler.
- `FetchSecret.value_b64: Zeroizing<String>` — Serde serialises via
  a custom `serialize_zeroizing_string` shim that treats the wrapper
  exactly like a plain string field.
- Per-fetch the `Vault` handle is constructed fresh, opened with the
  cached master, used to decrypt one entry at a time, then dropped
  at end of fetch — its internal `UnlockedState` zeroizes the
  wrapped DEKs and per-entry envelopes it loaded.
- The cap id is a 32-byte `[u8; 32]` random — not secret in the
  cryptographic sense (no key material), but expiring it within 60 s
  is the smallest practical window.

## Zeroize coverage (current, end of round 1b)

The following types hold key material and implement `Zeroize` /
`ZeroizeOnDrop` (either directly or via a `Zeroizing<>` wrapper):

- `botwork-opaque-handshake::{SessionKey, ExportKey, PasswordFile,
  ServerSetup, ClientRegistrationState, ClientLoginState,
  ServerLoginState}` — derived `ZeroizeOnDrop` (the underlying
  `opaque-ke` types) or custom `Drop` impls that wipe the byte
  buffer.
- `auth::lease::Bearer` — `Zeroizing<[u8; 32]>`.
- `auth::pending::PendingEntry` — `Zeroizing<Vec<u8>>` for the
  serialised `ServerLoginState`.
- `auth::lease::ValidatedLease.export_key` — `Zeroizing<Vec<u8>>`.
- `botwork-vault::UnlockedMasterKey` — `Zeroizing<[u8; 32]>`, opaque
  holder, no `Clone`, no `Debug` that leaks bytes.
- `botwork-vault::Vault::UnlockedState` — `Zeroizing` for the
  master key, zeroizing-on-drop for every per-entry envelope.
- `Vault::decrypt_entry` return type — `Zeroizing<Vec<u8>>`.
- Bearer extracted from `Authorization` header — `Zeroizing<String>`.
- `FetchSecret.value_b64` — `Zeroizing<String>`.

## Rate limiting

The four OPAQUE auth endpoints (`/auth/login/start`, `/auth/login/finish`,
`/auth/register/start`, `/auth/register/finish`) are protected by a
per-`(tenant, source-IP)` token-bucket rate limiter.

### Algorithm

Token-bucket per key: each `(tenant, client_ip)` pair has its own bucket.
Tokens replenish at a configured sustained rate; the bucket capacity caps
burst. Each request consumes one token. Exhausted buckets reply with
`429 Too Many Requests` and a `Retry-After: <secs>` header indicating when
one token will be available again.

### Key derivation

The key is the pair `(requested_tenant_string, client_ip)`.

- **Tenant:** always the *requested* string, applied uniformly before any
  store lookup so the limiter's behaviour and timing are identical whether
  the tenant exists or not. This preserves the OPAQUE enumeration-resistance
  property — a rejected 429 reveals nothing about tenant validity.
- **Client IP:** derived from the `x-forwarded-for` header (leftmost entry)
  or `x-real-ip`. Falls back to the string `"unknown"` when neither header
  is present. In the supported deployment, Envoy terminates all inbound
  traffic and injects `x-forwarded-for`, so the fallback is only reached
  for direct connections that bypass Envoy (which should only be internal
  or administrative traffic).
- **`/auth/login/finish`:** this endpoint has no tenant in the request body
  (it only carries a `handshake_id`). The limiter uses an empty-string
  tenant sentinel so the bucket is keyed per-IP, separate from the
  per-`(tenant, IP)` buckets for the other three endpoints.

### Store

**In-memory, per broker instance.** The map of rate-limit buckets is held
in process memory and resets on broker restart. Stale buckets (idle for
longer than 5 minutes) are pruned by the background prune task
(`crate::cache::prune_once`) that also sweeps the main cache and the
pending-handshake map.

This design is appropriate for a single-instance deployment. If the broker
is ever scaled to multiple replicas, a shared postgres-backed store would be
needed to prevent a client from simply rotating across replicas. That is a
future step; the in-memory approach is documented and constrained here so
the limitation is visible before any multi-replica deployment.

### Configuration

| Environment variable                     | Meaning                                         | Default |
|------------------------------------------|-------------------------------------------------|---------|
| `BOTWORK_AUTH_BROKER_RATE_LIMIT_RPS`     | Sustained rate (tokens/second per key). `0` disables limiting entirely. | `10`    |
| `BOTWORK_AUTH_BROKER_RATE_LIMIT_BURST`   | Burst capacity (tokens). Must be ≥ 1.          | `20`    |

These are read at startup alongside the TTL config. Unknown / malformed
values fall back to defaults with a `warn!` log.

Setting `BOTWORK_AUTH_BROKER_RATE_LIMIT_RPS=0` disables rate limiting
entirely. The in-process test harness uses `AuthState::from_stores`, which
defaults to a disabled limiter, so existing tests are unaffected.

### Implementation notes

The limiter lives in `src/auth/rate_limit.rs`. The `RateLimiter` is a field
of `AuthState` (disabled by default; the production binary sets it from env
via `AuthState::with_rate_limiter`). Unit tests for bucket refill, burst
behaviour, per-key isolation, and stale-bucket eviction use paused-clock
tokio tests. The integration test `tests/rate_limit.rs` asserts that the
429 response fires on the right request and that the structured error
envelope and `Retry-After` header are present.

## Metrics surface

`AppStateMetricsSnapshot` exposes:

- `cache_inserts` — monotonic since startup
- `cache_evictions_idle` / `cache_evictions_absolute` — split by reason
- `cache_size` — live
- `avg_age_secs` — live, computed from `created_at` on every entry

A future Prometheus exporter PR can pull these on `/metrics` scrape;
this PR deliberately doesn't ship the HTTP exporter so the dep closure
stays lean.

## Cross-repo dependency posture

`auth-broker` depends on `botwork-entity` from `botworkz/botwork`
via a `git = … tag = "v0.3.13"` dep. The tag pin keeps the lockfile
reproducible and prevents an upstream main-branch rebase from
silently swapping the schema. When this crate eventually moves into
`botworkz/botwork` proper, that becomes a `path = "../db/entity"`
dep; the git/tag indirection exists *only* because the two repos are
currently split.

## `/auth/check` lease-path on DB outage

Round 1a's design here was *fall through to the legacy
bearer-as-vault-password path on DB error*. **Round 1b removes the
legacy path entirely**; a transient DB outage now surfaces as a
structured `invalid_bearer` 401 to the client. The operational
consequence:

- Operators monitor the `auth/check: lease lookup db error` warn
  log and respond to sustained errors as DB outages.
- Clients re-run `bw` once the broker is healthy again.
- There is no "the legacy path saves us" path. This is acceptable
  because round 1b is the cutover release; the legacy path was
  always destined for deletion, and dragging it through a transient
  DB outage just to claim resilience would re-introduce the exact
  trust-broker-with-the-vault-password exposure the round was
  designed to eliminate.

## Out of scope (this PR)

- **TEE / enclave deployment** — Tier 3, deferred.
- **Removing the cache entirely** (decrypt-on-every-request from disk) — too
  expensive per-request.
- **`mlock` on by default** — gated behind a future feature flag.
- **Admin endpoints for lease revocation / listing.**
- **Janitor for expired / revoked lease rows.**
- **Per-tenant `max_lease` policy storage.**
- **OPAQUE re-registration / change-password flow** — has to
  invalidate every outstanding lease for the tenant before swapping
  the `opaque_password_file` row; its own issue.
- **Multi-tenant migration tooling** — the per-tenant runbook in
  [`docs/migration-v3-to-v4.md`](../docs/migration-v3-to-v4.md) is
  the v0 answer.

## Versioning

This document is a living one; the heading anchors above
(`#zeroize-audit`, `#per-secret-unlock-landed-in-round-1b`,
`#ttl-knobs`, `#drop-hygiene`, `#wrapping-key-lifecycle`,
`#enumeration-resistance----authlogin-dummy-flow`,
`#cap-mint-and-lease-cohort-revocation`, `#round-1b-complete`,
`#the-load-bearing-round-1b-property`,
`#write-path-authorisation`) are stable. Refer to them
rather than line numbers when citing this file from another PR.

## Write-path authorisation

`POST /secrets` and `DELETE /secrets/<service>/<name>?tenant=<tenant>` are the
secret-write endpoints used by the `api` service.

### Port 9100 — public listener trust contract

The auth-broker binds the public listener on
`BOTWORK_AUTH_BROKER_BIND` (default `0.0.0.0:9100`). This listener
serves every auth surface (`/auth/register/*`, `/auth/login/*`,
`/auth/check`, `/secrets/fetch`, …).

**Trust assumptions that must hold for this listener to be safe:**

1. **The broker must only be reachable via Envoy.** Tenant identity
   is derived from the `x-envoy-original-path` header. The broker
   accepts that header **unconditionally** — there is no signature
   or HMAC validation on it. Any caller that can reach port 9100
   can forge a tenant identity by sending an arbitrary
   `x-envoy-original-path` value.

   Authenticating that header, so tenant identity does not rest only
   on network reachability, is the same deliberately deferred concern
   as port 9101 caller authentication below: it is reserved for a
   future service-mesh / sidecar deployment in `botworkz/space`, and
   until then the Docker network boundary is the accepted control.

2. **The `0.0.0.0:9100` default is safe only under Docker network
   isolation.** In the supported container deployment this port is
   **never published to the host** (no `-p`/`--publish`). The
   Docker network namespace is the trust boundary. If you run the
   broker as a bare host process, override the bind address to
   `127.0.0.1:9100` (`BOTWORK_AUTH_BROKER_BIND=127.0.0.1:9100`) so
   it is not reachable beyond loopback.

3. **Do not add a port publish for this service.** Anything that can
   reach port 9100 can attempt to unlock any tenant vault it holds
   a valid password for, and can forge a tenant identity via
   `x-envoy-original-path`.

These invariants are documented in code at `src/main.rs:bind_from_env`
and are enforced by the container deploy in `botworkz/space`; they
are recorded here as explicit, auditable claims rather than implicit
assumptions.

### Port 9101 — internal-API listener trust model

The broker also binds an internal-only API listener on
`BOTWORK_AUTH_BROKER_API_BIND` (default `0.0.0.0:9101`, i.e. one
port above the public listener).

**The sole guard on this listener is Docker network membership.**
There is no bearer validation, no per-request authentication, and
no HMAC or TLS client certificate on port 9101. Any service that
can reach this port over the `botwork` Docker network can call:

- `POST /secrets` — write or overwrite a secret for **any tenant
  with an active in-memory lease**.
- `DELETE /secrets/<service>/<name>?tenant=<tenant>` — delete a
  secret for any such tenant.

**This is intentional.** In the supported deployment only the `api`
service container is expected to reach port 9101. The docker network
boundary is the authentication layer for this surface. Per-request
authentication of the caller (e.g. mTLS or a shared HMAC key) is
deliberately deferred to a future service-mesh / sidecar deployment
under container orchestration, where caller identity and transport
security belong in the infrastructure layer rather than in this
binary. Building a bespoke shared-secret scheme now would add secret
provisioning and rotation burden without an established channel to
carry it, and would be discarded once mesh-level mTLS lands. Until
then, the Docker network boundary — ports never published to the
host, and a host firewall that admits only 22/80/443 — is the
accepted, deliberate trust control for this listener.

**Operational requirements:**
- Port 9101 must **never** be published to the host.
- No service outside the `botwork` Docker network should have a
  route to this port.
- These constraints must be enforced by the deploy configuration in
  `botworkz/space`; this crate assumes that deployment invariant.

### Internal-only listener

The write endpoints are exposed on `BOTWORK_AUTH_BROKER_API_BIND` and are intended
for internal service-to-service traffic only. The listener is reachable by the
`api` service over the docker internal network (`botwork-internal`) and is never
a public Envoy/user-facing surface.

The deploy in `botworkz/space` is responsible for keeping this port
unpublished to the host.

Trust at this layer is therefore based on **network reachability**, not bearer
validation in these handlers.

### Same trust tier

A captured bearer can invoke user-facing `POST /api/v1/secrets` through Envoy,
which authenticates and forwards to `api`, and `api` forwards to this listener.
So the effective trust tier remains "anyone with a live bearer for that tenant";
it is the same security boundary as before, reached through the `api` write path.

### Secrets are one-way from the user's perspective

There is **no user-facing read endpoint** for deposited secrets, by design.
Secrets exit only via the capability-mediated `POST /secrets/fetch` path, which
requires a plugin identity and a valid cap. A bearer holder can write to a
vault, but cannot read back what is stored via the HTTP API.

This is an explicit security claim, not a deferred feature: the vault is a
secrets broker, not a password manager. See the top-level `README.md` for the
framing paragraph that should drive future "won't fix" decisions on this point.

### Per-tenant write serialisation

The write endpoints hold a per-tenant `tokio::sync::Mutex` for the duration of
each vault mutation. Concurrent writes for the same tenant serialise; concurrent
writes for different tenants proceed in parallel. No file-level CAS is used in
v1 — reserve that for multi-broker deployments.

### Lease lookup for vault unlock

Write handlers resolve the named tenant, find the most recent active lease for
that tenant (`revoked_at IS NULL`, `expires_at > now`, `idle_extends_to > now`),
and use in-memory lease material for that lease to unlock the vault master key.
If no active lease (or no in-memory lease material) is available, the endpoint
returns `503` with `error.code = "no_active_lease"` and asks the user to log in.
