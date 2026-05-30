# botwork-tools (Rust)

Generic CLI for Botwork operational tools.

## Usage

```bash
botwork-tools <SUBCOMMAND>
```

Currently implemented subcommands:
- `ps` - list running `mcp_session_*` containers, including short Docker container ID.

## Build

```bash
cargo build --release
```

Binary output:
- `target/release/botwork-tools`
