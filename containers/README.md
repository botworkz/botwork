# Containers

`botwork` builds one container image:

- `session-broker`: Rust session broker service image.

## Build locally

Build the image locally with EarthBuild (the maintained Earthly fork):

```bash
tmp="$(mktemp -d)"
base="https://github.com/EarthBuild/earthbuild/releases/download/v0.8.17"
curl -fsSL -o "${tmp}/earth-linux-amd64" "${base}/earth-linux-amd64"
curl -fsSL -o "${tmp}/checksum.asc" "${base}/checksum.asc"
( cd "${tmp}" && grep ' earth-linux-amd64$' checksum.asc | sha256sum -c - )
sudo install -m 0755 "${tmp}/earth-linux-amd64" /usr/local/bin/earthly
earthly bootstrap
earthly +session-broker-image
```

This produces `botwork/session-broker:local`.

`botworkz/space` and `botworkz/vm` consume this cross-repo in sibling/local mode
via `FROM ../botwork+session-broker-image`, so the `+session-broker-image` target
name and `botwork/session-broker:local` tag are a stable contract.

## Produce tarballs

Downstream consumers can export the locally built image as a tarball with:

```bash
make -C containers tarballs
```

`make -C containers` now routes image builds through `earthly +session-broker-image`
so the Earthfile is the single source of truth. `tarballs` remains as a thin
convenience wrapper and writes `containers/dist/session-broker.tar`, which
consumers can load with `docker load`.

## Release process

Publishing is fully automated via `.github/workflows/release.yml` and is driven
by the root `VERSION` file (repo root, not this directory).

**Version-driven cycle:**

1. Set `VERSION` to a clean semver (no suffix), e.g. `1.2.0`, and merge to `main`.
2. The release workflow detects the clean version and automatically:
   - Builds and pushes `ghcr.io/botworkz/botwork/session-broker:<VERSION>` and
     `ghcr.io/botworkz/botwork/session-broker:latest` to GHCR.
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
