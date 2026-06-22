# botwork-control-plane

`botwork-control-plane` owns the runtime view of every live plugin session
in a botwork deployment, and fans that view out to consumers that need it
in a push-shaped way. It ships two transports against the same
`Arc<SessionStore>`:

* **HTTP intake/read surface** on `:9300`. session-broker `POST`s on
  spawn, `DELETE`s on container exit, and either side can `GET` for a
  recovery sync.
* **xDS gRPC** on `:9301`. envoy's egress proxy subscribes via ADS
  (Aggregated Discovery Service); control-plane pushes a fresh LDS
  resource every time the session store mutates. The DFP cluster is
  static and pushed exactly once per stream. See
  [issue #81](https://github.com/botworkz/botwork/issues/81) for the
  full design.

There is intentionally no caller authentication yet — same posture as
config-broker and auth-broker. The trust boundary is the docker network:
control-plane joins `botwork-internal` and is reached by alias from
session-broker and (future) the egress envoy's xDS subscription.

## What it does (v0)

- Stores `SessionRecord`s keyed by `session_id`.
- Strict insert/delete: duplicate inserts and missing deletes both
  surface as 4xx so a control-plane / session-broker desync never
  passes silently.
- Validates wire input against the same shape regexes the rest of the
  botwork stack uses (session ids, tenant, workspace, plugin names,
  IPv4 container IP).
- Echoes any egress policy verbatim. v0 does not parse it; the schema
  is owned by config-broker (and grows in PR B alongside).

## What it does NOT do (v0)

- **No persistence of the in-memory store.** State is in-memory.
  Cold-start rebuilds the store by reading from postgres — see
  [Cold-start recovery](#cold-start-recovery) below. There is no
  on-disk snapshot in this binary.
- **No caller authentication.** Network membership is the access
  control. The deployment **must not** publish control-plane's HTTP
  or xDS port to the host (no `-p`/`--publish`).
- **No egress-policy schema enforcement.** The value is stored as
  opaque JSON. config-broker is the source of truth for the schema
  (botwork #88 enforces three forms: `all` / `none` / `{allow: [...]}`).
  control-plane parses the verbatim value at xDS compile time and
  fails closed on anything it can't recognise.

## Cold-start recovery

control-plane's `SessionStore` is in-memory: on every restart it
starts empty. If left alone, that breaks the xDS feeder the next time
control-plane restarts mid-deployment — SOTW xDS treats "absent from
snapshot" as "removed", so the egress envoy would silently tear down
every live route until each container exited and a fresh spawn
re-registered.

Post-RFE-#105-round-3 the broker stack landed two persistence-layer
tables that together hold every fact the live transport set carries:

* `session_worker` — one row per `mcp_session_*` container that
  session-broker has spawned. `reaped_at IS NULL` is the live tail;
  `container_name`, `container_ip`, `mcp_session_id`, and `plugin_id`
  are all populated by the broker's write-through path on spawn /
  initialize-response / teardown.
* `agent_session` — durable identity of a goose agent session keyed
  on `(tenant_id, workspace_id, agent_session_id)`. Carries the
  `state` lifecycle column (`active` / `grace` / `inactive` /
  `teardown_requested` / `purged`).

control-plane reads from those tables directly on cold start, runs a
single JOIN that mirrors the projection the old session-broker
recovery endpoint provided, and bulk-seeds the in-memory store before
binding either HTTP or xDS. The JOIN:

```sql
SELECT sw.mcp_session_id, sw.container_ip,
       t.name, w.name, p.name,
       p.egress
FROM   session_worker sw
JOIN   agent_session  a ON sw.agent_session_id = a.id
JOIN   workspace      w ON a.workspace_id      = w.id
JOIN   tenant         t ON a.tenant_id         = t.id
JOIN   plugin         p ON sw.plugin_id        = p.id
WHERE  sw.reaped_at IS NULL                     -- live container
  AND  sw.agent_session_id IS NOT NULL          -- past first-bind
  AND  sw.mcp_session_id   <> ''                -- past initialize
  AND  a.state IN ('active', 'grace')           -- addressable
```

The four predicates filter the rows session-broker would have
surfaced over its `transport_sessions` map — see
`recovery::fetch_live_sessions` for the rationale on each.

**Failure semantics (load-bearing):**

| postgres returns                  | Behaviour                                                                                          |
|-----------------------------------|----------------------------------------------------------------------------------------------------|
| `0 rows` (legitimate cold start)  | Store stays empty; control-plane binds.                                                            |
| `N rows`                          | Recovered N sessions; store populated; control-plane binds.                                        |
| `sea_orm::DbErr` / bad row        | Live state is unknown. Retry up to 30 × 5s, then **exit non-zero**. systemd's `Restart=always` keeps retrying. |

The "refuse to start on uncertainty" posture is deliberate. An empty
store is a *correct* recovery outcome only when the DB *tells us*
nothing was live; an empty store reached by guessing is a silent
break of the xDS feeder. session-broker's own in-process hard gate
(#82) means a control-plane that is mid-recovery produces 503s on
*new* spawns but does not break the running ones, so the
"refuse-to-start until postgres is reachable" loop is bounded and
observable.

The recovery loop can be disabled via
`BOTWORK_CONTROL_PLANE_DISABLE_RECOVERY=1` for break-glass scenarios
where the DB is unrecoverable. **This is not a supported production
posture** — setting it is an explicit decision to start with an
unknown live state.

**Sequencing.** Recovery requires postgres to be reachable. The
supported systemd order is `After=botwork-db-migrate.service` on the
control-plane unit (which transitively pulls in postgres + db-init).
**Crucially the pre-round-3 `After=botwork-session-broker.service`
edge is gone** — control-plane no longer needs session-broker
reachable to make recovery progress. The cycle that took down
botspace-01 on 2026-06-21 (`session-broker.After=control-plane` ↔
`control-plane.After=session-broker`) disappears with that edge.

## Endpoints

### `POST /sessions`

Request body (JSON):

```json
{
  "session_id":    "mcp_session_<token>",
  "container_ip":  "172.20.0.5",
  "tenant":        "phlax",
  "workspace":     "mcp",
  "plugin":        "fetch",
  "egress_policy": { "allow": [{"host": "github.com", "ports": [443]}] }
}
```

The accepted shapes for `egress_policy` (parsed here by
`policy::permissions_for_egress`):

* `"all"` or `{ "mode": "all" }` — unrestricted egress.
* `"none"` or `{ "mode": "none" }` — no policy emitted; envoy
  default-no-match denies.
* `{ "allow": [ {"host": "...", "ports": [443, ...]}, ... ] }` — exact
  `:authority` allowlist.

The `{ "mode": "..." }` object form is what config-broker 0.1.9+
normalises `egress: all` / `egress: none` from `plugins.yaml` into
before forwarding via session-broker (see
`config-broker::registry::parse_egress`). The bare-string sugar is
kept for older clients and direct callers; both encodings compile to
the same RBAC policy.

Anything else is **fail-closed**: no policy gets emitted for the
session, and the egress envoy denies its traffic. This matches the
hard-gate posture upstream (session-broker treats a bad ack as 503).

- `session_id` must match `^mcp_session_[a-z0-9]+$` (the shape
  session-broker constructs in `ext_proc.rs`).
- `container_ip` must be an IPv4 dotted-quad. IPv6 is not modelled in
  v0 because the broker stack assumes IPv4 throughout; bumping that is
  a schema change.
- `tenant`, `workspace`, `plugin` must each match
  `^[a-z][a-z0-9-]{0,30}$` — same rule as config-broker.
- `egress_policy` is optional on the wire; missing or `null` is
  stored verbatim and treated by the xDS compiler as fail-closed
  (no policy emitted → ALLOW + no match = denied). In practice
  every wire-side caller (session-broker) is supposed to set it,
  because config-broker's resolve always carries the field.

Success response (201):

```json
{ "status": "stored", "session_id": "mcp_session_<token>" }
```

`session_id` is echoed back so session-broker can sanity-check the ack
is for the record it sent; machines should branch on the HTTP status,
not the echoed id.

**Synchronous xDS ack gate** (new in 0.2.2 / #92): the 201 is NOT
returned until the egress envoy has ACKed the LDS push that carries
the new session's policy. If envoy is not connected (no xDS
subscriber) or doesn't ACK within `BOTWORK_CONTROL_PLANE_ACK_WAIT_MS`
(default 5000ms), the store mutation is rolled back and the handler
returns:

* `503 { "error": "no_xds_subscriber", "message": "..." }` — no
  egress envoy is currently subscribed.
* `503 { "error": "xds_ack_timeout", "message": "..." }` — subscribed
  but ACK did not arrive in time.

session-broker treats either 503 the same as before (hard-fail the
spawn, tear down the container, surface 503 to the client). The gate
closes the cold-start race where a freshly spawned plugin's first
tool call would 403 because xDS hadn't caught up; "201 from
control-plane" is now a contract that "the policy is live in envoy."

Operator break-glass: `BOTWORK_CONTROL_PLANE_DISABLE_ACK_WAIT=1`
flips the gate off and restores the pre-#92 behaviour where the
handler returns 201 as soon as the store mutation lands. Not a
supported posture — setting this is an explicit decision to accept
the cold-start race.

### `DELETE /sessions/<session_id>`

Idempotent in spirit but NOT in v0: a `DELETE` for an unknown id
returns 404, not 200. session-broker is expected to call DELETE
exactly once per session, from the container-exit listener. A second
DELETE for the same id is a bug worth surfacing.

Success response (200):

```json
{ "status": "removed", "session_id": "mcp_session_<token>" }
```

The same synchronous xDS ack gate applies to DELETE as to POST: 200
is not returned until envoy ACKs the LDS push that drops the
session's policy. On 503 the deletion is rolled back (the record is
re-inserted into the store) so the store keeps reflecting what envoy
actually has. This is load-bearing for `egress: none` sessions where
an in-flight SSRF-style request would otherwise leak through during
the window between delete and ACK.

### `GET /sessions/<session_id>`

Returns the stored `SessionRecord` verbatim. 404 if unknown.

### `GET /sessions`

Returns `{ "sessions": [ ... ] }` with records sorted by `session_id`.
Sort order is stable so human `curl /sessions` viewers and any future
cross-instance diffing scripts see consistent output.

## Error envelope

All non-2xx responses share the same shape (matches config-broker's
convention so callers can share retry/logging code):

```json
{ "error": "<machine code>", "message": "<human detail>" }
```

| Status | `error`           | When                                                                                |
|--------|-------------------|-------------------------------------------------------------------------------------|
| 201    | _(success body)_  | `POST` accepted, record stored.                                                     |
| 200    | _(success body)_  | `GET`/`DELETE` happy paths.                                                         |
| 400    | `invalid_request` | Body missing / non-JSON / required field absent / a field fails its shape check.    |
| 404    | `not_found`       | Path-supplied `session_id` is not in the store (applies to `GET` and `DELETE`).     |
| 409    | `already_exists`  | `POST` for a `session_id` already in the store.                                     |
| 500    | `internal`        | Reserved. Future use for store / disk / xDS push failures during a request.         |

## How session-broker treats responses (the hard gate)

This is the load-bearing design property: session-broker treats a
non-2xx from `POST /sessions` as a **hard fail for the session it was
about to hand off to envoy**. The session does not become reachable;
the spawned container is torn down via the existing exit path.

- 2xx → session-broker proceeds with the handoff.
- 4xx → bug in session-broker's payload (or schema drift between the
  two services). Session-broker maps to 500 to the originating client.
- 5xx OR transport / connect error / timeout → control-plane is
  degraded. Session-broker maps to 503. New sessions fail closed.

This is **not** a check that envoy will actually enforce policy — for
that, see the xDS push design in issue #81 — it is the smallest gate
that prevents an unpoliced container from ever serving a single
request.

## Environment variables

- `BOTWORK_DATABASE_URL` — postgres connection URL. Required; the
  binary exits 2 if unset. In production it is rendered by
  `botwork-db-init.service` into `/var/lib/botwork-db/secret.env` and
  surfaced to the unit via `EnvironmentFile=`. Same shape every
  other DB-bound consumer (config-broker, admin-api, session-broker)
  uses.
- `BOTWORK_CONTROL_PLANE_BIND` (default: `0.0.0.0:9300`) — HTTP bind
  address (session intake/read surface). The default is intentional:
  in the supported deployment, control-plane runs on the
  `botwork-internal` docker network with the `control_plane` alias,
  and its port is **never** published to the host. The docker network
  is the trust boundary, not the bind address. **Do not** add a port
  publish for this service.
- `BOTWORK_CONTROL_PLANE_XDS_BIND` (default: `0.0.0.0:9301`) — xDS
  gRPC bind address. envoy's ADS subscription connects here. Same
  trust boundary as the HTTP port; separate listener because tonic h2
  and axum h1 are different protocol stacks. The egress envoy
  bootstrap pins this endpoint by alias (`control_plane:9301`).
- `BOTWORK_CONTROL_PLANE_DISABLE_RECOVERY` — when set to `1`/`true`/
  `yes`, skip the cold-start recovery sync and start with an empty
  store. Break-glass only; see [Cold-start
  recovery](#cold-start-recovery) above for why this is not a
  supported posture. Name kept verbatim from the pre-round-3
  implementation — operator runbooks already grep for it; the
  semantics ("I accept the consequences of starting empty") are
  unchanged.
- `BOTWORK_CONTROL_PLANE_ACK_WAIT_MS` (default: `5000`) — per-request
  budget (milliseconds) for the synchronous xDS ack gate on
  `POST /sessions` and `DELETE /sessions/<id>`. envoy ACKs typically
  complete in <100ms; 5s gives plenty of headroom for the "envoy
  briefly paused / mid-config-load" case without blocking spawns
  indefinitely. Lower this in CI for faster failure surfaces, raise
  it if a future deployment puts more between envoy and
  control-plane. `0` is rejected — set
  `BOTWORK_CONTROL_PLANE_DISABLE_ACK_WAIT=1` to skip the gate
  entirely.
- `BOTWORK_CONTROL_PLANE_DISABLE_ACK_WAIT` — when set to `1`/`true`/
  `yes`, flip the synchronous xDS ack gate off; mutation handlers
  return 201/200 as soon as the in-memory store mutation lands. This
  restores the pre-#92 behaviour. Break-glass only; setting it
  accepts the cold-start race where a freshly spawned plugin's first
  tool call may 403 because xDS hadn't caught up.
- `RUST_LOG` — standard `tracing-subscriber` filter; defaults to
  `info`.

## xDS gRPC

control-plane exposes the envoy [Aggregated Discovery Service
(ADS)](https://www.envoyproxy.io/docs/envoy/latest/api-docs/xds_protocol)
on `:9301`. The egress envoy opens one bidi stream
(`StreamAggregatedResources`); over that stream control-plane serves
two resource types:

| Resource                                                              | Push trigger                                                                                       |
|-----------------------------------------------------------------------|----------------------------------------------------------------------------------------------------|
| `Listener` (`type.googleapis.com/envoy.config.listener.v3.Listener`)  | On initial subscribe **and** every time the session store mutates (insert / remove / bulk_seed).   |
| `Cluster` (`type.googleapis.com/envoy.config.cluster.v3.Cluster`)     | Once per stream. The dynamic_forward_proxy cluster is static; re-subscribes are silently ignored.  |

The compiled listener carries:

* One HCM with `CodecType::Http1` + a `CONNECT` upgrade entry (so
  envoy doesn't 400 the CONNECT before RBAC runs).
* RBAC filter (`action: ALLOW`) with one policy per session, keyed
  by `direct_remote_ip(container_ip)`. `egress: all` → permission
  `any: true`; `egress: {allow: [...]}` → one `:authority` exact
  match per host:port; `egress: none` → no policy emitted (denied
  by ALLOW + no match).
* dynamic_forward_proxy HTTP filter pointed at the
  `dynamic_forward_proxy_cache_config` DNS cache.
* router filter (terminal).

Filter order is **rbac → dfp → router** so denied CONNECTs are
short-circuited before envoy bothers resolving DNS for them.

SOTW only. `DeltaAggregatedResources` returns `unimplemented` to
force envoy back to the SOTW endpoint our bootstrap pins; never set
`ApiType::DeltaGrpc`.

NACKs (a `DiscoveryRequest` carrying `error_detail`) are logged and
held at the last-good version. envoy keeps the previous accepted
config. The next mutation triggers another push; we don't retry on
the NACK itself.

## Wire example

```
POST /sessions HTTP/1.1
Host: control_plane:9300
Content-Type: application/json

{"session_id":"mcp_session_abc","container_ip":"172.20.0.5","tenant":"phlax","workspace":"mcp","plugin":"fetch","egress_policy":{"allow":[{"host":"github.com","ports":[443]}]}}
```

Success response:

```
HTTP/1.1 201 Created
Content-Type: application/json

{"status":"stored","session_id":"mcp_session_abc"}
```

Duplicate POST response:

```
HTTP/1.1 409 Conflict
Content-Type: application/json

{"error":"already_exists","message":"session_id 'mcp_session_abc' already exists in control-plane store"}
```
