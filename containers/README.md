# Containers

`botwork` builds one container image:

- `session-broker`: Rust session broker service image.

## Build locally

Build the image locally with:

```bash
make -C containers containers
```

This produces `botwork/session-broker:local`.

## Produce tarballs

Downstream consumers can export the locally built image as a tarball with:

```bash
make -C containers tarballs
```

That writes `containers/dist/session-broker.tar`, which consumers can load with `docker load`.

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
