# botwork-tools (Rust)

Generic CLI for Botwork operational tools.

## Usage

```bash
botwork-tools <SUBCOMMAND>
```

Currently implemented subcommands:
- `ps` - list running `mcp_session_*` containers with bound agent,
  plugin, and age. Reads session metadata from session-broker's
  admin `GET /sessions` endpoint
  (`BOTWORK_TOOLS_SESSIONS_URL`, default
  `http://session_broker:9002/sessions`); intersects with `docker
  ps --filter name=^mcp_session_` for the runtime row set.
- `bootstrap` - apply a `bootstrap.yaml` through admin-api.

## Build

```bash
cargo build --release
```

Binary output:
- `target/release/botwork-tools`
