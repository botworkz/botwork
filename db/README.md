# `db/` — botwork persistence layer

This directory holds the SeaORM-based persistence layer for the botwork
workspace. It lands the infrastructure that future entities and consumer
cutovers will sit on; v0 itself does not change any consumer's behaviour.

See [RFE #97](https://github.com/botworkz/botwork/issues/97) for design
context. Companion deploy-side PRs in `botworkz/vm` and `botworkz/space`
ship the postgres image, init oneshot, db-init/db-migrate units, and the
separate `<vm>-db.qcow2` disk.

## Crates

Two crates, following SeaORM's canonical entity + migration split:

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

Library crate. v0 ships **no entity modules** — the crate exists so that
`botwork-migration` and (eventually) every persistence-aware consumer
depends on a single source of truth for the schema. The first entity
arrives with the first consumer cut over to the DB.

Public surface:

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
  `MigratorTrait`. `migrations()` returns an empty vec in v0.
* `bin/botwork-migration` (`src/main.rs`) is the **production oneshot**.
  It connects via `connect_from_env`, runs `Migrator::up`, and exits.
  This is the binary the `botwork/db-migrate:local` container runs as
  its CMD.

The full `sea-orm-migration` operator CLI surface
(`status` / `down` / `fresh` / `refresh` / `reset`) is intentionally NOT
exposed here in v0 — see `src/main.rs` and RFE 97 (out-of-scope) for
the reasoning. It comes back as a feature-gated second binary once
there are real migrations whose state is worth inspecting.

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

- insert seed/fixture data (use a `botwork-tools` subcommand instead),
- paper over dev-vs-prod differences,
- conditionally branch on environment.

Seed/fixture data has separate homes:

- Test fixtures live in test code, inserted per test via
  `botwork-entity`.
- Dev/operator seed data lives in a `botwork-tools` subcommand
  (the eventual `botwork-tools plugins reconcile`), invoked by space-side
  bootstrap, idempotent, not part of `Migrator::up()`.

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
  2. the `seaql_migrations` tracking table exists,
  3. a second `Migrator::up` is also successful (idempotency — the
     production oneshot can restart safely).

  The test gates on docker availability and prints a structured
  `IGNORED` line when docker isn't reachable, so `cargo test` stays
  green on dev machines without docker. The end-to-end production-path
  proof lives in `containers.yml` (the `db-migrate` smoke step), which
  is gated on docker being present.

## Container image

`botwork/db-migrate:local`, built from `containers/db-migrate/Dockerfile`.

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
