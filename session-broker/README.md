# session-broker

## Architecture: per-session routing only

`session-broker` is the routing component. On every request it decides which
upstream container to forward traffic to, and on first contact it asks the
launcher to spin up a per-session container.

It does **not** own a static plugin registry: the parsed view of
`/etc/botwork/plugins.yaml` lives in
[`botwork-config-broker`](../config-broker/README.md) and is fetched per spawn
via `POST /resolve`. Spawn is the only path that talks to config-broker; the
hot, known-session routing path reads everything it needs off
`TransportState`.

```
goose / curl
    │
    ▼
  Envoy
    │  ext_authz → auth-broker        ── x-botwork-cap (mint)
    │  ext_proc  → session-broker
    │              ├─ /resolve        ──▶ config-broker  (spawn only)
    │              └─ /secrets/fetch  ──▶ auth-broker    (spawn only)
    │  
    └─ /launch ────────────────────────▶ launcher
```

## Per-tenant secrets injection

`session-broker` is the only component that handles `x-botwork-cap`.

On spawn (`POST` without `Mcp-Session-Id`), it exchanges the cap with
`BOTWORK_AUTH_BROKER_URL` (`/secrets/fetch`) and maps returned secrets to
container env vars of the form `BOTWORK_SECRET_<SERVICE>_<NAME>`.

Name mapping is documented in `src/secrets.rs` (`build_env_entries`), including
byte-wise sanitization and collision handling.

`session-broker` then passes those env vars to launcher `/launch` as optional
`env: [{name, value}]` entries. The MCP container itself does not call
auth-broker and does not need cap awareness.

Auth-broker fetch errors are fail-open: spawn continues with no injected
secrets, so control-plane/auth-broker issues do not take down the MCP data
plane. Config-broker errors, by contrast, are fail-closed: spawn cannot
proceed without a descriptor (see "Config-broker resolution" below).

## Config-broker resolution

Spawn-time descriptor resolution:

```
POST {BOTWORK_CONFIG_BROKER_ENDPOINT}/resolve
  { "tenant": "<tenant>", "namespace": "<ns>", "plugin": "<name>" }
```

Successful response is the plugin's full descriptor (image, port, network,
path, upstream_auth, resources, env, config_blob). See
[`config-broker/README.md`](../config-broker/README.md) for the wire shape.

### Failure modes

| Result                                  | Client-facing status                              |
|-----------------------------------------|---------------------------------------------------|
| Operator-fault 4xx (`unknown_plugin`, `invalid_namespace`, `invalid_request`) | Pass-through 4xx with same message |
| Server fault 5xx, transport error, timeout, garbage response | 502 with detail; spawn fails closed |

There is **no** retry on the spawn-path config-broker call in v0. Fail-closed
keeps the failure surface narrow and operator-debuggable; an outage of
config-broker is operationally identical to a failure of any other dependency
on the spawn path (already true of auth-broker on the bearer-token path).

### Caching

None in v0. Each spawn does one round-trip. Existing sessions are unaffected
by config-broker availability — once a session is bound the policy
(`upstream_auth`, `port`, `path`) is captured on `TransportState` and routing
proceeds locally without further config-broker calls.

## Plugin registry: ownership note

The fields documented below — `config`, `env`, `resources`, `upstream_auth`,
plus `image`, `port`, `network`, `path` — are the operator-facing shape of
`/etc/botwork/plugins.yaml`. **The file lives with config-broker, not
session-broker.** session-broker no longer reads it. The grammar and rules
documented here are still authoritative — they're enforced at config-broker
startup — but to edit the file you SSH to wherever config-broker runs and
restart that service. session-broker does not need to be bounced.

