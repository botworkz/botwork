# botwork-admin-api

`botwork-admin-api` is the HTTP+JSON CRUD service on top of
`botwork-entity`. It is the future writer of the persistence layer
that today is owned by the `botwork-bootstrap` boot oneshot.

See [RFE #106](https://github.com/botworkz/botwork/issues/106) for the
design context. PR1 landed the service-shaped scaffolding + a single
health endpoint; PR2 (this PR) adds read-side endpoints for all four
entities and extracts the per-entry validators into a shared
`botwork-admin-core` crate. The write endpoints (POST/PUT/DELETE,
delete-guards, xDS coupling) land in PR3.

## What ships post-PR3

```text
GET /admin/api/v1/health                            -> { status, db }

GET /admin/api/v1/tenants                           -> { items: [...], total }
POST /admin/api/v1/tenants                          -> 201 Tenant + Location
GET /admin/api/v1/tenants/{id}                      -> Tenant
PUT /admin/api/v1/tenants/{id}                      -> 200 Tenant
DELETE /admin/api/v1/tenants/{id}                   -> 204 / 409

GET /admin/api/v1/workspaces                        -> { items: [...], total }
    ?tenant_id=<uuid>                                  (optional filter)
GET /admin/api/v1/workspaces/{id}                   -> Workspace
POST /admin/api/v1/workspaces                       -> 201 Workspace + Location
PUT /admin/api/v1/workspaces/{id}                   -> 200 Workspace
DELETE /admin/api/v1/workspaces/{id}                -> 204

GET /admin/api/v1/plugins                           -> { items: [...], total }
GET /admin/api/v1/plugins/{id}                      -> Plugin
POST /admin/api/v1/plugins                          -> 201 Plugin + Location
PUT /admin/api/v1/plugins/{id}                      -> 200 Plugin
DELETE /admin/api/v1/plugins/{id}                   -> 204 / 409

GET /admin/api/v1/workspace_plugins                 -> { items: [...], total }
    ?workspace_id=<uuid>&plugin_id=<uuid>              (optional filters)
GET /admin/api/v1/workspace_plugins/{wid}/{pid}     -> WorkspacePlugin
POST /admin/api/v1/workspace_plugins                -> 201 WorkspacePlugin
PUT /admin/api/v1/workspace_plugins/{wid}/{pid}     -> 200 WorkspacePlugin
DELETE /admin/api/v1/workspace_plugins/{wid}/{pid}  -> 204

POST /admin/api/v1/secrets                          -> 201 { stored, created } + Location
DELETE /admin/api/v1/secrets/{service}/{name}       -> 204 / 404
```

Plus the unchanged infrastructure from PR1:

* the container image (`botwork/admin-api:local`, distroless,
  uid 1100, same posture as config-broker);
* the `Earthfile` + release workflow entries that build
  and push it alongside the other broker images;
* an end-to-end CI smoke that spins postgres + db-migrate + admin-api
  on a throwaway docker network and curls `/admin/api/v1/health` from
  a sibling client container.

### Response shapes

* **Success body** — entity model serialised verbatim via SeaORM's
  derived `Serialize`. List endpoints wrap the body in
  `{ "items": [...], "total": N }` so pagination
  (`?limit=&offset=`, `next_cursor`) can land later as a pure-additive
  change.
* **Error envelope** (mirrors config-broker / control-plane):

  ```json
  { "error": "<machine code>", "message": "<human detail>" }
  ```

  v0 emits `not_found`, `bad_request`, `internal`. PR3 adds
  `conflict` (delete-guard hit) and `precondition_failed` (optimistic
  lock lost).

### Hitting it from on the VM

Once admin-api is running on the deployed VM, the service is reachable
inside the docker network by alias. The simplest curl from the VM host
is via a one-shot client container on the same network:

```bash
# From an SSH session on the VM:
docker run --rm --network botwork-internal curlimages/curl:8.10.1 \
  http://admin_api:9400/admin/api/v1/tenants
# -> {"items":[{"id":"...","name":"phlax","created_at":"...","updated_at":"..."}],"total":1}
```

No host port is published. LAN exposure waits on the overlay
extending ext_authz to recognise an admin scope on `/admin/api/*`
(see RFE #106 § "Trust posture").

## Trust posture

* **No caller authentication in v0.** Same posture as config-broker
  and control-plane: the trust boundary is the docker network
  (`botwork-internal`), and the listener port (`9400`) is never
  `--publish`ed.
* The future operator-facing exposure comes from the ingress envoy
  adding an `/admin/api/*` route in front of the existing
  `envoy.filters.http.ext_authz` seam. admin-api itself stays
  credless and reads `x-botwork-tenant` (and, when the overlay adds
  it, `x-botwork-role`) verbatim from the request.
* Secrets endpoints follow the same posture as the rest:
  `x-botwork-tenant` is set by envoy ext_authz and trusted as the
  secret's scope; admin-api does no further authz. The tenant is
  read from the header, **never** from the URL path — this is by
  design and matches the rest of admin-api.

## Secret store coupling

The secrets write endpoints (`POST /admin/api/v1/secrets`,
`DELETE /admin/api/v1/secrets/{service}/{name}`) do not store
secrets in postgres. They forward to a dedicated `secret_store`
backend service over the `botwork-internal` docker network.

The backend is not part of `botwork` — composition is provided by
deployment (see `botworkz/space`). From admin-api's perspective the
backend is anonymous: it could be a cocoon-vault adapter, an HSM
proxy, or a test stub. The wire contract is a small, stable HTTP
seam:

* `POST   /secrets` — body `{ tenant, service, name, kind, value_b64, ... }`
* `DELETE /secrets/{service}/{name}?tenant={tenant}` — tenant as
  query param, never in the path

**Failure semantics:**

* `BOTWORK_ADMIN_API_DISABLE_SECRET_STORE=1` (break-glass) —
  admin-api returns 503 immediately with a message that names the
  flag. No request reaches the backend.
* Transport failure / backend 5xx — admin-api returns 503
  `unavailable` with "secret-store unavailable" in the message. The
  secret was NOT stored.
* Backend 409 — propagated as 409 `already_exists` (use
  `overwrite: true` to replace).
* Backend 404 on delete — propagated as 404 `not_found`.
* Backend 400 — propagated as 400 `bad_request` (bad base64, unknown
  kind, etc. — the backend is the authority on what is well-formed).

## Production invocation pattern

```bash
docker run --rm --name botwork-admin-api \
  --network botwork-internal --network-alias admin_api \
  --user 1100:1100 \
  --env-file /var/lib/botwork-db/secret.env \
  -e BOTWORK_DATABASE_URL \
  botwork/admin-api:local
```

Version probe: `botwork-admin-api --version` (or `-V`).

This is what the `botwork-admin-api.service` systemd unit (lives in
`botworkz/vm`) runs.

## Environment variables

- `BOTWORK_DATABASE_URL` (required) — postgres URL in the canonical
  `postgres://botwork:<password>@postgres/botwork` shape. Same env the
  rest of the persistence-aware consumers use.
- `BOTWORK_ADMIN_API_BIND` (default: `0.0.0.0:9400`) — bind address.
  The default is intentional: in the supported deployment admin-api
  runs on the `botwork-internal` docker network with the `admin_api`
  alias, and its port is **never** published to the host. The docker
  network is the trust boundary, not the bind address. **Do not** add
  a port publish for this service.
- `BOTWORK_CONTROL_PLANE_ENDPOINT` (default: `http://control_plane:9300`) —
  live-state ack target. Break-glass:
  `BOTWORK_ADMIN_API_DISABLE_LIVE_GATE=1` bypasses control-plane
  coupling. Not for production use.
- `BOTWORK_SECRET_STORE_ENDPOINT` (default: `http://secret_store:9500`) —
  secret-store backend endpoint. See "Secret store coupling" below.
- `BOTWORK_ADMIN_API_DISABLE_SECRET_STORE` (default unset) — break-glass;
  all secret writes return 503 immediately with a clear message. Not
  for production use.
- `RUST_LOG` — standard `tracing-subscriber` filter; defaults to
  `info`.

## Exit codes

| Code | Meaning                                                              |
|------|----------------------------------------------------------------------|
| 0    | normal exit (currently unreachable — `axum::serve` runs forever).    |
| 2    | `BOTWORK_DATABASE_URL` is not set.                                   |
| 3    | Connection to postgres failed.                                       |
| 4    | Failed to bind `BOTWORK_ADMIN_API_BIND`.                             |
| 5    | `axum::serve` returned an error (transport / shutdown failure).      |

systemd's `Restart=always` on `botwork-admin-api.service` picks up any
non-zero exit and retries.

## Test posture

Same rails as `db/`, `bootstrap/`, and `config-broker/`:
[`testcontainers`](https://crates.io/crates/testcontainers) stays
under `[dev-dependencies]` (enforced by
`db/migration/tests/testcontainers_isolation.rs`) and no test reads
`BOTWORK_DATABASE_URL` (enforced by
`db/migration/tests/no_env_leakage.rs`).

The integration test `tests/integration.rs` spins a real postgres,
runs `Migrator::up`, applies a fixture `bootstrap.yaml` (1 tenant,
1 workspace, 2 plugins, 2 bindings with one carrying a `config:`
blob) via `botwork_bootstrap::apply`, binds the router on a random
local port, and exercises every read endpoint plus the
list-with-filter shape. End-to-end production-path proof lives in
`.github/workflows/ci.yml` (the `admin-api` smoke step).

## Container image

`botwork/admin-api:local`, built from `admin-api/Dockerfile`.

Distroless `base-nossl-debian12:nonroot` runtime, same posture as
config-broker / control-plane. Built by `earthly +admin-api-image`
from the repo root (or by `docker build -f admin-api/Dockerfile .`).
