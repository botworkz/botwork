# botwork-config-broker

`botwork-config-broker` resolves plugin descriptors for `botwork-session-broker`
on every spawn. It owns the parsed view of `/etc/botwork/plugins.yaml` and
serves it over HTTP+JSON; session-broker no longer reads `plugins.yaml` itself.

This v0 is a **faithful relocation** of the registry that previously lived in
session-broker's old `plugin_registry.rs` (deleted in #79). There is no behaviour change: the same
validation rules apply, the same defaults are filled in, and namespace policy
is unchanged ("anything that matches the regex is fine"). Future work — per-
tenant overrides, ORM-backed sources, schema enforcement — slots in behind the
same wire contract without further changes to session-broker.

See [issue #75](https://github.com/botworkz/botwork/issues/75) for design
context.

## What it does

For each `POST /resolve` call, config-broker looks up `plugin` in the in-memory
registry loaded from `plugins.yaml` at startup and returns the resolved
descriptor (image, port, path, upstream_auth, env, resources,
config_blob). Session-broker uses that descriptor to launch the per-session
container.

## Endpoints

### `POST /resolve`

Request body (JSON):

```json
{ "tenant": "phlax", "namespace": "mcp", "plugin": "github" }
```

- All three fields are required.
- All three must match `^[a-z][a-z0-9-]{0,30}$`.
- `tenant` is shape-validated only in v0; the resolver does not key on it
  (per-tenant overrides are out of scope).

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
- `resources` is always present but is an object whose individual fields
  (`cpus`, `memory`, `pids`) are each omitted when not set.
- `env` is always an array; may be empty.
- `config_blob` is **omitted** when the operator did not set `config:` on the
  plugin (matching the existing absence-not-empty semantics for the env-var
  injection in session-broker). When present, it is already a compact-JSON
  string — session-broker drops it verbatim into `BOTWORK_MCP_CONFIG`.
- `upstream_auth` is one of `"none"` or `"bearer/<service>"`.
- `egress` is **omitted** when the operator did not set `egress:` on the
  plugin; an explicit `egress: {}` is preserved as `{}` (operator chose an
  empty policy, distinct from absent). When present it is a JSON object
  whose internal schema is **owned by control-plane** (botwork #81),
  not by config-broker — config-broker only verifies "must be a mapping"
  and shuttles the value through verbatim. session-broker is similarly
  opaque to the schema; it forwards the block to control-plane as the
  `egress_policy` of a `SessionRecord`.

### Error envelope

All non-2xx responses share the same shape:

```json
{ "error": "<machine code>", "message": "<human detail>" }
```

The HTTP status code is the canonical 4xx/5xx classification; the `error`
string is a stable machine-readable code that callers can branch on.

| Status | `error`             | When                                                                           |
|--------|---------------------|--------------------------------------------------------------------------------|
| 200    | _(success body)_    | Plugin resolved.                                                                |
| 400    | `invalid_request`   | Body missing / non-JSON / required field absent / `tenant` or `plugin` malformed. |
| 400    | `invalid_namespace` | `namespace` does not match `^[a-z][a-z0-9-]{0,30}$`.                            |
| 404    | `unknown_plugin`    | Plugin not in the registry loaded at startup.                                   |
| 500    | `internal`          | Reserved. Future use for parse / DB / source failures during a request.         |
| 503    | `unavailable`       | Reserved. Future use for "infrastructure transiently degraded".                 |

session-broker maps these onto client-facing responses:

- 4xx → passes through (operator/client problem; same 4xx visible at the edge).
- 5xx OR transport error / timeout → 502 with the underlying detail (config-broker
  is upstream of session-broker for this hop). **Spawn fails closed.**

## Environment variables

- `BOTWORK_PLUGIN_REGISTRY_PATH` (default: `/etc/botwork/plugins.yaml`) — path
  to the YAML registry. Refuses to start on parse / validation failure.
- `BOTWORK_CONFIG_BROKER_BIND` (default: `0.0.0.0:9200`) — bind address. The
  default is intentional: in the supported deployment, config-broker runs on
  the `botwork` docker network with the `config_broker` alias, and its port is
  **never** published to the host. The docker network is the trust boundary,
  not the bind address. **Do not** add a port publish for this service.
- `RUST_LOG` — standard `tracing-subscriber` filter; defaults to `info`.

## Security model (v0)

- **Credless.** No mutual auth between caller (session-broker) and config-broker.
  Network membership is the only access control.
- The trust boundary is the docker network. Anyone who can reach
  `config_broker:9200` can read every plugin's static config (image names,
  static env, config blobs).
- Plugin config is **not** secret material. Secrets are still produced via
  auth-broker / vault and never traverse this service. Config blobs may
  *reference* secret env names (e.g. `token_env: BOTWORK_SECRET_GITHUB_PAT`) —
  the *value* never appears in this service.
- The trust posture will tighten as the architecture evolves (sidecar vs
  remote, mTLS vs caps); the wire contract is designed to absorb that without
  the caller surface changing.

## Operator workflow

1. Edit `/etc/botwork/plugins.yaml` (the file format is unchanged from when
   session-broker owned it).
2. `systemctl restart botwork-config-broker` — hot-reload is intentionally not
   in v0.
3. Existing sessions keep running with their already-spawned container; the
   new config applies to the next spawn.

## Out of scope (v0)

- Per-tenant / per-namespace config overrides.
- ORM/DB-backed config sources.
- Schema validation of the `config_blob` content.
- Schema validation of the `egress` content (control-plane owns the schema;
  config-broker only enforces "must be a mapping").
- Hot reload (SIGHUP).
- Caching / TTL / invalidation hooks.
- Audit log surface, version stamping, dashboards.
- Caller authentication of any kind.

## Wire example

```
POST /resolve HTTP/1.1
Host: config_broker:9200
Content-Type: application/json

{"tenant":"phlax","namespace":"mcp","plugin":"github"}
```

Successful response:

```
HTTP/1.1 200 OK
Content-Type: application/json

{"image":"botwork/mcp-github:local","port":8000,"path":"/","upstream_auth":"bearer/github.com","resources":{"memory":"4g","pids":1024},"env":[{"name":"GITHUB_TOOLSETS","value":"default,actions"}],"config_blob":"{\\"routes\\":[{\\"owner\\":\\"botworkz\\"}]}"}
```

Unknown-plugin response:

```
HTTP/1.1 404 Not Found
Content-Type: application/json

{"error":"unknown_plugin","message":"unknown plugin: github"}
```
