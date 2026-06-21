# botwork-admin-api

`botwork-admin-api` is the HTTP+JSON CRUD service on top of
`botwork-entity`. It is the future writer of the persistence layer
that today is owned by the `botwork-bootstrap` boot oneshot.

See [RFE #106](https://github.com/botworkz/botwork/issues/106) for the
design context — this is the v0 skeleton PR that lands the
service-shaped scaffolding and a single health endpoint. The entity
CRUD handlers land in PR2 once the write-side validators have been
lifted out of `bootstrap/` into a shared `botwork-admin-core` lib.

## What v0 ships

* one `GET /admin/api/v1/health` endpoint that returns
  `{ "status": "ok", "db": "reachable" | "unreachable" }` (`SELECT 1`
  probe; 200 in both arms so operators can curl the service even when
  the DB is sad);
* the container image (`botwork/admin-api:local`, distroless,
  uid 1100, same posture as config-broker);
* the `Earthfile` + `Makefile` + release workflow entries that build
  and push it alongside the other broker images;
* an end-to-end CI smoke that spins postgres + db-migrate + admin-api
  on a throwaway docker network and curls `/admin/api/v1/health` from
  a sibling client container.

The companion changes that put admin-api in the deployed VM image
(`shasset.yaml` pin, systemd unit, image-loader entry, goss assertion,
end-to-end smoke) land in a follow-up PR against `botworkz/vm` once
this image is published to GHCR.

The production v0 invocation pattern (matching the future systemd
unit `botwork-admin-api.service`):

```bash
docker run --rm --name botwork-admin-api \
  --network botwork-internal --network-alias admin_api \
  --user 1100:1100 \
  --env-file /var/lib/botwork-db/secret.env \
  -e BOTWORK_DATABASE_URL \
  botwork/admin-api:local
```

## Hitting it from on the VM

Once the vm-side companion PR lands and admin-api is running, the
service is reachable inside the docker network by alias. The
simplest curl from the VM host is via a one-shot client container
on the same network:

```bash
# From an SSH session on the VM, after the vm-side companion lands:
docker run --rm --network botwork-internal curlimages/curl:8.10.1 \
  http://admin_api:9400/admin/api/v1/health
# -> {"status":"ok","db":"reachable"}
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

systemd's `Restart=always` on `botwork-admin-api.service` (added in
the vm-side companion PR) picks up any non-zero exit and retries.

## Test posture

Same rails as `db/`, `bootstrap/`, and `config-broker/`:
[`testcontainers`](https://crates.io/crates/testcontainers) stays
under `[dev-dependencies]` (enforced by
`db/migration/tests/testcontainers_isolation.rs`) and no test reads
`BOTWORK_DATABASE_URL` (enforced by
`db/migration/tests/no_env_leakage.rs`).

The integration test `tests/integration.rs` spins a real postgres,
runs `Migrator::up`, binds the router on a random local port, and
asserts the health endpoint reports `db: "reachable"`. End-to-end
production-path proof lives in `.github/workflows/containers.yml`
(the `admin-api` smoke step).

## Container image

`botwork/admin-api:local`, built from `containers/admin-api/Dockerfile`.

Distroless `base-nossl-debian12:nonroot` runtime, same posture as
config-broker / control-plane. Built by `earthly +admin-api-image`
from the repo root (and by `make -C containers admin-api`).
