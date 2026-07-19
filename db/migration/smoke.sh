#!/usr/bin/env bash
#
# Per-service container smoke test for `botwork/db-migrate:local`.
#
# Local reproduction:
#
#   docker buildx build --platform linux/amd64 --load \
#     -t botwork/db-migrate:local -f db/migration/Dockerfile .
#   bash db/migration/smoke.sh

set -euo pipefail

# End-to-end production-path proof for db-migrate (RFE 97):
#   1. boot a fresh postgres on a throwaway docker network,
#   2. run db-migrate against it via BOTWORK_DATABASE_URL,
#   3. assert exit 0 (Migrator::up succeeds),
#   4. assert the seaql_migrations tracking table exists,
#   5. run db-migrate AGAIN and assert it still exits 0
#      (idempotency — restart of the systemd oneshot is safe),
#   6. confirm BOTWORK_DATABASE_URL unset surfaces a structured
#      "MissingUrl" error and a non-zero exit (operator
#      misconfig is fail-loud, not silent-success).
#
# We pin postgres:16-alpine here. It MUST match the tag used
# by `db/migration/tests/migrate_smoke.rs` (POSTGRES_TAG) and
# by the vm-baked shasset entry once the companion PR lands.
# Drift between those three is exactly the class of bug this
# smoke is here to catch.
net="botwork-smoke-db-$$"
pg="botwork-smoke-postgres-$$"
trap "docker rm -f ${pg} >/dev/null 2>&1 || true; \
      docker network rm ${net} >/dev/null 2>&1 || true" EXIT
docker network create "${net}" >/dev/null

docker run -d --name "${pg}" \
  --network "${net}" --network-alias postgres \
  -e POSTGRES_USER=botwork \
  -e POSTGRES_PASSWORD=smoke \
  -e POSTGRES_DB=botwork \
  postgres:16-alpine >/dev/null

# Wait for postgres to actually accept connections, not just for
# the container to be up. `pg_isready` is shipped in the image.
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

# 1. + 2. + 3. — first up. Production CMD takes no args.
out1="$(docker run --rm --network "${net}" \
  -e BOTWORK_DATABASE_URL="postgres://botwork:smoke@postgres/botwork" \
  botwork/db-migrate:local 2>&1)"
echo "${out1}"
echo "${out1}" | grep -q "migrations applied"

# 4. — tracking table exists.
table="$(docker exec "${pg}" psql -U botwork -d botwork -tAc \
  "SELECT to_regclass('public.seaql_migrations')")"
if [[ "${table}" != "seaql_migrations" ]]; then
  echo "expected public.seaql_migrations after first up, got: '${table}'" >&2
  exit 1
fi

# 4b. — the schema-landing migrations actually ran. Smoke-check the
# most recently added table from RFE #146 alongside the others so a
# silently-skipped migration trips immediately rather than at the
# next consumer cutover.
for tbl in tenant workspace plugin workspace_plugin agent_session \
           session_worker opaque_password_file lease plugin_image_facet; do
  got="$(docker exec "${pg}" psql -U botwork -d botwork -tAc \
    "SELECT to_regclass('public.${tbl}')")"
  if [[ "${got}" != "${tbl}" ]]; then
    echo "expected public.${tbl} after first up, got: '${got}'" >&2
    exit 1
  fi
done

# 5. — idempotent re-run.
out2="$(docker run --rm --network "${net}" \
  -e BOTWORK_DATABASE_URL="postgres://botwork:smoke@postgres/botwork" \
  botwork/db-migrate:local 2>&1)"
echo "${out2}"
echo "${out2}" | grep -q "migrations applied"

# 6. — fail-loud on missing URL. We do NOT use `set -e` here; we
# capture the exit code and check it explicitly so the error
# message in the log is visible regardless.
set +e
docker run --rm botwork/db-migrate:local >misslog 2>&1
miss_rc=$?
set -e
cat misslog
if [[ "${miss_rc}" -eq 0 ]]; then
  echo "expected non-zero exit when BOTWORK_DATABASE_URL unset" >&2
  exit 1
fi
grep -q "BOTWORK_DATABASE_URL is not set" misslog