The remaining sections are kept here because the *injection semantics* (what
ends up in the container's env vars, in what order, with what reservations)
are session-broker's responsibility.

## Plugin registry: `config`

Each plugin in `/etc/botwork/plugins.yaml` may declare a structured, non-secret
`config:` mapping. config-broker serialises this to compact JSON; session-broker
injects it as `BOTWORK_MCP_CONFIG` in every spawned container for that plugin.

```yaml
plugins:
  github:
    image: botwork/mcp-github:local
    config:
      default_token_env: BOTWORK_SECRET_GITHUB_DEFAULT
      routes:
        - owner: botworkz
          token_env: BOTWORK_SECRET_GITHUB_BOTWORKZ
        - owner: phlax
          token_env: BOTWORK_SECRET_GITHUB_PHLAX
```

The plugin container receives `BOTWORK_MCP_CONFIG` as a compact-JSON string and
is responsible for parsing it.

### Plugin-side contract

- **Name.** `BOTWORK_MCP_CONFIG` — exact and stable; renaming is a breaking
  change for every plugin.
- **Shape.** Compact-JSON object (`{…}`).  config-broker guarantees it is valid
  JSON; the *content* under the top-level object is opaque pass-through.
- **Absence semantics.** If the operator did not set `config:` the variable is
  **not present** in the container env (same as unset `env:` entries).  Plugins
  must treat a missing variable the same as no config.
- **Stability.** Static per-plugin; read once at container startup.  The value
  does not change mid-session; a container restart is needed to pick up new
  config.
- **What config must NOT contain.** No secrets.  Route entries or feature flags
  may *reference* a secret by env-var name (e.g. `token_env:
  BOTWORK_SECRET_GITHUB_BOTWORKZ`), but the secret *value* is never in the
  config blob — it arrives separately via `BOTWORK_SECRET_*`.

### Operator rules

- The field is optional and defaults to absent (no injection).
- The value must be a YAML mapping; scalars and sequences at the top level are
  rejected at config-broker startup.
- The serialised JSON is capped at 64 KiB.
- `BOTWORK_MCP_CONFIG` is **reserved** — setting it directly in the `env:`
  mapping is a parse-time error.  Use `config:` instead.

### Injection order

The final env sent to the launcher is ordered:
1. Static plugin `env:` entries.
2. `BOTWORK_MCP_CONFIG` (if `config:` is set).
3. Vault-derived `BOTWORK_SECRET_*` entries.

## Plugin registry: `env`

Each plugin in `/etc/botwork/plugins.yaml` may declare a static, non-secret
`env:` mapping. These env vars are injected into every spawned container for
that plugin, regardless of whether a `x-botwork-cap` header is present.

```yaml
plugins:
  github:
    image: botwork/mcp-github:local
    upstream_auth: bearer/github.com
    env:
      GITHUB_TOOLSETS: default,actions
      GITHUB_TERSE_DESCRIPTIONS: "true"
```

### Rules

- The field is optional and defaults to empty.
- Keys must match `[A-Z_][A-Z0-9_]*`, must not be in the reserved set
  (`PATH`, `LD_PRELOAD`, `LD_LIBRARY_PATH`), must not start
  with `DOCKER_`, must not start with `BOTWORK_SECRET_` (reserved for
  vault-derived entries), and must not be `BOTWORK_MCP_CONFIG` (reserved for
  structured config injection — use the `config:` field). `HOME` and `USER`
  are intentionally **not** reserved and may be set per-plugin (e.g.
  `HOME: /workspace` for cache-heavy plugins like `bazel` or `cargo`).
- Non-string YAML scalars (booleans, integers) are **rejected at parse time**
  with a clear error suggesting the user quote the value.
- Values are capped at 64 KiB.
- At most 32 entries per plugin.
- Duplicate keys within a single plugin's `env:` are rejected.

Bad config causes config-broker startup to fail immediately with a message
naming the offending plugin and field. session-broker is unaffected.

### Merge order

When a container is spawned the final `env` array sent to the launcher is
built as: **static plugin env first, then `BOTWORK_MCP_CONFIG` (if set), then
vault-derived secrets**. This keeps the `BOTWORK_SECRET_*` block contiguous at
the end for easy scanning in logs.
The combined list is capped at 64 entries; if `static + config + secrets > 64`,
secrets are truncated (not static env or config).

## Plugin registry: `resources`

Each plugin may optionally define per-plugin launcher resource overrides:

```yaml
plugins:
  cargo:
    image: botwork/mcp-cargo:local
    resources:
      cpus: "4.0"
      memory: "4g"
      pids: 1024
```

- Any of `cpus`, `memory`, and `pids` may be omitted.
- `cpus`/`memory` must be non-empty strings; `pids` must be an integer in `1..=4294967295`.
- Unknown keys under `resources` are rejected at config-broker startup.
- session-broker forwards this block to launcher `/launch`; omitted fields fall back to launcher defaults.

## Plugin registry: `upstream_auth`

`/etc/botwork/plugins.yaml` supports `upstream_auth` per plugin with this
string grammar:

- `none` (default): broker strips `Authorization` before routing to the
  per-session container.
- `bearer/<service>`: broker resolves the single cap-visible vault secret tagged
  with `<service>` and sets `Authorization: Bearer <value>` on the upstream
  request.

`upstream_auth: bearer` without a service is a parse-time error. If the field is
omitted or `null`, it defaults to `none`.

### Security model

The client's seam bearer never reaches the per-session container.

These are two different credentials by design:

- vault password bearer authenticates **client -> seam**
- vault secret authenticates **seam -> upstream**

When `upstream_auth: bearer/<service>` is enabled, `session-broker` exchanges
`x-botwork-cap` with auth-broker, finds the single visible vault secret whose
`service` matches `<service>`, and mints the upstream `Authorization` header
from that secret. The policy (`bearer/<service>` or `none`) is captured on
`TransportState` at spawn time, so subsequent requests on the same session
make the strip-or-forward decision locally.

### Operator workflow

Add the upstream credential to the tenant vault with the service tag that the
plugin references:

```bash
botwork-vault add --root .vault/ \
  --tenant <tenant> \
  --service github.com \
  --name pat \
  --value '<token>'
```

Then set the plugin config, for example:

```yaml
plugins:
  mcp-github:
    image: botwork/mcp-github:local
    upstream_auth: bearer/github.com
```

If zero secrets match, or more than one secret matches the configured service,
spawn fails with a 5xx so operators can fix vault state explicitly.

## Environment variables

- `BOTWORK_SESSION_BROKER_ADMIN_ADDR` (default `0.0.0.0:9002`).
- `BOTWORK_SESSION_BROKER_GRPC_ADDR` (default `0.0.0.0:9001`).
- `BOTWORK_LAUNCHER_SOCKET_PATH` (default `/run/botwork/launcher.sock`).
- `BOTWORK_AUTH_BROKER_URL` (default `http://auth_broker:9100`).
- `BOTWORK_CONFIG_BROKER_ENDPOINT` (default `http://config_broker:9200`).
- `BOTWORK_BROKER_SOCKET_PATH` (default `/run/botwork/broker.sock`).
- `BOTWORK_SESSION_REGISTRY_PATH` (default `/var/lib/botwork/sessions.json`).
- `BOTWORK_BROKER_DISCONNECT_GRACE_SECS` (default `30`).

`BOTWORK_PLUGIN_REGISTRY_PATH` is no longer read by session-broker — it is now
config-broker's environment variable.
