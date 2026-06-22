#!/usr/bin/env bash
#
# Per-service container smoke test for `botwork/admin-ui:local`.
#
# Called from .github/workflows/_container.yml after `earthly
# +admin-ui-image` has produced the local image. Lives next to
# the Dockerfile so the per-service test surface is discoverable
# alongside the per-service build surface; line-for-line identical
# to the body that used to live inline in .github/workflows/containers.yml.
#
# Local reproduction:
#
#   earthly +admin-ui-image
#   bash containers/admin-ui/smoke.sh

set -euo pipefail

# End-to-end production-path proof for admin-ui (RFE #106
# follow-up — Leptos control panel skeleton):
#
#   1. boot admin-ui on a throwaway docker network with the
#      `admin_ui` alias production uses (no postgres, no
#      db-migrate — admin-ui is a pure static-bundle server,
#      it has no DB connection);
#   2. curl /healthz from a sibling client container, assert
#      the JSON contract field;
#   3. curl /admin/ from a sibling client container, assert
#      we get an HTML body containing the trunk-stamped
#      marker string — this is the proof that the multi-
#      stage Dockerfile actually ran `trunk build` and the
#      include_dir! macro saw a populated dist/.
#
# No "fail-loud on missing URL" arm here because admin-ui
# has no required env. The bind address has a default and
# the bundle is baked into the binary.
net="botwork-smoke-admin-ui-$$"
ui="botwork-smoke-admin-ui-svc-$$"
trap "docker rm -f ${ui} >/dev/null 2>&1 || true; \
      docker network rm ${net} >/dev/null 2>&1 || true" EXIT
docker network create "${net}" >/dev/null

docker run -d --name "${ui}" \
  --network "${net}" --network-alias admin_ui \
  botwork/admin-ui:local >/dev/null

# Wait for the listener to bind. The startup log line is
# `[admin-ui] starting on 0.0.0.0:9500`; grep for it rather
# than sleeping a fixed amount so a slow scheduler doesn't
# flake the test.
ready=0
for _ in $(seq 1 30); do
  if docker logs "${ui}" 2>&1 | grep -q "starting on 0.0.0.0:9500"; then
    ready=1
    break
  fi
  sleep 1
done
if [[ "${ready}" -ne 1 ]]; then
  echo "admin-ui did not bind in time" >&2
  docker logs "${ui}" >&2 || true
  exit 1
fi

# 2. — /healthz.
body="$(docker run --rm --network "${net}" curlimages/curl:8.10.1 \
  --fail --silent --show-error \
  http://admin_ui:9500/healthz)"
echo "healthz body: ${body}"
echo "${body}" | grep -q '"status":"ok"'

# 3. — /admin/ should return the trunk-built shell, not 404.
# We grep for the page <title> we set in admin-ui/wasm/index.html;
# if the include_dir! bundle is empty this assertion fails
# loudly rather than the more confusing "404 from the server".
body="$(docker run --rm --network "${net}" curlimages/curl:8.10.1 \
  --fail --silent --show-error \
  http://admin_ui:9500/admin/)"
echo "admin body bytes: $(printf '%s' "${body}" | wc -c)"
echo "${body}" | grep -q "botwork admin"
echo "${body}" | grep -q "data-trunk"

# 4. — sub-resource path check. The bug this catches:
# if Trunk.toml drops `public_url = "/admin/"`, trunk emits
# `<link rel="modulepreload" href="/<hash>.js">` instead of
# `<link rel="modulepreload" href="/admin/<hash>.js">`. In
# the bare container the admin-ui server then 404s those
# root-relative paths, and the browser surfaces it as
# `NS_ERROR_CORRUPTED_CONTENT`. Through envoy (vm/space) the
# same path 401s on ext_authz. Either way the panel is
# broken in a way the existing "/admin/ returns HTML" check
# doesn't see.
#
# We:
#   a. parse the HTML for the wasm-bindgen JS loader URL
#      (trunk emits it as `<link rel="modulepreload"
#      href="...">`),
#   b. assert the URL starts with `/admin/` — root-relative
#      `/foo.js` is the specific failure mode this probe
#      catches,
#   c. curl it and assert 200 + a JS-ish content-type,
#   d. do the same for the `.wasm` blob (preload href).
js_url="$(printf '%s' "${body}" \
  | grep -oE 'href="[^"]+\.js"' \
  | head -1 \
  | sed -E 's/^href="([^"]+)"$/\1/')"
if [[ -z "${js_url}" ]]; then
  echo "could not find a JS bundle href in /admin/ body" >&2
  printf '%s\n' "${body}" >&2
  exit 1
fi
echo "js_url: ${js_url}"
case "${js_url}" in
  /admin/*) ;;
  *)
    echo "JS bundle href ${js_url} is not /admin/-prefixed" >&2
    echo "this means Trunk.toml lost public_url = \"/admin/\"" >&2
    exit 1
    ;;
esac

wasm_url="$(printf '%s' "${body}" \
  | grep -oE 'href="[^"]+\.wasm"' \
  | head -1 \
  | sed -E 's/^href="([^"]+)"$/\1/')"
if [[ -z "${wasm_url}" ]]; then
  echo "could not find a wasm preload href in /admin/ body" >&2
  printf '%s\n' "${body}" >&2
  exit 1
fi
echo "wasm_url: ${wasm_url}"
case "${wasm_url}" in
  /admin/*) ;;
  *)
    echo "wasm preload href ${wasm_url} is not /admin/-prefixed" >&2
    echo "this means Trunk.toml lost public_url = \"/admin/\"" >&2
    exit 1
    ;;
esac

# c. — JS loader must serve 200 + JS-ish content-type. We
# use `curl -D` to capture headers separately from the body
# because the JS payload is noisy in logs and we only care
# about the contract here.
hdrs="$(docker run --rm --network "${net}" curlimages/curl:8.10.1 \
  --fail --silent --show-error --dump-header - --output /dev/null \
  "http://admin_ui:9500${js_url}")"
echo "JS headers:"
printf '%s\n' "${hdrs}"
printf '%s\n' "${hdrs}" \
  | tr -d '\r' \
  | awk -F': ' 'tolower($1)=="content-type"{print tolower($2)}' \
  | grep -Eq '^(application/javascript|text/javascript)'

# d. — wasm blob must serve 200 + application/wasm.
hdrs="$(docker run --rm --network "${net}" curlimages/curl:8.10.1 \
  --fail --silent --show-error --dump-header - --output /dev/null \
  "http://admin_ui:9500${wasm_url}")"
echo "wasm headers:"
printf '%s\n' "${hdrs}"
printf '%s\n' "${hdrs}" \
  | tr -d '\r' \
  | awk -F': ' 'tolower($1)=="content-type"{print tolower($2)}' \
  | grep -q '^application/wasm$'
