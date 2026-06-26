# botwork

## Versioning

`/VERSION` at the repository root is botwork's single source of truth for the
release version.

The release automation in `.github/workflows/bump.yml` updates that file after
each release. Every binary now reads `/VERSION` at compile time through the
`botwork-version` crate, so `--version` output, startup version logs, and
protocol surfaces like MCP `clientInfo.version` stay aligned.

* Every per-crate runtime image carries the OCI standard labels for
  introspection without booting the container:

  ````bash
  docker image inspect ghcr.io/botworkz/botwork/session-broker:0.3.16 \
    --format '{{ json .Config.Labels }}' | jq
  # {
  #   "org.opencontainers.image.revision": "<full git sha>",
  #   "org.opencontainers.image.source": "https://github.com/botworkz/botwork",
  #   "org.opencontainers.image.version": "0.3.16"
  # }
  ````

  The `version` label always matches the `/VERSION` file at build
  time, including the `-dev` suffix on pre-release builds.