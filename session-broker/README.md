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
