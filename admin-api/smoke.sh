#!/usr/bin/env bash
#
# Per-service container smoke test for `botwork/admin-api:local`.
#
# Local reproduction (db-migrate image must also be present locally):
#
#   earthly +db-migrate-image
#   earthly +admin-api-image
#   bash admin-api/smoke.sh

set -euo pipefail

# End-to-end production-path proof for admin-api (RFE #106 PR1):
#
#   1. boot a fresh postgres on a throwaway docker network,
#   2. land the schema via db-migrate (admin-api's health probe
#      runs SELECT 1; it doesn't strictly need migrations, but
#      production always has them — keep the test path honest),
#   3. run admin-api on the same network with the DB URL set,
#   4. curl /admin/api/v1/health from a sibling client container
#      on the same network (the host has no published port —
#      same trust-boundary posture as config-broker / control-
#      plane), assert the response shape,
#   5. confirm BOTWORK_DATABASE_URL unset surfaces a structured
#      error and a non-zero exit (operator misconfig is fail-loud).
#
# postgres pin must match db/migration/tests/migrate_smoke.rs and
# the vm-baked shasset; drift between the three is exactly what
# this smoke is here to catch.
net="botwork-smoke-admin-api-$$"
pg="botwork-smoke-postgres-$$"
api="botwork-smoke-admin-api-svc-$$"
trap "docker rm -f ${api} >/dev/null 2>&1 || true; \
      docker rm -f ${pg} >/dev/null 2>&1 || true; \
      docker network rm ${net} >/dev/null 2>&1 || true" EXIT
docker network create "${net}" >/dev/null

docker run -d --name "${pg}" \
  --network "${net}" --network-alias postgres \
  -e POSTGRES_USER=botwork \
  -e POSTGRES_PASSWORD=smoke \
  -e POSTGRES_DB=botwork \
  postgres:16-alpine >/dev/null

ready=0
for _ in $(seq 1 30); do
  if docker exec "${pg}" pg_isready -U botwork -d botwork >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 1
done
if [[ "${ready}" -ne 1 ]]; then
  echo "postgres did not become ready in time" >&2
  docker logs "${pg}" >&2 || true
  exit 1
fi

docker run --rm --network "${net}" \
  -e BOTWORK_DATABASE_URL="postgres://botwork:smoke@postgres/botwork" \
  botwork/db-migrate:local >/dev/null

# 3. — start admin-api detached on the network with the alias
# production uses. No --publish: the trust boundary is the
# docker network, not a host port.
docker run -d --name "${api}" \
  --network "${net}" --network-alias admin_api \
  -e BOTWORK_DATABASE_URL="postgres://botwork:smoke@postgres/botwork" \
  botwork/admin-api:local >/dev/null

# Wait for the listener to bind. The startup log line is
# `[admin-api] starting on 0.0.0.0:9400`; grep for it rather
# than sleeping a fixed amount so a slow scheduler doesn't
# flake the test.
ready=0
for _ in $(seq 1 30); do
  if docker logs "${api}" 2>&1 | grep -q "starting on 0.0.0.0:9400"; then
    ready=1
    break
  fi
  sleep 1
done
if [[ "${ready}" -ne 1 ]]; then
  echo "admin-api did not bind in time" >&2
  docker logs "${api}" >&2 || true
  exit 1
fi

# 4. — hit /admin/api/v1/health from a sibling curl container.
# We pin curl by digest-less tag (CI-only, throwaway) and use
# --fail so a non-2xx surfaces as a non-zero exit. The body is
# tiny JSON; check the two contract fields.
body="$(docker run --rm --network "${net}" curlimages/curl:8.10.1 \
  --fail --silent --show-error \
  http://admin_api:9400/admin/api/v1/health)"
echo "health body: ${body}"
echo "${body}" | grep -q '"status":"ok"'
echo "${body}" | grep -q '"db":"reachable"'

# 5. — fail-loud on missing URL. Same shape as the other broker
# smokes: capture exit, expect non-zero, expect the structured
# error string in stderr.
set +e
docker run --rm botwork/admin-api:local >misslog 2>&1
miss_rc=$?
set -e
cat misslog
if [[ "${miss_rc}" -eq 0 ]]; then
  echo "expected non-zero exit when BOTWORK_DATABASE_URL unset" >&2
  exit 1
fi
grep -q "BOTWORK_DATABASE_URL is not set" misslog
