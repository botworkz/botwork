# botwork-api

`botwork-api` is the HTTP+JSON CRUD service on top of
`botwork-entity`. It is the writer of the persistence layer
that bootstraps tenant/workspace/plugin/binding state.

See [RFE #106](https://github.com/botworkz/botwork/issues/106) for the
original design context. Phase 2 of
[botworkz/space#311](https://github.com/botworkz/space/issues/311)
retired the `/admin/api/v1/*` URL space in favour of the path-borne
tenant layout described below.

## Route table (Phase 2)

```text
GET /api/health                                          → { status, db }   (unauthed)

# Admin-gated (requires x-botwork-admin: true from auth-broker)
GET  /api/tenants                                        → { items: [...], total }
POST /api/tenants                                        → 201 Tenant + Location
GET  /api/tenants/{id}                                   → Tenant
PUT  /api/tenants/{id}                                   → 200 Tenant
DELETE /api/tenants/{id}                                 → 204 / 409

GET  /api/plugins                                        → { items: [...], total }
GET  /api/plugins/{id}                                   → Plugin
POST /api/plugins                                        → 201 Plugin + Location
PUT  /api/plugins/{id}                                   → 200 Plugin
DELETE /api/plugins/{id}                                 → 204 / 409

# Tenant-scoped (path tenant must match x-botwork-tenant header from auth-broker)
GET  /api/tenant/{tenant}/workspaces                     → { items: [...], total }
POST /api/tenant/{tenant}/workspaces                     → 201 Workspace + Location
GET  /api/tenant/{tenant}/workspaces/{id}                → Workspace
PUT  /api/tenant/{tenant}/workspaces/{id}                → 200 Workspace
DELETE /api/tenant/{tenant}/workspaces/{id}              → 204

GET  /api/tenant/{tenant}/workspace_plugins              → { items: [...], total }
    ?workspace_id=<uuid>&plugin_id=<uuid>                  (optional filters)
GET  /api/tenant/{tenant}/workspace_plugins/{wid}/{pid}  → WorkspacePlugin
POST /api/tenant/{tenant}/workspace_plugins              → 201 WorkspacePlugin
PUT  /api/tenant/{tenant}/workspace_plugins/{wid}/{pid}  → 200 WorkspacePlugin
DELETE /api/tenant/{tenant}/workspace_plugins/{wid}/{pid}→ 204

GET  /api/tenant/{tenant}/agent_sessions                 → { items: [...], total }
    ?state=active|reaped&live=true|false                   (optional filters)
GET  /api/tenant/{tenant}/agent_sessions/{id}            → AgentSession

GET  /api/tenant/{tenant}/session_workers                → { items: [...], total }
    ?live=true|false&agent_session_id=<uuid>               (optional filters)
GET  /api/tenant/{tenant}/session_workers/{id}           → SessionWorker

POST /api/tenant/{tenant}/secrets                        → 201 { stored, created } + Location
DELETE /api/tenant/{tenant}/secrets/{service}/{name}     → 204 / 404
```

**Deleted in Phase 2:** the entire `/admin/api/v1/*` route space is gone. There
is no compat shim. This is a "ships together or not at all" cut per [space#311].

**Not handled here:** `/api/auth/{login,logout,whoami}` — these are proxied to
`botwork-extra`'s auth-broker by envoy and never reach this service.

## Path-borne tenant contract

All tenant-scoped endpoints (`/api/tenant/{tenant}/*`) enforce:

1. **`x-botwork-tenant` header must be present** — injected by auth-broker after
   validating the bearer/cookie. Absent = 403 `cross_tenant_forbidden`.
2. **`x-botwork-tenant` value must match the URL `{tenant}` segment** — prevents
   cross-tenant access even if a misbehaving proxy sets the wrong header.
   Mismatch = 403 `cross_tenant_forbidden`.
3. **`x-botwork-admin: true` gates admin-only routes** — tenant list/mutations
   and plugin list/mutations. Absent = 403 `admin_required`.

This replaces the old "trust `x-botwork-tenant` verbatim" posture. The path IS
the identity; the header IS the authority; they must agree.

## Name validation

Tenant, workspace, and plugin names are validated by `botwork-api-core::names`:

- **Regex:** `^[A-Za-z0-9_-]{1,63}$`
- **Reserved (tenant-scope v1):** `["admin", "api", "auth", "static", "stats", "logs"]`
- **Case-sensitive storage**, **normalised-unique** — `Phlax` blocks creating `phlax`
- Create endpoints return:
  - `400 invalid_name` — name fails regex or length constraint
  - `400 reserved_name` — name is in the reserved list (tenant scope)
  - `409 already_exists` — normalised name already taken

The canonical source of the regex and reserved list is
`botwork-extra/auth-broker/src/grammar.rs`. `api-core` vendors it; see
`api-core/README.md`.

## Response shapes

* **Success body** — entity model serialised verbatim via SeaORM's
  derived `Serialize`. List endpoints wrap the body in
  `{ "items": [...], "total": N }`.
* **Error envelope:**

  ```json
  { "error": "<machine code>", "message": "<human detail>" }
  ```

  Error codes: `not_found`, `bad_request`, `invalid_name`, `reserved_name`,
  `cross_tenant_forbidden`, `admin_required`, `conflict`, `precondition_failed`,
  `internal`, `unavailable`.

## Secret store coupling

The secrets write endpoints (`POST /api/tenant/{tenant}/secrets`,
`DELETE /api/tenant/{tenant}/secrets/{service}/{name}`) forward to a
dedicated `secret_store` backend service. The tenant comes from the URL
path (no `tenant` field in the request body — that was dropped in Phase 2).

The wire contract:

* `POST   /secrets` — body `{ tenant, service, name, kind, value_b64, ... }`
* `DELETE /secrets/{service}/{name}?tenant={tenant}`

**Failure semantics:** 503 on backend unavailability, 409 propagated on duplicate,
404 propagated on missing delete.

Break-glass: `BOTWORK_API_DISABLE_SECRET_STORE=1`.

## Production invocation pattern

```bash
docker run --rm --name botwork-api \
  --network botwork-internal --network-alias admin_api \
  --user 1100:1100 \
  --env-file /var/lib/botwork-db/secret.env \
  -e BOTWORK_DATABASE_URL \
  botwork/api:local
```

## Environment variables

- `BOTWORK_DATABASE_URL` (required) — postgres URL.
- `BOTWORK_API_BIND` (default: `0.0.0.0:9400`) — bind address (never published).
- `BOTWORK_CONTROL_PLANE_ENDPOINT` (default: `http://control_plane:9300`) —
  live-state ack target. Break-glass: `BOTWORK_API_DISABLE_LIVE_GATE=1`.
- `BOTWORK_SECRET_STORE_ENDPOINT` (default: `http://secret_store:9500`).
- `BOTWORK_API_DISABLE_SECRET_STORE` — break-glass; secrets return 503.
- `RUST_LOG` — tracing-subscriber filter; defaults to `info`.

## Exit codes

| Code | Meaning                                                              |
|------|----------------------------------------------------------------------|
| 0    | normal exit (currently unreachable — `axum::serve` runs forever).    |
| 2    | `BOTWORK_DATABASE_URL` is not set.                                   |
| 3    | Connection to postgres failed.                                       |
| 4    | Failed to bind `BOTWORK_API_BIND`.                                   |
| 5    | `axum::serve` returned an error.                                     |

## Test posture

Integration tests in `tests/integration.rs` spin a real postgres via
`testcontainers`, apply the fixture bootstrap.yaml, and exercise all
endpoints including cross-tenant denial, name validation rejection, and
admin-gating. Requires Docker. Run with:

```bash
cargo test -p botwork-api
```

## References

- [botworkz/space#311](https://github.com/botworkz/space/issues/311) — Phase 2 design (canonical decision doc)
- [RFE #106](https://github.com/botworkz/botwork/issues/106) — original admin-api RFE
- [RFE #175](https://github.com/botworkz/botwork/issues/175) — api/ui rename
