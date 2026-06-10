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
  (`PATH`, `HOME`, `USER`, `LD_PRELOAD`, `LD_LIBRARY_PATH`), must not start
  with `DOCKER_`, and must not start with `BOTWORK_SECRET_` (reserved for
  vault-derived entries).
- Non-string YAML scalars (booleans, integers) are **rejected at parse time**
  with a clear error suggesting the user quote the value.
- Values are capped at 64 KiB.
- At most 32 entries per plugin.
- Duplicate keys within a single plugin's `env:` follow YAML 1.2 semantics:
  the last value wins. This matches how `serde_yaml` and most other YAML
  loaders behave, so the file's surface meaning is preserved.

Bad config causes broker startup to fail immediately with a message naming the
offending plugin and field.

### Merge order

When a container is spawned the final `env` array sent to the launcher is
built as: **static plugin env first, then vault-derived secrets**. This keeps
the `BOTWORK_SECRET_*` block contiguous at the end for easy scanning in logs.
The combined list is capped at 64 entries; if `static + secrets > 64`,
secrets are truncated (not static env).

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
