#!/usr/bin/env bash
#
# Per-service container smoke test for `botwork/session-broker:local`.
#
# Called from .github/workflows/_container.yml after `earthly
# +session-broker-image` has produced the local image. Lives next to
# the Dockerfile so the per-service test surface is discoverable
# alongside the per-service build surface; line-for-line identical
# to the body that used to live inline in .github/workflows/containers.yml.
#
# Local reproduction:
#
#   earthly +session-broker-image
#   bash containers/session-broker/smoke.sh

set -euo pipefail

# RFE #105 PR2: session-broker became a DB consumer. It now
# fail-fasts on a missing BOTWORK_DATABASE_URL the same way
# config-broker / bootstrap / admin-api do, so the container
# smoke flips from "boot it and grep the bind log" to "boot
# it without env and grep the fail-fast error" — the
# symmetric shape config-broker's smoke uses.
#
# The "broker actually boots cleanly with a live DB" property
# is covered by:
#   * the cargo testcontainers suite under
#     `session-broker/tests/agent_session_writethrough_test.rs`
#     (postgres in a sidecar, full lifecycle assertions);
#   * the vm/ integration suite where session-broker runs
#     alongside a real postgres + launcher, and the goss
#     check on `botwork-session-broker.service` proves the
#     listener bind.
#
# We do NOT spin a postgres sidecar here because the broker
# also needs a unix-socket launcher to be properly exercised,
# and the launcher isn't shipped as a container image — it's
# a host-side binary baked into vm/. Mocking it just to check
# one log line would be a higher-flake surface than the
# explicit-fail-fast check.
output="$(docker run --rm botwork/session-broker:local 2>&1 || true)"
echo "${output}"
echo "${output}" | grep -q "BOTWORK_DATABASE_URL is not set"
