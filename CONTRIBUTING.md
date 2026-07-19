# Contributing to botwork

## Coverage

### Running coverage locally

Install `cargo-tarpaulin` once:

```sh
cargo install cargo-tarpaulin --locked
```

Then run coverage from the repository root:

```sh
make coverage
# or equivalently:
DOCKER_HOST=unix:///nonexistent cargo tarpaulin
```

Both commands pick up `tarpaulin.toml` automatically and write an Lcov
report into a `coverage/` directory.

> **Local vs CI:** local coverage uses `cargo tarpaulin` (reads
> `tarpaulin.toml`). CI uses `grcov` over LLVM profraw data. The
> exclusion lists in the two tools must be kept in sync — see
> `tarpaulin.toml` and the grcov `--ignore` blocks in
> `.github/workflows/_crate.yml` and `.github/workflows/ci.yml`.

### What is measured

Coverage is **unit-test only** — the same tests that `cargo test
--workspace --all-targets` runs, excluding:

- `botwork-ui-wasm`: this crate only targets `wasm32-unknown-unknown`
  and cannot be built for the host.

### What is excluded from the coverage denominator

The following paths are listed in `tarpaulin.toml` under `exclude-files`
so they do not inflate (or deflate) the reported percentage:

| Pattern | Reason |
|---|---|
| `db/entity/src/*` | SeaORM `#[derive(DeriveEntityModel)]` generated entity models — macro-expanded glue with no meaningful branch logic. |
| `db/migration/src/*` | Migration definitions (`lib.rs`, `main.rs`, every `m2026*.rs`) — these run only against a live database via the migration runner and are exercised by integration/CI steps, not unit tests. |
| `*/main.rs` | Process-bootstrap entry points across every workspace binary (api, session-broker, control-plane, config-broker, botwork-cli, launcher, ui/server) — arg-parsing shell, tokio-runtime setup, and server bind/serve wiring that is not meaningfully unit-testable. |

To update this list, edit `tarpaulin.toml` (`exclude-files`) and update
this table, **and** update the matching `--ignore` args in
`.github/workflows/_crate.yml` and `.github/workflows/ci.yml` to keep
the CI grcov exclusions in sync.

### Two test tiers

- **Unit tier (no docker):** for `botwork-api`, use the store seam
  in `api/src/store/` (approach A): handler tests should inject the
  in-memory domain mocks from `api/src/store/mock.rs`; SeaORM-backed
  store behavior can still be unit tested with `sea_orm::MockDatabase`
  where needed. Reference shape: `botwork-extra/auth-broker/src/store/`.
  This is the tier measured by the no-docker tarpaulin job.
- **Integration tier (docker):** use `testcontainers` + real postgres
  to exercise real SQL, constraints, and transaction behaviour. This
  tier runs in crate smoke/CI jobs and is not instrumented by the
  no-docker tarpaulin run.

`MockDatabase` is a **fixture, not a database**: it replays canned
query/exec results in queue order and does not enforce SQL semantics,
constraints, ordering guarantees, or full transaction semantics. Keep
SQL correctness checks (joins/filters/constraints/FK behaviour) in the
integration tier; use unit tests for handler control flow, auth/header
gates, parse/validation branches, and error mapping.

### Env-mutation tests (interim pattern)

When a test must mutate process-global env vars (`std::env::set_var` /
`remove_var`), use both:

- `#[serial(env)]` from `serial_test` to serialize env-mutating tests.
- `botwork_test_support::EnvGuard::apply(...)` to snapshot + restore env
  on drop.

`serial_test` does **not** restore env values by itself; always pair it
with `EnvGuard` for panic/assertion-safe cleanup.

### Fail-under policy

**No coverage floor is enforced yet.** This run establishes the
baseline. A maintainer will set a `--fail-under` threshold in a later
dedicated PR once the baseline is known and the auth-broker / vault /
botwork-login code has landed from `botwork-extra`.
