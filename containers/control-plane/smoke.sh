#!/usr/bin/env bash
#
# Per-service container smoke test for `botwork/control-plane:local`.
#
# Called from .github/workflows/_container.yml after `earthly
# +control-plane-image` has produced the local image. Lives next to
# the Dockerfile so the per-service test surface is discoverable
# alongside the per-service build surface; line-for-line identical
# to the body that used to live inline in .github/workflows/containers.yml.
#
# Local reproduction:
#
#   earthly +control-plane-image
#   bash containers/control-plane/smoke.sh

set -euo pipefail

# control-plane v0.1.8+ polls session-broker for a cold-start
# recovery sync before binding (botworkz/botwork#87 / #81). In
# the isolated container smoke there is no session-broker, so
# the supported break-glass flag BOTWORK_CONTROL_PLANE_DISABLE_
# RECOVERY=1 is set here. The recovery path itself is exercised
# end-to-end by the vm/ integration suite where session-broker
# actually exists; this step only proves the binary boots and
# binds both 0.0.0.0:9300 (HTTP) and 0.0.0.0:9301 (xDS gRPC,
# added in #91).
#
# Like session-broker, control-plane has no graceful-shutdown
# handler so we mirror the detach + read-logs + force-rm shape
# rather than using `timeout`. The trap fires regardless of how
# the step ends so no container leaks into the runner.
name="botwork-smoke-control-plane-$$"
trap "docker rm -f ${name} >/dev/null 2>&1 || true" EXIT
docker run -d --name "${name}" \
  -e BOTWORK_CONTROL_PLANE_DISABLE_RECOVERY=1 \
  botwork/control-plane:local >/dev/null
sleep 3
output="$(docker logs "${name}" 2>&1)"
echo "${output}"
echo "${output}" | grep -q "starting HTTP on 0.0.0.0:9300"
echo "${output}" | grep -q "starting xDS gRPC on 0.0.0.0:9301"
