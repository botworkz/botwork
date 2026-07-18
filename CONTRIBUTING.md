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

Both commands pick up `tarpaulin.toml` automatically and write Lcov and
Xml reports into a `coverage/` directory.

### What is measured

Coverage is **unit-test only** — the same tests that `cargo test
--workspace --all-targets` runs, excluding:

- `botwork-ui-wasm`: this crate only targets `wasm32-unknown-unknown`
  and cannot be built for the host.

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

### Fail-under policy

**No coverage floor is enforced yet.** This run establishes the
baseline. A maintainer will set a `--fail-under` threshold in a later
dedicated PR once the baseline is known and the auth-broker / vault /
botwork-login code has landed from `botwork-extra`.
