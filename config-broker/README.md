# botwork-config-broker

`botwork-config-broker` resolves plugin descriptors for `botwork-session-broker`
on every spawn. It reads from the postgres-backed schema landed by
[RFE #101](https://github.com/botworkz/botwork/issues/101) and serves the
result over HTTP+JSON.

See RFE #101 for the cutover context — pre-cutover the broker loaded an
in-memory snapshot of `/etc/botwork/plugins.yaml`; post-cutover the
authoritative source is postgres, written to by `botwork-bootstrap` at every
boot.

## What it does

For each `POST /resolve` call, config-broker joins
`tenant -> workspace -> workspace_plugin -> plugin` and returns the resolved
descriptor (image, port, path, upstream_auth, env, resources, config_blob,
egress). session-broker uses that descriptor to launch the per-session
container and forwards the `egress` block to control-plane as the session's
policy.

Validation lives **on the write side** in `botwork-bootstrap`; the broker
trusts the DB. The only validation at request time is the regex shape of
the three names (`tenant`/`workspace`/`plugin`), which produces a clean 400
for malformed callers rather than letting a SQL identifier slip through.

## Endpoints

### `POST /resolve`

Request body (JSON):

```json
{ "tenant": "phlax", "workspace": "mcp", "plugin": "github" }
```

- All three fields are required.
- All three must match `^[a-z][a-z0-9-]{0,30}$`.

> Wire-shape rename (RFE #101 PR2): the field name changed from
> `namespace` to `workspace` to match the model. session-broker and the
> URL grammar at the edge changed in lock-step.

Success response (200):

```json
{
  "image": "botwork/mcp-github:local",
  "port": 8000,
  "path": "/",
  "upstream_auth": "bearer/github.com",
  "resources": { "memory": "4g", "pids": 1024 },
  "env": [ { "name": "GITHUB_TOOLSETS", "value": "default,actions" } ],
  "config_blob": "{\"routes\":[{\"owner\":\"botworkz\"}]}",
  "egress": {
    "allow": [ { "host": "api.github.com", "ports": [443] } ]
  }
}
```

- `image`, `port`, `path`, `upstream_auth` are always present.
- `resources` is always present but its individual fields (`cpus`, `memory`,
  `pids`) are each omitted when not set.
- `env` is always an array; may be empty.
- `config_blob` is **omitted** when the binding has no `config:` override
  (matching the existing absence-not-empty semantics for the env-var
  injection in session-broker). When present, it is already a compact-JSON
  string — session-broker drops it verbatim into `BOTWORK_MCP_CONFIG`.
- `upstream_auth` is one of `"none"` or `"bearer/<service>"`.
- `egress` is always present. The wire shape is normalised by bootstrap to
  one of:
  - `{ "mode": "all" }`
  - `{ "mode": "none" }`
  - `{ "allow": [{ "host": <bare-host>, "ports": [<u16>...] }, ...] }`

  config-broker forwards the block verbatim. The xDS materialiser owns the
  policy semantics; config-broker doesn't interpret it beyond storage.

### Error envelope

All non-2xx responses share the same shape:

```json
{ "error": "<machine code>", "message": "<human detail>" }
```

| Status | `error`             | When                                                                           |
|--------|---------------------|--------------------------------------------------------------------------------|
| 200    | _(success body)_    | Binding found, descriptor returned.                                             |
| 400    | `invalid_request`   | Body missing / non-JSON / required field absent / `tenant` or `plugin` malformed. |
| 400    | `invalid_workspace` | `workspace` does not match `^[a-z][a-z0-9-]{0,30}$`.                            |
| 404    | `unknown_plugin`    | No `(tenant, workspace, plugin)` binding row in the DB.                          |
| 500    | `internal`          | DB error during resolve.                                                        |
| 503    | `unavailable`       | Reserved. Future use for "infrastructure transiently degraded".                 |

session-broker maps these onto client-facing responses:

- 4xx → passes through (operator/client problem; same 4xx visible at the edge).
- 5xx OR transport error / timeout → 502 with the underlying detail
  (config-broker is upstream of session-broker for this hop). **Spawn fails
  closed.**

## Environment variables

- `BOTWORK_DATABASE_URL` (required) — postgres URL in the canonical
  `postgres://botwork:<password>@postgres/botwork` shape. Same env the
  rest of the workspace's persistence-aware consumers use.
- `BOTWORK_CONFIG_BROKER_BIND` (default: `0.0.0.0:9200`) — bind address. The
  default is intentional: in the supported deployment, config-broker runs on
  the `botwork-internal` docker network with the `config_broker` alias, and
  its port is **never** published to the host. The docker network is the
  trust boundary, not the bind address. **Do not** add a port publish for
  this service.
- `RUST_LOG` — standard `tracing-subscriber` filter; defaults to `info`.

## Security model (v0)

- **Credless.** No mutual auth between caller (session-broker) and config-broker.
  Network membership is the only access control.
- The trust boundary is the docker network. Anyone who can reach
  `config_broker:9200` can read every binding's image, env, and config blob.
- Plugin config is **not** secret material. Secrets are still produced via
  auth-broker / vault and never traverse this service. Config blobs may
  *reference* secret env names (e.g. `token_env: BOTWORK_SECRET_GITHUB_PAT`)
  — the *value* never appears in this service.

## Operator workflow

1. Edit `/etc/botwork/bootstrap.yaml`.
2. `systemctl restart botwork-bootstrap` (oneshot upsert).
3. config-broker picks up the new rows on its next `/resolve` — no
   broker restart required, no cache to invalidate.

Existing sessions keep running with their already-spawned container; the
new config applies to the next spawn.

## Out of scope (v0)

- Caching. Every `/resolve` runs the JOIN. v0 traffic doesn't justify
  shaving the cycles.
- Hot reload of bootstrap.yaml without `systemctl restart`.
- Per-binding overrides beyond `config:`.
- Schema validation of the `config_blob` content.
- Policy semantics of the `egress` block.
- Audit log surface, version stamping, dashboards.
- Caller authentication of any kind.

## Wire example

```
POST /resolve HTTP/1.1
Host: config_broker:9200
Content-Type: application/json

{"tenant":"phlax","workspace":"mcp","plugin":"github"}
```

Successful response:

```
HTTP/1.1 200 OK
Content-Type: application/json

{"image":"botwork/mcp-github:local","port":8000,"path":"/","upstream_auth":"bearer/github.com","resources":{"memory":"4g","pids":1024},"env":[{"name":"GITHUB_TOOLSETS","value":"default,actions"}],"config_blob":"{\"routes\":[{\"owner\":\"botworkz\"}]}","egress":{"allow":[{"host":"api.github.com","ports":[443]}]}}
```

Unknown-binding response:

```
HTTP/1.1 404 Not Found
Content-Type: application/json

{"error":"unknown_plugin","message":"no binding for tenant 'phlax' workspace 'mcp' plugin 'github'"}
```
