#!/usr/bin/env bash
#
# Per-service container smoke test for `botwork/config-broker:local`.
#
# Local reproduction:
#
#   earthly +config-broker-image
#   bash config-broker/smoke.sh

set -euo pipefail

# Post-RFE #101 PR2: config-broker reads from postgres at startup.
# The fail-fast on no DB URL is the operator-visible signal that
# the env-file plumbing didn't land — production CMD must surface
# that as a non-zero exit, not a hung connect.
output="$(docker run --rm botwork/config-broker:local 2>&1 || true)"
echo "${output}"
echo "${output}" | grep -q "BOTWORK_DATABASE_URL is not set"
