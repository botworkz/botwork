#!/usr/bin/env bash
#
# Per-service container smoke test for `botwork/auth-broker:local`.
#
# Local reproduction:
#
#   docker buildx build --platform linux/amd64 --load \
#     -t botwork/auth-broker:local -f auth-broker/Dockerfile .
#   bash auth-broker/smoke.sh

set -euo pipefail

name="botwork-smoke-auth-broker-$$"
body_file="$(mktemp /tmp/botwork-auth-broker-smoke-body.XXXXXX)"
trap "docker rm -f ${name} >/dev/null 2>&1 || true; rm -f ${body_file} >/dev/null 2>&1 || true" EXIT

docker run -d --rm --name "${name}" \
  -p 9100:9100 \
  -p 9101:9101 \
  botwork/auth-broker:local >/dev/null

ready=0
for _ in $(seq 1 30); do
  status="$(curl --silent --show-error --output "${body_file}" --write-out '%{http_code}' \
    http://127.0.0.1:9100/ || true)"
  if [[ "${status}" != "000" ]]; then
    ready=1
    break
  fi
  sleep 1
done
if [[ "${ready}" -ne 1 ]]; then
  echo "auth-broker did not become reachable on 127.0.0.1:9100 in time" >&2
  docker logs "${name}" >&2 || true
  exit 1
fi

if [[ "${status}" != "401" ]]; then
  echo "expected 401 from http://127.0.0.1:9100/, got ${status}" >&2
  cat "${body_file}" >&2 || true
  exit 1
fi
cat "${body_file}"
jq -e '.error.code and .error.message and .error.remediation.command' < "${body_file}" >/dev/null

status_internal="$(curl --silent --show-error --output /dev/null --write-out '%{http_code}' \
  http://127.0.0.1:9101/ || true)"
if [[ "${status_internal}" != "404" ]]; then
  echo "expected 404 from internal listener http://127.0.0.1:9101/, got ${status_internal}" >&2
  exit 1
fi
