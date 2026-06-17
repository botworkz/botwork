# session-broker

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
plane.

## Plugin registry: `config`

Each plugin in `/etc/botwork/plugins.yaml` may declare a structured, non-secret
`config:` mapping. The broker serialises this to compact JSON and injects it as
`BOTWORK_MCP_CONFIG` in every spawned container for that plugin.

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
- **Shape.** Compact-JSON object (`{…}`).  The broker guarantees it is valid
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
  rejected at broker startup.
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

Bad config causes broker startup to fail immediately with a message naming the
offending plugin and field.

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
- Unknown keys under `resources` are rejected at broker startup.
- Broker forwards this block to launcher `/launch`; omitted fields fall back to launcher defaults.

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
from that secret.

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
