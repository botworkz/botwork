# botwork-tools (Rust)

Generic CLI for Botwork operational tools.

## Usage

```bash
botwork-tools <SUBCOMMAND>
```

Currently implemented subcommands:
- `version` - print the shared botwork release version baked into the
  binary.
- `ps` - list running `mcp_session_*` containers with bound agent,
  plugin, and age. Reads session metadata from session-broker's
  admin `GET /sessions` endpoint
  (`BOTWORK_TOOLS_SESSIONS_URL`, default
  `http://session_broker:9002/sessions`); intersects with `docker
  ps --filter name=^mcp_session_` for the runtime row set.
- `bootstrap` - apply a `bootstrap.yaml` through admin-api.
- `mcp-probe` - probe an MCP image, validate it against an
  `mcp-package.yaml`, and emit / verify / describe the
  `org.botwork.mcp.*` label set (RFE #147).

## `botwork-tools mcp-probe`

Producer-side tool for the
[image-labels-as-plugin-descriptor flow](https://github.com/botworkz/botwork/issues/147).

Given a containerised MCP server image and a sibling
`mcp-package.yaml`, the probe:

1. Starts the image as a throwaway container on an ephemeral host
   port.
2. Drives an MCP `initialize` → `tools/list` →
   conditional `resources/list` / `prompts/list` handshake.
3. Composes the full `org.botwork.mcp.*` label set
   (BTreeMap-sorted = deterministic).
4. Runs one of three modes:
   - `generate` — patches the image config with the labels (via
     `crane mutate` when present, falling back to
     `docker buildx build --label`).
   - `verify` — re-probes a labeled image, fails with exit 6 if
     anything captured drifted from the labels on disk.
   - `describe` — prints the would-be label set to stdout
     (`key=value\n`, alphabetical, no image write).

CLI shape:

```text
botwork-tools mcp-probe <generate|verify|describe> [OPTIONS]
```

Common options:

- `--in <ref>` — source image (tag or digest). Required.
- `--package <path>` — `mcp-package.yaml`; defaults to
  `./mcp-package.yaml`.
- `--port <port>` — bind a host port for the probe; default
  ephemeral.
- `--timeout <secs>` — overall handshake timeout (default 60).
- `--runtime <name>` — container runtime; default `docker`.

Generate-only:

- `--out <ref>` — destination image tag. Required for `generate`.

Exit codes (full table in `src/mcp_probe/mod.rs`):

| Code | Meaning                                                  |
|------|----------------------------------------------------------|
| 0    | success                                                  |
| 2    | invalid CLI usage                                        |
| 3    | mcp-package.yaml missing / unreadable / fails validation |
| 4    | container failed to start / port never accepted          |
| 5    | MCP handshake error                                      |
| 6    | label drift detected (verify mode only)                  |
| 7    | image-patching tool unavailable / failed                 |

### `mcp-package.yaml` shape

Closed schema (`#[serde(deny_unknown_fields)]` at every level). The
package-side fields mirror `bootstrap.yaml`'s plugin entry one-for-one
(`port`, `path`, `upstream_auth`, `egress`, `resources`, `env`) so
the producer-side validator and the consumer-side catalog upserter
share rules verbatim. The package-only fields are `isolation` and
`spill`.

```yaml
# echo/mcp-package.yaml
name: echo                           # required; [a-z][a-z0-9-]{0,30}
port: 8000                           # default
path: /mcp                           # default (NB: differs from bootstrap default of /)
upstream_auth: none                  # default; or bearer/<service>
isolation: shared                    # required; shared | per_agent_session | per_request
egress:
  mode: none                         # required; mode: all|none OR allow: [...]
resources: {}                        # optional; defaults
env: []                              # optional; defaults
spill:
  mode: never                        # required; never | always | size
  # threshold_bytes: 65536           # required iff mode=size
  # include_methods: [tools/call]    # optional allowlist
  # include_tools: [fetch]           # optional allowlist
```

Validation is shared with `bootstrap.yaml` plugin entries via
`botwork-admin-core::package::validate_package` — same regexes,
same env-name reservations (`BOTWORK_SECRET_*`, `BOTWORK_MCP_CONFIG`,
`DOCKER_*`), same egress / resources / static-env caps. A package
file that passes the probe will pass the catalog upserter.

### GitHub Action

A composite action lives at `actions/mcp-probe/action.yml` (top-level
`actions/`, NOT `.github/actions/` — the latter is reserved for
internal CI plumbing this repo's own workflows consume; the former
is the convention for shipped composite actions external repos pin).
Pin to a tagged release of this repo:

```yaml
- uses: botworkz/botwork/actions/mcp-probe@vX.Y.Z
  with:
    mode: generate                # or "verify" or "describe"
    image: mcp-foo:unlabeled
    package: ./mcp-package.yaml
    out: botwork/mcp-foo:local    # generate-only
```

The action installs `botwork-tools` (released alongside this repo)
and `crane`, then invokes the probe. The action's exit status is
the probe's exit code so CI gating maps directly to the table
above.

## Build

```bash
cargo build --release
```

Binary output:
- `target/release/botwork-tools`
