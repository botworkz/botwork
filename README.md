# botwork

## Versioning

`/VERSION` at the repository root is botwork's single source of truth for the
release version.

The release automation in `.github/workflows/bump.yml` updates that file after
each release. Every binary now reads `/VERSION` at compile time through the
`botwork-version` crate, so `--version` output, startup version logs, and
protocol surfaces like MCP `clientInfo.version` stay aligned.

A follow-up PR will additionally wire this value into OCI
`org.opencontainers.image.version` labels and start surfacing `GIT_SHA` in
compiled builds.