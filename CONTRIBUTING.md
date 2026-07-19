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

### What is excluded from the coverage denominator

The following paths are listed in `tarpaulin.toml` under `exclude-files`
so they do not inflate (or deflate) the reported percentage:

| Pattern | Reason |
|---|---|
| `db/entity/src/*` | SeaORM `#[derive(DeriveEntityModel)]` generated entity models — macro-expanded glue with no meaningful branch logic. |
| `db/migration/src/*` | Migration definitions (`lib.rs`, `main.rs`, every `m2026*.rs`) — these run only against a live database via the migration runner and are exercised by integration/CI steps, not unit tests. |
| `*/main.rs` | Process-bootstrap entry points across every workspace binary (api, session-broker, control-plane, config-broker, botwork-cli, launcher, tools, ui/server) — arg-parsing shell, tokio-runtime setup, and server bind/serve wiring that is not meaningfully unit-testable. |

To update this list, edit `tarpaulin.toml` (`exclude-files`) and update
this table.  Do **not** add exclusions for files that contain real branch
logic — only generated code, database-only migration runners, and
process-bootstrap entry points qualify.

### Item-level coverage exclusions

For **specific functions or `impl` blocks** within otherwise-covered files,
annotate with `#[cfg(not(tarpaulin_include))]`.  This is the correct
mechanism for:

- `impl DockerApi for Docker` production wrappers and `connect_docker()`
  in each crate's `docker.rs` — thin bollard passthroughs that require a
  live docker socket; the *logic* is tested via `FakeDocker`/`SpyDocker`.
- Production socket-wrapper functions in `recovery.rs`
  (`force_remove_container`, `recover_live_workers`) that call
  `connect_docker()` directly.
- Trivial wiring or passthrough functions in crate `lib.rs` files
  (`build_app_state`, `run`, `version_string`) where there is no branch
  logic to test.

Rules for item-level exclusions — stricter than file-level:
- The item must be **either** (a) a documented socket-only production
  wrapper **or** (b) a genuinely logic-free re-export / passthrough.
- Do **not** exclude the `*_impl` seam functions, trait definitions, test
  doubles (`FakeDocker`/`SpyDocker`), or any function with real branches.
- Because `tarpaulin_include` is an undeclared cfg key, the workspace
  `Cargo.toml` declares it via
  `unexpected_cfgs = { level = "warn", check-cfg = ['cfg(tarpaulin_include)'] }`
  so `cargo clippy -D warnings` stays green.

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
