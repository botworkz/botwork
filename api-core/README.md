# botwork-api-core

Per-entry validators for the persistence layer.

Shared between `botwork-bootstrap` (today's boot-time writer of
`bootstrap.yaml`) and `botwork-api` (the HTTP+JSON writer that
replaces it under [RFE #106](https://github.com/botworkz/botwork/issues/106)).
The two writers are structurally different but the "what makes a
plugin / binding spec valid" question has exactly one answer; this
crate holds it.

## What lives here

* `error::ValidationError` — structured errors for every rule the
  validators enforce. Carries the offending field path and a
  human-readable detail; the bootstrap binary lifts these into
  `BootstrapError::PluginInvalid` / `BindingInvalid`, and api
  maps them into HTTP 400/409 response bodies.
* `plugin_spec::validate_one` — validates a single `RawPluginEntry`
  into a `ValidatedPlugin` (image / port / path / upstream_auth / env
  / resources / egress, with the same defaults and rules the
  pre-cutover `config-broker/src/registry.rs` shipped).
* `plugin_spec::validate_workspace_plugin_config` — validates a
  per-binding `config:` blob (non-mapping rejected, oversized
  rejected, empty mapping → `None`).
* Constants (`RESERVED_ENV_NAMES`, `SECRET_ENV_PREFIX`,
  `CONFIG_ENV_NAME`, `MAX_ENV_VALUE_BYTES`, `MAX_STATIC_ENV_ENTRIES`,
  `PLUGIN_NAME_RE`) — contract values with `launcher/src/validate.rs`.

## What does NOT live here

* **SeaORM entity types.** The crate is DB-agnostic so it can be
  consumed by tests / future tooling that don't link sea-orm.
  Conversions live in the consumer crates.
* **Apply / upsert logic.** That's `botwork-bootstrap::runner` today
  and `botwork-api::write` (PR3) tomorrow.
* **The yaml-shape `BootstrapConfig` struct.** That's bootstrap-only
  (it models the on-disk file shape, not the validation rules).
* **List-level rules** — duplicate-name detection, unknown-plugin
  references in bindings. Those are caller-driven: bootstrap enforces
  them while traversing its yaml tree, api enforces them
  per-request against the live DB.

## History

Originally lifted from the pre-cutover `config-broker/src/registry.rs`
into `bootstrap/src/plugin_spec.rs` for [RFE #101 PR2]. Pulled out of
bootstrap into this crate for [RFE #106 PR2] so api can consume
the same rules without depending on bootstrap (which is a soon-to-be-
retired writer with its own runtime concerns).

[RFE #101 PR2]: https://github.com/botworkz/botwork/issues/101
[RFE #106 PR2]: https://github.com/botworkz/botwork/issues/106
