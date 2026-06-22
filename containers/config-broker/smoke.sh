#!/usr/bin/env bash
#
# Per-service container smoke test for `botwork/config-broker:local`.
#
# Called from .github/workflows/_container.yml after `earthly
# +config-broker-image` has produced the local image. Lives next to
# the Dockerfile so the per-service test surface is discoverable
# alongside the per-service build surface; line-for-line identical
# to the body that used to live inline in .github/workflows/containers.yml.
#
# Local reproduction:
#
#   earthly +config-broker-image
#   bash containers/config-broker/smoke.sh

set -euo pipefail

# Post-RFE #101 PR2: config-broker reads from postgres at startup.
# The fail-fast on no DB URL is the operator-visible signal that
# the env-file plumbing didn't land — production CMD must surface
# that as a non-zero exit, not a hung connect.
output="$(docker run --rm botwork/config-broker:local 2>&1 || true)"
echo "${output}"
echo "${output}" | grep -q "BOTWORK_DATABASE_URL is not set"
