# botwork-login

Client-side OPAQUE login + lease bearer keyring management for
[`botwork-auth-broker`](../auth-broker).

Round 1a of [#123][rfe-123] / closes [#139][issue-139]. The CLI is the
thing the user runs *before* `goose session`: it drives an OPAQUE
handshake against the broker, persists the resulting bearer + lease
metadata in the OS keyring, and exposes a small set of subcommands
for inspecting / consuming / removing that state.

[rfe-123]: https://github.com/botworkz/botwork-extra/issues/123
[issue-139]: https://github.com/botworkz/botwork-extra/issues/139

## Why this exists

`botwork-auth-broker`'s round-1a OPAQUE endpoints
(`/auth/{register,login}/{start,finish}` — landed in
[#136][issue-136]) are HTTP-only. Without a client, the lease flow
is *endpoints with no caller*. `botwork-login` is the user-facing
deliverable that turns those endpoints into a thing an operator (or
a downstream tool / web UI / admin UI) can drive.

[issue-136]: https://github.com/botworkz/botwork-extra/issues/136

The library shape (`commands::*::run`) is deliberate: a future web UI
calls the same library entry points without shelling out, and the
typed-args structs (`LoginArgs`, `RegisterArgs`, …) accept a
library-supplied `password: Option<Zeroizing<Vec<u8>>>` so a
non-tty caller doesn't have to fake a stdin.

## Quick start

```sh
# One-time per tenant (operator-only):
$ botwork-login register --tenant phlax
Password: ********
Confirm password: ********
✓ Registered tenant 'phlax' (suite v1). Run `botwork-login --tenant phlax` to mint a lease.

# Per login session (~7 days default lease):
$ botwork-login --tenant phlax
Password: ********
✓ Logged in to phlax. Lease expires 2026-07-01T22:00:00+00:00 (in 6days 23h 59m).

# Offline keyring introspection:
$ botwork-login status --tenant phlax
phlax: logged in. Lease expires 2026-07-01T22:00:00+00:00 (in 6days 23h 59m).
       Lease id: 8f3e4a00-…
       Server: http://192.168.122.50:9100

# Shell-eval helper:
$ eval "$(botwork-login env --tenant phlax)"
$ goose session   # picks up ${BOTWORK_BEARER} via the extension config

# Drop the local entry (does NOT call any server-side revoke):
$ botwork-login logout --tenant phlax
✓ Removed keyring entry for phlax.
```

## Subcommands

| Subcommand | Purpose | Network? |
|---|---|---|
| `login` (default) | OPAQUE handshake, persist bearer in the keyring. | yes |
| `register` | Operator-flow OPAQUE registration. Run once per tenant. | yes |
| `status` | Read lease state + remaining time from the keyring. | no |
| `env` | Print `export <VAR>='<bearer>'` for shell consumption. | no |
| `logout` | Drop the keyring entry. v0 is keyring-only. | no |
| `--version` / `-V` | Print `botwork-login <version>` and exit 0. | no |

### `login` (default)

```
botwork-login [--tenant <TENANT>] [--lease <DURATION>] [--server <URL>]
              [--credential-identifier <ID>] [--cacert <PATH>] [--password-stdin]
```

- `--lease`: humantime (`7d`, `30d`, `12h`, `600s`). Default `7d`.
  Server-capped at `LEASE_HARD_CAP_SECONDS` (30d).
- `--password-stdin`: read one line from stdin instead of prompting.
- `--server`: override the broker URL (else env / config / default).
  **Must include an explicit `http://` or `https://` scheme.**
  Scheme-less values (e.g. `127.0.0.1:9100`) are rejected with a
  clear error rather than silently rewritten.
- `--credential-identifier`: OPAQUE credential identifier override
  (defaults to the tenant name).
- `--cacert`: path to a PEM CA certificate bundle to trust in addition
  to the built-in roots; falls back to `SSL_CERT_FILE` when omitted.

On wrong password: `incorrect password for tenant '<tenant>'`,
exit 1. On network error: user-readable message naming the URL,
exit 2. On 401 at `/auth/login/finish` for an *unknown* tenant the
client-side OPAQUE state machine catches `InvalidLogin` first, so
that arm also surfaces as the same wrong-password message — by
design, for enumeration resistance.

### `register`

Same shape as `login` but drives the registration handshake. Used
once per tenant by an admin / operator to seed the
`opaque_password_file` row. The broker's 404-on-unknown-tenant arm
maps to `LoginError::UnknownTenant`; 409-on-already-registered to
`LoginError::AlreadyRegistered` so the CLI prints a useful message
rather than a generic server error.

### `status`

```
botwork-login status --tenant <TENANT>
```

Reads the keyring entry, parses `expires_at`, prints remaining
time via `humantime::format_duration`. Exits 0 with a valid lease,
1 if no lease or expired. Offline — never touches the network.

### `env`

```
botwork-login env --tenant <TENANT> [--token-env <VAR>]
```

Prints `export BOTWORK_BEARER='<bearer>'` to stdout for shell
consumption via `eval "$(botwork-login env --tenant phlax)"` or
direnv `.envrc`. Exits 1 if no valid lease; prints *nothing* to
stdout in that case (so `eval` doesn't try to `export ''=`) and
the error goes to stderr.

### `logout`

v0 is keyring-only. Drops the local entry; does *not* call any
admin revoke endpoint (none exists yet). When the broker grows a
revoke endpoint, `logout` gains a `--revoke` flag.

## Configuration

`~/.config/botspace/config.toml` (or
`$XDG_CONFIG_HOME/botspace/config.toml`):

```toml
# Default broker URL. Must include an explicit http:// or https:// scheme.
# Scheme-less values (e.g. 192.168.122.50:9100) are rejected at startup.
server = "http://192.168.122.50:9100"

# Default token env var name for `env` output.
token_env = "BOTWORK_BEARER"

# Per-tenant overrides.
[tenants.phlax]
credential_identifier = "phlax"   # defaults to section name
```

Resolution order: **CLI flag > env var > config file > built-in default**.

Env vars:

- `BOTWORK_LOGIN_SERVER` — broker URL. Must include an explicit
  `http://` or `https://` scheme; scheme-less values are rejected.
- `SSL_CERT_FILE` — PEM CA certificate bundle path (used when
  `--cacert` is not passed).
- `BOTWORK_LOGIN_CONFIG` — config file path override (handy for tests).
- `BOTWORK_LOGIN_KEYRING_DIR` — file-fallback keyring root (handy for
  tests + headless deploys).

Built-in defaults:

- Server: `http://127.0.0.1:9100`
- Token env: `BOTWORK_BEARER`
- Lease: `7d`
- Credential identifier: tenant name

## Keyring storage

One JSON file per tenant at
`~/.config/botspace/keyring/<tenant>.json` (mode `0600`, atomic
tempfile + rename). Payload shape:

```json
{
  "bearer": "ABCDEF…",
  "lease_id": "8f3e4a00-…",
  "expires_at": "2026-07-01T22:00:00Z",
  "server": "http://192.168.122.50:9100",
  "credential_identifier": "phlax",
  "suite_version": 1
}
```

Path resolution (first match wins):

1. `BOTWORK_LOGIN_KEYRING_DIR` (tests + power users).
2. `XDG_CONFIG_HOME/botspace/keyring/`.
3. `HOME/.config/botspace/keyring/`.

### Why files, not an OS keyring backend

Round-1a deployments are libvirt VMs and docker containers —
neither runs a D-Bus session, so the `secret-service` keystore
that platform-native crates reach for is unreachable. v0
sidesteps the whole shape by using a single deterministic
filesystem path on every platform.

A future iteration can re-introduce OS-native backends (Keychain
on macOS, Credential Manager on Windows, `linux-keyutils` on
Linux) behind a Cargo feature with a deliberate per-platform
probe at startup — not silently through a third-party crate's
mock-on-no-backend fallback.

## Exit codes

| Variant family | Exit |
|---|---|
| `InvalidLogin` / `NoLease` / `LeaseExpired` / `UnknownTenant` / `AlreadyRegistered` / bad `--lease` / `Config` / `InvalidServer` | 1 |
| `Network` / `UnexpectedStatus` / `MalformedResponse` | 2 |
| `Keyring` | 3 |

## Library API

The CLI shim is intentionally thin. A future web / admin UI calls
the library directly:

```rust
use botwork_login::commands::login::{run as run_login, LoginArgs};

let outcome = run_login(LoginArgs {
    tenant: "phlax".into(),
    server: Some("http://192.168.122.50:9100".into()),
    password: Some(zeroize::Zeroizing::new(b"hunter2".to_vec())),
    lease: Some("30d".into()),
    ..LoginArgs::default()
}).await?;
```

The wire-level entry points live one layer down in
`botwork_login::client::{run_login, run_register}` — useful when a
caller wants the OPAQUE round-trip without the keyring side-effect.

## Out of scope (v0)

Per [#139][issue-139]:

- Auto-refresh / sliding lease renewal on the client side. The
  broker slides on `/auth/check`; the CLI just re-`login`s when
  asked. A future `refresh` subcommand can trade an existing
  bearer for a fresh one once the broker grows the matching
  endpoint.
- TUI / multi-tenant switcher.
- Web UI (this crate's library shape will accommodate; we ship
  the CLI only here).
- Lease revocation (admin endpoint doesn't exist yet).
- Change-password / OPAQUE re-registration flow (complex, needs to
  invalidate every outstanding lease for the tenant first).

## Tests

```sh
cargo test --locked --workspace --all-targets --features test-support
```

- **Unit tests** (25): duration parsing, config resolution, keyring
  JSON round-trip + file-fallback IO + path-traversal rejection,
  exit-code mapping, status-code mapping, response-body truncation
  (UTF-8 boundaries).
- **Integration tests** (docker-gated, log-skip when docker isn't
  reachable):
  - `tests/login_round_trip.rs` — register → login → status → env
    → /auth/check end-to-end against a real broker + postgres.
  - `tests/error_mapping.rs` — `InvalidLogin` / `UnknownTenant` /
    `AlreadyRegistered` arms against real wire 401 / 404 / 409.

Docker gating mirrors the pattern already used by `auth-broker`'s
`opaque_e2e` / `opaque_dummy` suites: the probe runs a small
sentinel image with a 5s timeout, and the test prints `IGNORED:
docker not reachable, skipping …` rather than failing when the
host has no docker.
