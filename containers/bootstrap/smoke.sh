#!/usr/bin/env bash
#
# Per-service container smoke test for `botwork/bootstrap:local`.
#
# Called from .github/workflows/_container.yml after `earthly
# +bootstrap-image` has produced the local image. Lives next to
# the Dockerfile so the per-service test surface is discoverable
# alongside the per-service build surface; line-for-line identical
# to the body that used to live inline in .github/workflows/containers.yml.
#
# Local reproduction (db-migrate image must also be present locally):
#
#   earthly +db-migrate-image
#   earthly +bootstrap-image
#   bash containers/bootstrap/smoke.sh

set -euo pipefail

# End-to-end production-path proof for bootstrap (RFE #101):
#
#   1. boot a fresh postgres on a throwaway docker network,
#   2. land the schema via db-migrate (bootstrap depends on it),
#   3. mount a minimal bootstrap.yaml into the bootstrap container
#      and run it; assert exit 0,
#   4. assert tenant/workspace/plugin/workspace_plugin rows are
#      what the yaml says they should be,
#   5. re-run bootstrap against the same DB and assert exit 0
#      (idempotency — boot can restart safely),
#   6. confirm BOTWORK_DATABASE_URL unset surfaces a structured
#      error and a non-zero exit (operator misconfig is fail-loud).
#
# postgres pin must match db/migration/tests/migrate_smoke.rs and
# the vm-baked shasset; drift between the three is exactly what
# this smoke is here to catch.
net="botwork-smoke-bootstrap-$$"
pg="botwork-smoke-postgres-$$"
cfg="${RUNNER_TEMP:-/tmp}/bootstrap.yaml"
trap "docker rm -f ${pg} >/dev/null 2>&1 || true; \
      docker network rm ${net} >/dev/null 2>&1 || true; \
      rm -f ${cfg}" EXIT
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

cat > "${cfg}" <<'YAML'
tenants:
- name: phlax
  workspaces:
  - name: mcp
    plugins:
    - name: mcp-bash
      config:
        foo: bar
    - name: mcp-fetch

plugins:
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:1.0
  egress: none
- name: mcp-fetch
  image: ghcr.io/example/mcp-fetch:1.0
  egress:
    allow:
    - host: example.com
      ports: [443]
YAML

# 1. + 2. + 3. — first apply.
out1="$(docker run --rm --network "${net}" \
  -e BOTWORK_DATABASE_URL="postgres://botwork:smoke@postgres/botwork" \
  -v "${cfg}:/etc/botwork/bootstrap.yaml:ro" \
  botwork/bootstrap:local 2>&1)"
echo "${out1}"
echo "${out1}" | grep -q "applied bootstrap from /etc/botwork/bootstrap.yaml"

# 4. — sanity-check the row counts and the resolve-shape join.
counts="$(docker exec "${pg}" psql -U botwork -d botwork -tA -c \
  "SELECT (SELECT count(*) FROM tenant) || ':' \
          || (SELECT count(*) FROM workspace) || ':' \
          || (SELECT count(*) FROM plugin) || ':' \
          || (SELECT count(*) FROM workspace_plugin)")"
if [[ "${counts}" != "1:1:2:2" ]]; then
  echo "expected 1:1:2:2 (tenant:workspace:plugin:wp), got: '${counts}'" >&2
  exit 1
fi

resolved="$(docker exec "${pg}" psql -U botwork -d botwork -tA -c \
  "SELECT p.image \
     FROM plugin p \
     JOIN workspace_plugin wp ON wp.plugin_id = p.id \
     JOIN workspace w ON w.id = wp.workspace_id \
     JOIN tenant t ON t.id = w.tenant_id \
     WHERE t.name='phlax' AND w.name='mcp' AND p.name='mcp-bash'")"
if [[ "${resolved}" != "ghcr.io/example/mcp-bash:1.0" ]]; then
  echo "resolve query returned: '${resolved}'" >&2
  exit 1
fi

# 5. — idempotent re-run.
out2="$(docker run --rm --network "${net}" \
  -e BOTWORK_DATABASE_URL="postgres://botwork:smoke@postgres/botwork" \
  -v "${cfg}:/etc/botwork/bootstrap.yaml:ro" \
  botwork/bootstrap:local 2>&1)"
echo "${out2}"
echo "${out2}" | grep -q "applied bootstrap from /etc/botwork/bootstrap.yaml"

# 6. — fail-loud on missing URL.
set +e
docker run --rm \
  -v "${cfg}:/etc/botwork/bootstrap.yaml:ro" \
  botwork/bootstrap:local >misslog 2>&1
miss_rc=$?
set -e
cat misslog
if [[ "${miss_rc}" -eq 0 ]]; then
  echo "expected non-zero exit when BOTWORK_DATABASE_URL unset" >&2
  exit 1
fi
grep -q "BOTWORK_DATABASE_URL is not set" misslog
