# Containers

`botwork` builds four container images:

- `session-broker`: Rust session broker service image.
- `config-broker`: Rust config broker service image (resolves plugin
  descriptors for session-broker; owns `plugins.yaml`).
- `control-plane`: Rust control-plane service image (xDS for envoy + per-
  session policy fan-out).
- `db-migrate`: Rust **oneshot** (not a server) that runs SeaORM migrations
  against postgres at boot and exits. See `db/migration/` for the binary
  and RFE 97 for the design.

## Build locally

Build the images locally with EarthBuild (the maintained Earthly fork):

```bash
tmp="$(mktemp -d)"
base="https://github.com/EarthBuild/earthbuild/releases/download/v0.8.17"
curl -fsSL -o "${tmp}/earth-linux-amd64" "${base}/earth-linux-amd64"
curl -fsSL -o "${tmp}/checksum.asc" "${base}/checksum.asc"
( cd "${tmp}" && grep ' earth-linux-amd64$' checksum.asc | sha256sum -c - )
sudo install -m 0755 "${tmp}/earth-linux-amd64" /usr/local/bin/earthly
# Initializes EarthBuild's local buildkit daemon on first use.
earthly bootstrap
rm -rf "${tmp}"
earthly +session-broker-image
earthly +config-broker-image
earthly +control-plane-image
earthly +db-migrate-image
# Or build everything:
earthly +images
```

This produces `botwork/session-broker:local`, `botwork/config-broker:local`,
`botwork/control-plane:local`, and `botwork/db-migrate:local`.

> **Release builds** stamp each image with `org.opencontainers.image.revision`
> set to `$GITHUB_SHA` and verify the label matches before pushing to GHCR —
> if there is ever a mismatch the workflow fails rather than silently shipping
> the wrong image. Local builds (and PR builds) do not pass `GIT_SHA`, so the
> label will be empty, which is fine — the check only runs in the release path.

`botworkz/vm` consumes these cross-repo in sibling/local mode via
`FROM ../botwork+<svc>-image`, so the `+<svc>-image` target names and
`botwork/<svc>:local` tags are a stable contract.

## Produce tarballs

Downstream consumers can export the locally built images as tarballs with:

```bash
make -C containers tarballs
```

`make -C containers` routes image builds through `earthly +<svc>-image` so the
Earthfile is the single source of truth. `tarballs` remains as a thin
convenience wrapper and writes `containers/dist/<svc>.tar` for each service,
which consumers can load with `docker load`.

## Release process

Publishing is fully automated via `.github/workflows/release.yml` and is driven
by the root `VERSION` file (repo root, not this directory).

**Version-driven cycle:**

1. Set `VERSION` to a clean semver (no suffix), e.g. `1.2.0`, and merge to `main`.
2. The release workflow detects the clean version and automatically:
   - Builds and pushes `ghcr.io/botworkz/botwork/session-broker:<VERSION>`,
     `ghcr.io/botworkz/botwork/config-broker:<VERSION>`,
     `ghcr.io/botworkz/botwork/control-plane:<VERSION>`,
     `ghcr.io/botworkz/botwork/db-migrate:<VERSION>`, and the corresponding
     `:latest` tags to GHCR.
   - Builds release binaries for `botwork-launcher` and `botwork-tools`.
   - Creates a GitHub Release `v<VERSION>` with those binaries as assets.
3. The published Release event triggers a second job that bumps `VERSION` to the
   next minor dev version (e.g. `1.2.0` → `1.3.0-dev`) and commits it back to
   `main`. That `-dev` push is a no-op for publishing, completing the loop.

**Push to `main` with a `-dev` VERSION (e.g. day-to-day development) → no publish.**

**How to cut a release:**
```bash
# 1. Edit VERSION in the repo root (remove the -dev suffix)
echo "1.2.0" > VERSION
git add VERSION
git commit -m "chore: release 1.2.0"
# 2. Open a PR and merge to main — automation does the rest
```
