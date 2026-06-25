# `db/` — botwork persistence layer

This directory holds the SeaORM-based persistence layer for the botwork
workspace. RFE #97 landed the rails (postgres in the stack, db-migrate
oneshot, empty migrator); this iteration (RFE #101) lays down the v0
schema — `tenant`, `workspace`, `plugin`, `workspace_plugin` — that
config-broker will read from after the wire cutover.

See [RFE #97](https://github.com/botworkz/botwork/issues/97) and
[RFE #101](https://github.com/botworkz/botwork/issues/101) for design
context. Companion oneshot — `bootstrap/` — translates the
`/etc/botwork/bootstrap.yaml` file into row upserts on every boot.

## Crates

| Crate              | Path             | Role                                                 |
|--------------------|------------------|------------------------------------------------------|
| `botwork-entity`   | `db/entity/`     | Per-table SeaORM entity modules; connection helpers. |
| `botwork-migration`| `db/migration/`  | `Migrator` + the production migrate-oneshot binary.  |

Why nested under `db/` instead of top-level `entity/` and `migration/` per
the SeaORM tutorial: the workspace documents services with their role
(`config-broker/`, `control-plane/`, …), and top-level `entity/` carries
no meaning on its own. Nesting under `db/` keeps the persistence layer
discoverable as a unit. Crate **names** (`botwork-entity`,
`botwork-migration`) are still flat so `cargo tree` output is unambiguous.

### `botwork-entity`

Library crate. v0 ships the following entity modules:

| Module                              | Role                                                                                   |
|-------------------------------------|----------------------------------------------------------------------------------------|
| `botwork_entity::tenant`            | Top-level account row. Globally-unique `name`.                                          |
| `botwork_entity::workspace`         | Tenant-scoped binding unit. `(tenant_id, name)` unique. Default name `mcp`.            |
| `botwork_entity::plugin`            | Globally-named package. `name` unique. Carries image + opaque egress JSON.             |
| `botwork_entity::workspace_plugin`  | Composite-PK binding row. Per-binding `config` blob (nullable).                        |
| `botwork_entity::connection`        | `connect(url)` / `connect_from_env()` helpers; the `BOTWORK_DATABASE_URL` contract.    |

Public connection surface (unchanged from RFE #97):

```rust
use botwork_entity::connection::{connect, connect_from_env, ConnectError, DATABASE_URL_ENV};
```

* `connect(url)` — explicit-URL constructor. Used by tests with a
  testcontainer-provided URL and by callers that compose the URL
  themselves.
* `connect_from_env()` — production entry point. Reads
  [`DATABASE_URL_ENV`] (`BOTWORK_DATABASE_URL`) from the process
  environment and delegates.

### `botwork-migration`

Library + binary.

* `lib.rs` exposes `pub struct Migrator` implementing
  `MigratorTrait`. v0 has exactly one migration:
  `m20260620_000001_create_core_tables` which lands the four tables,
  their FK relationships, and the supporting indexes.
* `bin/botwork-migration` (`src/main.rs`) is the **production oneshot**.
  It connects via `connect_from_env`, runs `Migrator::up`, and exits.
  This is the binary the `botwork/db-migrate:local` container runs as
  its CMD.

The full `sea-orm-migration` operator CLI surface
(`status` / `down` / `fresh` / `refresh` / `reset`) is intentionally NOT
exposed here in v0 — see `src/main.rs` and RFE #97 (out-of-scope) for
the reasoning. It comes back as a feature-gated second binary once
production needs an operator surface that exists outside admin-api.

## Schema (v0)

```
   tenant ─1:N─┐
               ├─ workspace ─M:N─ workspace_plugin ─N:1─ plugin
               ├─ agent_session ─1:N─ session_worker ─N:1─ plugin
               ├─ opaque_password_file               (0..1 per tenant)
               └─ lease ─1:N

   plugin ─N:1─ plugin_image_facet         (current_facet_id pointer)
```

The `tenant`/`workspace`/`workspace_plugin`/`plugin` quadrant is the
config-broker resolve surface (RFE #101). The
`agent_session`/`session_worker` pair is the durable identity +
per-incarnation projection for goose sessions (RFE #105). The
`opaque_password_file`/`lease` pair is the auth-broker persistence
layer (botworkz/botwork#141; parent RFE
[botworkz/botwork-extra#123](https://github.com/botworkz/botwork-extra/issues/123)).
The `plugin_image_facet` row + `plugin.current_facet_id` pointer is
the image-borne plugin-descriptor surface
([RFE #146](https://github.com/botworkz/botwork/issues/146); tracking
design [`botworkz/space#303`](https://github.com/botworkz/space/issues/303)).

### Tables

#### `tenant`

| Column      | Type          | Notes                                          |
|-------------|---------------|------------------------------------------------|
| `id`        | `uuid` PK     | `DEFAULT gen_random_uuid()`                    |
| `name`      | `text`        | UNIQUE — operator-typed slug (`phlax`)          |
| `created_at`| `timestamptz` | `DEFAULT CURRENT_TIMESTAMP`                    |
| `updated_at`| `timestamptz` | `DEFAULT CURRENT_TIMESTAMP`                    |

#### `workspace`

| Column      | Type          | Notes                                          |
|-------------|---------------|------------------------------------------------|
| `id`        | `uuid` PK     | `DEFAULT gen_random_uuid()`                    |
| `tenant_id` | `uuid`        | FK → `tenant.id` ON DELETE **RESTRICT**         |
| `name`      | `text`        | UNIQUE per tenant (`(tenant_id, name)`)        |
| `created_at`| `timestamptz` | `DEFAULT CURRENT_TIMESTAMP`                    |
| `updated_at`| `timestamptz` | `DEFAULT CURRENT_TIMESTAMP`                    |

The unique-index on `(tenant_id, name)` is intentional: every new
tenant gets a default workspace called `mcp`, so `name` alone is not
unique — many tenants can own a workspace called `mcp` without
collision.

#### `plugin`

| Column      | Type          | Notes                                          |
|-------------|---------------|------------------------------------------------|
| `id`        | `uuid` PK     | `DEFAULT gen_random_uuid()`                    |
| `name`      | `text`        | UNIQUE — globally-named package (`mcp-bash`)    |
| `image`     | `text`        | Docker image reference                         |
| `egress`    | `jsonb`       | Opaque to the storage layer; config-broker /  |
|             |               | control-plane own the schema                   |
| `created_at`| `timestamptz` | `DEFAULT CURRENT_TIMESTAMP`                    |
| `updated_at`| `timestamptz` | `DEFAULT CURRENT_TIMESTAMP`                    |

#### `workspace_plugin`

| Column         | Type        | Notes                                                      |
|----------------|-------------|------------------------------------------------------------|
| `workspace_id` | `uuid` PK#1 | FK → `workspace.id` ON DELETE **CASCADE**                  |
| `plugin_id`    | `uuid` PK#2 | FK → `plugin.id` ON DELETE **RESTRICT**                    |
| `config`       | `jsonb`     | Per-binding override. NULL = no override.                  |
| `created_at`   | `timestamptz`| `DEFAULT CURRENT_TIMESTAMP`                                |
| `updated_at`   | `timestamptz`| `DEFAULT CURRENT_TIMESTAMP`                                |

`PRIMARY KEY (workspace_id, plugin_id)` is the natural binding key.
A reverse-direction index `ix_workspace_plugin_plugin (plugin_id)`
exists for the future admin-api "where is plugin X used?" query.

#### `plugin_image_facet` (RFE #146)

| Column              | Type          | Notes                                                                  |
|---------------------|---------------|------------------------------------------------------------------------|
| `id`                | `uuid` PK     | `DEFAULT gen_random_uuid()`                                            |
| `plugin_name`       | `text`        | `org.botwork.mcp.name` (not an FK; see RFE #146)                       |
| `image_ref`         | `text`        | Resolved image reference (e.g. `botwork/mcp-echo:local`)               |
| `image_config_sha`  | `text`        | `docker image inspect`'s `Id`; together with `plugin_name` UNIQUE      |
| `spec_version`      | `text`        | `org.botwork.mcp.spec` (`"v1"` today)                                  |
| `port`              | `integer`     | `org.botwork.mcp.port` (u16-validated at upsert)                       |
| `path`              | `text`        | `org.botwork.mcp.path` (`/`-prefixed)                                  |
| `upstream_auth`     | `text`        | `"none"` or `"bearer/<service>"` — same wire form as `plugin`          |
| `egress`            | `jsonb`       | `org.botwork.mcp.egress`; same wire shape as `plugin.egress`           |
| `resources`         | `jsonb` NULL  | `{cpus?, memory?, pids?}` container caps                               |
| `env`               | `jsonb`       | `[{name, value}, ...]` static env (default `'[]'`)                     |
| `isolation`         | `text`        | `"shared" \| "per_agent_session" \| "per_request"`                     |
| `capabilities`      | `jsonb`       | MCP `capabilities` object from `initialize`                            |
| `tools`             | `jsonb`       | Denormalised tool array, pre-prefixed (default `'[]'`)                 |
| `resources_catalog` | `jsonb`       | Denormalised resource array (default `'[]'`)                           |
| `prompts`           | `jsonb`       | Denormalised prompt array (default `'[]'`)                             |
| `protocol_version`  | `text`        | MCP wire pin (e.g. `"2024-11-05"`)                                     |
| `spill_policy`      | `jsonb` NULL  | `{mode, threshold_bytes, include_methods, include_tools}` or NULL      |
| `observed_at`       | `timestamptz` | `DEFAULT CURRENT_TIMESTAMP` — first time the catalog saw this image    |

Insert-only. `(plugin_name, image_config_sha)` is the natural key
(UNIQUE); re-observations of the same image labels are no-ops via
the catalog upserter's `ON CONFLICT DO NOTHING`. JSON columns for
`tools`/`resources_catalog`/`prompts` keep `/resolve` as one row out
even though those fields are arrays of objects (no predicate runs
against them; see RFE #146 for the side-tables-vs-jsonb rationale).
No reader / writer wires this up in this PR — RFE #146 is the
schema landing only.

#### `plugin.current_facet_id` (RFE #146)

`plugin` gains an optional `current_facet_id uuid NULL` column with a
FK to `plugin_image_facet.id` ON DELETE **RESTRICT**. NULL during
the rollout window where the catalog hasn't observed a facet for
this plugin yet (config-broker's `/resolve` continues to read off
`plugin` directly in that window); the future
`botwork-image-catalog` oneshot repoints it after each ingest.

The supporting `ix_plugin_current_facet (current_facet_id)` btree
index lands in the same migration so the eventual `/resolve` JOIN
through the pointer doesn't have to alter `plugin` a second time.

#### `opaque_password_file` (botworkz/botwork#141)

| Column          | Type          | Notes                                                                  |
|-----------------|---------------|------------------------------------------------------------------------|
| `id`            | `uuid` PK     | `DEFAULT gen_random_uuid()`                                            |
| `tenant_id`     | `uuid`        | UNIQUE, FK → `tenant.id` ON DELETE **CASCADE**                         |
| `password_file` | `bytea`       | `opaque-ke` registration output (RFC draft-irtf-cfrg-opaque-13 §3.1)   |
| `suite_version` | `integer`     | `NOT NULL DEFAULT 1` — placeholder for future suite rotation           |
| `created_at`    | `timestamptz` | `DEFAULT CURRENT_TIMESTAMP`                                            |
| `updated_at`    | `timestamptz` | `DEFAULT CURRENT_TIMESTAMP`                                            |

One row per tenant in v0. Auth-broker reads this on every login
handshake to compute its half of the OPAQUE protocol. The blob is
opaque to postgres — no `@>` predicates needed.

#### `lease` (botworkz/botwork#141)

| Column               | Type          | Notes                                                          |
|----------------------|---------------|----------------------------------------------------------------|
| `id`                 | `uuid` PK     | `DEFAULT gen_random_uuid()`                                    |
| `tenant_id`          | `uuid`        | FK → `tenant.id` ON DELETE **CASCADE**                         |
| `bearer_hash`        | `bytea`       | UNIQUE — SHA-256 of the bearer; bearer plaintext never stored  |
| `wrapped_export_key` | `bytea`       | OPAQUE `export_key` sealed under a broker-side wrapping key    |
| `issued_at`          | `timestamptz` | `DEFAULT CURRENT_TIMESTAMP`                                    |
| `expires_at`         | `timestamptz` | Hard ceiling; client request capped at tenant `max_lease`      |
| `idle_extends_to`    | `timestamptz` | `min(expires_at, now + idle_window)`; bumped on each use       |
| `revoked_at`         | `timestamptz` | NULL = live; non-NULL = terminal audit state                   |

One row per outstanding lease. Auth-broker INSERTs on successful
OPAQUE login, looks up by `bearer_hash` on every request to
validate-and-extend, UPDATEs `idle_extends_to` on each use, and sets
`revoked_at` on explicit revoke / password change / admin action. The
partial index `ix_lease_live (tenant_id, expires_at) WHERE revoked_at
IS NULL` is the hot path for both the operator "list my active
leases" surface and the janitor's expired-row sweep — keeping the
revoked audit tail out of the index keeps it cheap as the tail
grows.

### FK semantics, named

* **`workspace.tenant_id` → `tenant.id` ON DELETE RESTRICT.**
  Deleting a tenant with workspaces must be deliberate (drop the
  workspaces first). Prevents a stray DELETE in some future migration
  from cascade-wiping every binding.
* **`workspace_plugin.workspace_id` → `workspace.id` ON DELETE CASCADE.**
  A binding without a workspace is meaningless; deleting the workspace
  tears down its bindings in the same statement.
* **`workspace_plugin.plugin_id` → `plugin.id` ON DELETE RESTRICT.**
  A plugin in use anywhere must be disabled everywhere before it can
  be removed. The future admin-api's "delete plugin" surface walks the
  reverse-index and refuses the operation if any bindings still point
  at it.
* **`opaque_password_file.tenant_id` → `tenant.id` ON DELETE CASCADE**
  (botworkz/botwork#141). The OPAQUE registration blob is meaningless
  without the tenant it authenticates; cascading on delete keeps the
  janitor out of the loop. The deliberate-two-step posture still lives
  one layer up at `workspace.tenant_id` RESTRICT.
* **`lease.tenant_id` → `tenant.id` ON DELETE CASCADE**
  (botworkz/botwork#141). Same posture: a lease without a tenant is
  meaningless.
* **`plugin.current_facet_id` → `plugin_image_facet.id` ON DELETE
  RESTRICT** (RFE #146). Deleting a facet a live `plugin` row points
  at would silently break that plugin's `/resolve` once the resolve
  cutover lands; RESTRICT keeps the upserter's insert-only posture
  safe. A future janitor that prunes old facets has to walk
  `plugin.current_facet_id` first and either re-point or refuse.

### Resolve hot-path

The query that config-broker will run on every `POST /resolve` (post-
cutover):

```sql
SELECT p.image, p.egress, wp.config
FROM   plugin p
JOIN   workspace_plugin wp ON wp.plugin_id    = p.id
JOIN   workspace        w  ON w.id            = wp.workspace_id
JOIN   tenant           t  ON t.id            = w.tenant_id
WHERE  t.name = $1 AND w.name = $2 AND p.name = $3;
```

Every WHERE-clause column has an index supporting it (UNIQUE indexes
on `tenant.name` and `plugin.name`, the composite UNIQUE on
`workspace`).

### JSONB columns

`plugin.egress` and `workspace_plugin.config` are `jsonb` (not `json`).
The decision to keep these opaque at the storage layer was deliberately
deferred until a real query forces structure — see RFE #101 §
"JSONB vs typed columns". `jsonb` keeps `@>`/`?` predicates and GIN
indexes available without rewriting the column type later.

## Wire / env contract

Single env var, URL-shaped, read at consumer startup:

```
BOTWORK_DATABASE_URL=postgres://botwork:<password>@postgres/botwork
```

- Hostname `postgres` is the docker network alias on `botwork-internal`.
- DB name `botwork`, role `botwork` (not `postgres` superuser).
- Password is the random seed generated on first boot by the
  space-side bootstrap, written to `/var/lib/botwork-db/secret.env`
  (mode 0600). Consumers mount it via systemd `EnvironmentFile=`.
- URL composition lives in the space-side bootstrap oneshot, not in any
  rust binary. Rust just reads the composed URL.
- Pool-tuning knobs are deliberately absent in v0; consumers get the
  SeaORM defaults. Tuning lands per-consumer in a follow-up once we have
  a real workload to measure.

## The schema-only rail

> A `MigrationTrait` impl in `botwork-migration` **describes schema only**.
> It does not insert data, does not seed fixtures, does not paper over
> environment differences.

Why this matters: migrations run unconditionally in `Migrator::up()` on
every restart of the migrate oneshot in production. Anything in there
ships to every deployment.

Concretely, do not:

- insert seed/fixture data (use the `bootstrap/` crate instead),
- paper over dev-vs-prod differences,
- conditionally branch on environment.

Seed/fixture data has separate homes:

- Test fixtures live in test code, inserted per test via
  `botwork-entity`.
- Operator seed data lives in `bootstrap/` (the `botwork-bootstrap`
  binary), invoked by the `botwork-bootstrap.service` systemd oneshot
  ordered between db-migrate and config-broker. Idempotent across
  reboots.

This is convention, not enforced. PR review is the gate.

## Test posture — testcontainers, isolated from production env

Tests use [`testcontainers`](https://crates.io/crates/testcontainers) and
[`testcontainers-modules`](https://crates.io/crates/testcontainers-modules)
to spin a real postgres for each integration test. Two rails keep
production paths sealed off from test code; both are CI-enforced.

### Rail 1: `testcontainers` is dev-only

`testcontainers` and `testcontainers-modules` MUST appear only under
`[dev-dependencies]`. The test `db/migration/tests/testcontainers_isolation.rs`
walks every workspace member's `Cargo.toml` and asserts neither crate
appears under `[dependencies]` (or any `[target.<cfg>.dependencies]`
runtime table).

A future move to `cargo-deny` could subsume this check; for now the in-tree
test is portable and zero-dep.

### Rail 2: tests never read `BOTWORK_DATABASE_URL`

The test `db/migration/tests/no_env_leakage.rs` walks every workspace
member's `tests/` directory and asserts no `.rs` file mentions
`BOTWORK_DATABASE_URL` (or the helpers that read it). Tests must obtain
their `DatabaseConnection` via `botwork_entity::connection::connect(url)`
with the URL from a testcontainer's mapped host port — never via
`connect_from_env`.

This guarantees no test can accidentally point at a real postgres if
the env var happens to be set in the runner's environment.

### Smoke surface

* `db/entity/`: unit tests in `src/connection.rs` cover error wiring
  without docker (malformed URL → structured `DbErr`).
* `db/migration/`: integration test `tests/migrate_smoke.rs` spins a real
  postgres via testcontainers and asserts:
  1. `Migrator::up` succeeds against an empty DB,
  2. the four v0 tables exist + the `seaql_migrations` row records the
     single v0 migration,
  3. a second `Migrator::up` is also successful and doesn't re-insert
     the tracking row (idempotency — the production oneshot can restart
     safely),
  4. the three named FK constraints have the expected `ON DELETE`
     actions (RESTRICT / CASCADE / RESTRICT),
  5. two tenants can each own a workspace called `mcp` without
     collision (the composite-uniqueness rail).

  The test gates on docker availability and prints a structured
  `IGNORED` line when docker isn't reachable, so `cargo test` stays
  green on dev machines without docker. The end-to-end production-path
  proof lives in `db/migration/smoke.sh` (invoked by the `db-migrate`
  job in `.github/workflows/ci.yml`), which is gated on
  docker being present.

## Container image

`botwork/db-migrate:local`, built from `db/migration/Dockerfile`.

Production invocation pattern:

```
docker run --rm \
  --network botwork-internal \
  --env-file /var/lib/botwork-db/secret.env \
  botwork/db-migrate:local
```

The container runs as the broker uid (1100), per the workspace
convention, and exits as soon as `Migrator::up` returns. systemd
`Type=oneshot` on the `botwork-db-migrate.service` unit picks up the
exit code; non-zero blocks every subsequent broker unit on the boot
chain.

Exit codes (see `src/main.rs`):

| Code | Meaning                                                              |
|------|----------------------------------------------------------------------|
| 0    | Migrations applied (none pending is a valid result).                 |
| 2    | `BOTWORK_DATABASE_URL` is not set.                                   |
| 3    | Connection to postgres failed.                                       |
| 4    | A migration ran and failed.                                          |
