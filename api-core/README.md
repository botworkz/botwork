# botwork-api-core

Per-entry validators for the persistence layer, plus name validation for the
Phase 2 URL grammar reshape ([botworkz/space#311](https://github.com/botworkz/space/issues/311)).

## What lives here

* `error::ValidationError` — structured errors for plugin/binding validation.
* `plugin_spec::validate_one` — validates a single `RawPluginEntry`.
* `plugin_spec::validate_workspace_plugin_config` — validates per-binding `config:` blobs.
* `names` — name validation for tenants, workspaces, and plugins (see below).

## Name validation (`names` module)

Canonical source: **`botwork-extra/auth-broker/src/grammar.rs`**.
This file (`api-core/src/names.rs`) is a vendor copy — **DO NOT EDIT directly**.
Sync from auth-broker when the upstream changes.

### Grammar

- **Regex:** `^[A-Za-z0-9_-]{1,63}$`
- **Case-sensitive storage**, **normalised-unique** (`Phlax` blocks creating `phlax`)
- Same regex for tenants, workspaces, and plugins; reserved lists may diverge per scope in future.

### Reserved names (tenant-scope v1)

`["admin", "api", "auth", "static", "stats", "logs"]`

Anything that fails the regex is implicitly in system-space (e.g. `.well-known/*`,
paths with `.` or `@`). The explicit reserved list only covers names that *match*
the regex but must not be used as tenant names because they collide with top-level
listener routes.

### Public API

```rust
use botwork_api_core::names::{
    validate_tenant_name,    // → Ok(()) or Err(NameError::Invalid | NameError::Reserved)
    validate_workspace_name, // → Ok(()) or Err(NameError::Invalid)
    validate_plugin_name,    // → Ok(()) or Err(NameError::Invalid)
    normalise_name,          // → lowercase; used for uniqueness checks
    NameError,
    NAME_REGEX,
    RESERVED_TENANT_NAMES,
};
```

`NameError` maps to HTTP 400 with `error.code = "invalid_name"` or `"reserved_name"`.

## References

- [botworkz/space#311](https://github.com/botworkz/space/issues/311) — Phase 2 design
- [RFE #106](https://github.com/botworkz/botwork/issues/106) — original admin-api RFE
