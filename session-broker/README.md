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

`/etc/botwork/plugins.yaml` supports `upstream_auth` per plugin:

- `none` (default): broker strips `Authorization` before routing to the
  per-session container.
- `bearer`: broker forwards the incoming `Authorization` header to the
  per-session container unchanged.

If `upstream_auth` is omitted or `null`, it defaults to `none`.
Unknown values are rejected at registry-load time.

### Security implication of `bearer`

When `upstream_auth: bearer` is enabled, whatever the client sent in
`Authorization` reaches the per-session container untouched.
With the current seam this is typically the tenant vault password bearer, so
this mode effectively requires one value to satisfy both seam auth and upstream
auth. This can be an acceptable single-tenant dev/test stopgap, but it is not
an acceptable general production model.

### Future work

A future `vault` mode is planned where auth-broker mints a per-request upstream
`Authorization` from a vault-stored secret. The parser is intentionally strict
about unknown `upstream_auth` values so this can be added later as an additive
change.

### Ops note for downstream consumers

Downstream listeners that strip `authorization` unconditionally in Lua must be
updated for `upstream_auth: bearer` to work end-to-end. `session-broker` is the
authority for this decision; downstream consumers (for example
`botworkz/space`) need a follow-up change to remove unconditional stripping.
