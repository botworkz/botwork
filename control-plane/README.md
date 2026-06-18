# botwork-control-plane

`botwork-control-plane` owns the runtime view of every live plugin session
in a botwork deployment, and fans that view out to consumers that need it
in a push-shaped way. v0 ships the **session store and HTTP intake/read
surface only**: session-broker `POST`s on spawn, `DELETE`s on container
exit, and either side can `GET` for a recovery sync. The xDS server that
turns the stored view into envoy resources lands in a follow-up PR (see
[issue #81](https://github.com/botworkz/botwork/issues/81)).

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
  botwork stack uses (session ids, tenant, namespace, plugin names,
  IPv4 container IP).
- Echoes any egress policy verbatim. v0 does not parse it; the schema
  is owned by config-broker (and grows in PR B alongside).

## What it does NOT do (v0)

- **No xDS server.** That's the next PR; this one is the agreed
  in-memory shape it will consume.
- **No persistence.** State is in-memory. Cold-start rebuilds the
  store by polling session-broker's `GET /control-plane/sessions`
  admin endpoint (see [Cold-start recovery](#cold-start-recovery)
  below); there is no on-disk snapshot.
- **No caller authentication.** Network membership is the access
  control. The deployment **must not** publish control-plane's port to
  the host (no `-p`/`--publish`).
- **No egress-policy schema enforcement.** The value is stored as
  opaque JSON. config-broker is the source of truth for the schema;
  control-plane is a fan-out, not a validator.

## Cold-start recovery

control-plane's `SessionStore` is in-memory: on every restart it
starts empty. If left alone, that breaks the (future) xDS feeder
the next time control-plane restarts mid-deployment -- SOTW xDS
treats "absent from snapshot" as "removed", so the egress envoy
would silently tear down every live route until each container
exited and a fresh spawn re-registered.

To avoid that, control-plane polls session-broker's
`GET /control-plane/sessions` admin endpoint on startup and bulk-
seeds the store before binding its own HTTP listener. session-broker
is the source of truth for the live transport set (it owns
`mcp_session_*` containers), so this is a one-way rebuild against
the authoritative state.

**Failure semantics (load-bearing):**

| session-broker returns | Behaviour                                                                                  |
|------------------------|--------------------------------------------------------------------------------------------|
| `200 { sessions: [] }` | Legitimate cold start with no live sessions. Store stays empty; control-plane binds.       |
| `200 { sessions: [N] }`| Recovered N sessions; store populated; control-plane binds.                                |
| transport / non-2xx / bad envelope / bad record | Live state is unknown. Retry up to 30 × 5s, then **exit non-zero**. systemd's `Restart=always` keeps retrying. |

The "refuse to start on uncertainty" posture is deliberate. An empty
store is a *correct* recovery outcome only when session-broker
*tells us* it's empty; an empty store reached by guessing is a
silent break of the xDS feeder. session-broker's own in-process
hard gate (#82) means a control-plane that is mid-recovery produces
503s on *new* spawns but does not break the running ones, so the
"refuse-to-start until session-broker is reachable" loop is bounded
and observable.

The recovery loop can be disabled via
`BOTWORK_CONTROL_PLANE_DISABLE_RECOVERY=1` for break-glass scenarios
where session-broker is unrecoverable. **This is not a supported
production posture** — setting it is an explicit decision to start
with an unknown live state.

**Sequencing.** Recovery requires session-broker to be reachable.
The supported systemd order is `After=botwork-session-broker.service`
on the control-plane unit, with `Wants=` (not `Requires=`) in either
direction: a hard mutual dependency deadlocks on first boot. The
in-process gate is what actually enforces the security property; the
systemd order is just sequencing convenience.

## Endpoints

### `POST /sessions`

Request body (JSON):

```json
{
  "session_id":    "mcp_session_<token>",
  "container_ip":  "172.20.0.5",
  "tenant":        "phlax",
  "namespace":     "mcp",
  "plugin":        "fetch",
  "egress_policy": { "allow_hosts": ["github.com"] }
}
```

- `session_id` must match `^mcp_session_[a-z0-9]+$` (the shape
  session-broker constructs in `ext_proc.rs`).
- `container_ip` must be an IPv4 dotted-quad. IPv6 is not modelled in
  v0 because the broker stack assumes IPv4 throughout; bumping that is
  a schema change.
- `tenant`, `namespace`, `plugin` must each match
  `^[a-z][a-z0-9-]{0,30}$` — same rule as config-broker.
- `egress_policy` is optional; missing or `null` means "no policy /
  default-open." It is stored verbatim.

Success response (201):

```json
{ "status": "stored", "session_id": "mcp_session_<token>" }
```

`session_id` is echoed back so session-broker can sanity-check the ack
is for the record it sent; machines should branch on the HTTP status,
not the echoed id.

### `DELETE /sessions/<session_id>`

Idempotent in spirit but NOT in v0: a `DELETE` for an unknown id
returns 404, not 200. session-broker is expected to call DELETE
exactly once per session, from the container-exit listener. A second
DELETE for the same id is a bug worth surfacing.

Success response (200):

```json
{ "status": "removed", "session_id": "mcp_session_<token>" }
```

### `GET /sessions/<session_id>`

Returns the stored `SessionRecord` verbatim. 404 if unknown.

### `GET /sessions`

Returns `{ "sessions": [ ... ] }` with records sorted by `session_id`.
Sort order is stable so the recovery-sync consumer (control-plane
restart → polls session-broker, compares snapshots) and human
`curl /sessions` viewers see consistent output.

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

- `BOTWORK_CONTROL_PLANE_BIND` (default: `0.0.0.0:9300`) — bind
  address. The default is intentional: in the supported deployment,
  control-plane runs on the `botwork-internal` docker network with the
  `control_plane` alias, and its port is **never** published to the
  host. The docker network is the trust boundary, not the bind
  address. **Do not** add a port publish for this service.
- `BOTWORK_SESSION_BROKER_ENDPOINT` (default:
  `http://session_broker:9002`) — session-broker's admin server,
  polled at startup for cold-start recovery. The default targets the
  alias session-broker registers on `botwork-internal`. Override when
  running control-plane out of the canonical docker network (e.g.
  local iteration against a loopback session-broker).
- `BOTWORK_CONTROL_PLANE_DISABLE_RECOVERY` — when set to `1`/`true`/
  `yes`, skip the cold-start recovery sync and start with an empty
  store. Break-glass only; see [Cold-start
  recovery](#cold-start-recovery) above for why this is not a
  supported posture.
- `RUST_LOG` — standard `tracing-subscriber` filter; defaults to
  `info`.

## Wire example

```
POST /sessions HTTP/1.1
Host: control_plane:9300
Content-Type: application/json

{"session_id":"mcp_session_abc","container_ip":"172.20.0.5","tenant":"phlax","namespace":"mcp","plugin":"fetch","egress_policy":{"allow_hosts":["github.com"]}}
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
