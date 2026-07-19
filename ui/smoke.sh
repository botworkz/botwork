#!/usr/bin/env bash
#
# Per-service container smoke test for `botwork/ui:local`.
#
# Local reproduction:
#
#   docker buildx build --platform linux/amd64 --load \
#     -t botwork/ui:local -f ui/Dockerfile .
#   bash ui/smoke.sh

set -euo pipefail

# End-to-end production-path proof for ui (RFE #106
# follow-up — Leptos control panel skeleton, Phase 2 reshape
# from botworkz/space#311):
#
#   1. boot ui on a throwaway docker network with the
#      `admin_ui` alias production uses (no postgres, no
#      db-migrate — ui is a pure static-bundle server,
#      it has no DB connection);
#   2. curl /healthz from a sibling client container, assert
#      the JSON contract field;
#   3. curl /login from a sibling client container, assert
#      we get an HTML body containing the trunk-stamped
#      marker string — this is the proof that the multi-
#      stage Dockerfile actually ran `trunk build` and the
#      include_dir! macro saw a populated dist/. Post-Phase-2
#      the entry-point routes are /login (unauthed) and
#      /{tenant}/* (SPA shell); both serve the same index.html
#      and we use /login here because it doesn't require a
#      tenant context.
#
# No "fail-loud on missing URL" arm here because ui
# has no required env. The bind address has a default and
# the bundle is baked into the binary.
net="botwork-smoke-ui-$$"
ui="botwork-smoke-ui-svc-$$"
trap "docker rm -f ${ui} >/dev/null 2>&1 || true; \
      docker network rm ${net} >/dev/null 2>&1 || true" EXIT
docker network create "${net}" >/dev/null

docker run -d --name "${ui}" \
  --network "${net}" --network-alias admin_ui \
  botwork/ui:local >/dev/null

# Wait for the listener to bind. The startup log line is
# `[ui] starting on 0.0.0.0:9500`; grep for it rather
# than sleeping a fixed amount so a slow scheduler doesn't
# flake the test.
ready=0
for _ in $(seq 1 30); do
  if docker logs "${ui}" 2>&1 | grep -q "\[ui\] starting on 0.0.0.0:9500"; then
    ready=1
    break
  fi
  sleep 1
done
if [[ "${ready}" -ne 1 ]]; then
  echo "ui did not bind in time" >&2
  docker logs "${ui}" >&2 || true
  exit 1
fi

# 2. — /healthz.
body="$(docker run --rm --network "${net}" curlimages/curl:8.10.1 \
  --fail --silent --show-error \
  http://admin_ui:9500/healthz)"
echo "healthz body: ${body}"
echo "${body}" | grep -q '"status":"ok"'

# 3. — /login should return the trunk-built shell, not 404.
# We grep for the page <title> we set in ui/wasm/index.html;
# if the include_dir! bundle is empty this assertion fails
# loudly rather than the more confusing "404 from the server".
body="$(docker run --rm --network "${net}" curlimages/curl:8.10.1 \
  --fail --silent --show-error \
  http://admin_ui:9500/login)"
echo "login body bytes: $(printf '%s' "${body}" | wc -c)"
echo "${body}" | grep -q "botwork ui"
echo "${body}" | grep -q "data-trunk"

# 4. — sub-resource path check. The bug this catches:
# if Trunk.toml drops `public_url = "/static/"`, trunk emits
# `<link rel="modulepreload" href="/<hash>.js">` instead of
# `<link rel="modulepreload" href="/static/<hash>.js">`. In
# the bare container the ui server then 404s those
# root-relative paths, and the browser surfaces it as
# `NS_ERROR_CORRUPTED_CONTENT`. Through envoy (vm/space) the
# same path 401s on ext_authz. Either way the panel is
# broken in a way the existing "/login returns HTML" check
# doesn't see.
#
# We:
#   a. parse the HTML for the wasm-bindgen JS loader URL
#      (trunk emits it as `<link rel="modulepreload"
#      href="...">`),
#   b. assert the URL starts with `/static/` — root-relative
#      `/foo.js` is the specific failure mode this probe
#      catches,
#   c. curl it and assert 200 + a JS-ish content-type,
#   d. do the same for the `.wasm` blob (preload href),
#   e. do the same for the baseline stylesheet (also a
#      data-trunk asset pipelined under public_url; the
#      panel still renders without it but is operator-
#      hostile, so we want the same fail-loud).
js_url="$(printf '%s' "${body}" \
  | grep -oE 'href="[^"]+\.js"' \
  | head -1 \
  | sed -E 's/^href="([^"]+)"$/\1/')"
if [[ -z "${js_url}" ]]; then
  echo "could not find a JS bundle href in /login body" >&2
  printf '%s\n' "${body}" >&2
  exit 1
fi
echo "js_url: ${js_url}"
case "${js_url}" in
  /static/*) ;;
  *)
    echo "JS bundle href ${js_url} is not /static/-prefixed" >&2
    echo "this means Trunk.toml lost public_url = \"/static/\"" >&2
    exit 1
    ;;
esac

wasm_url="$(printf '%s' "${body}" \
  | grep -oE 'href="[^"]+\.wasm"' \
  | head -1 \
  | sed -E 's/^href="([^"]+)"$/\1/')"
if [[ -z "${wasm_url}" ]]; then
  echo "could not find a wasm preload href in /login body" >&2
  printf '%s\n' "${body}" >&2
  exit 1
fi
echo "wasm_url: ${wasm_url}"
case "${wasm_url}" in
  /static/*) ;;
  *)
    echo "wasm preload href ${wasm_url} is not /static/-prefixed" >&2
    echo "this means Trunk.toml lost public_url = \"/static/\"" >&2
    exit 1
    ;;
esac

# Baseline stylesheet. Trunk pipelines `<link data-trunk rel="css">`
# and emits a fingerprinted `<hash>.css` under public_url. We
# grep for the rendered href (the data-trunk attribute is stripped
# from the emitted HTML, so it doesn't show up in step 3's grep) and
# enforce the same `/static/` prefix invariant as JS/wasm.
css_url="$(printf '%s' "${body}" \
  | grep -oE 'href="[^"]+\.css"' \
  | head -1 \
  | sed -E 's/^href="([^"]+)"$/\1/')"
if [[ -z "${css_url}" ]]; then
  echo "could not find a CSS href in /login body" >&2
  echo "did the data-trunk rel=\"css\" directive get dropped from index.html?" >&2
  printf '%s\n' "${body}" >&2
  exit 1
fi
echo "css_url: ${css_url}"
case "${css_url}" in
  /static/*) ;;
  *)
    echo "css href ${css_url} is not /static/-prefixed" >&2
    echo "this means Trunk.toml lost public_url = \"/static/\"" >&2
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

# e. — stylesheet must serve 200 + text/css. Catches "trunk
# emitted the link but the file is missing from dist/" (an
# include_dir! footgun if the file was renamed without
# updating index.html), distinct from step (3)'s "is index.html
# in the bundle".
hdrs="$(docker run --rm --network "${net}" curlimages/curl:8.10.1 \
  --fail --silent --show-error --dump-header - --output /dev/null \
  "http://admin_ui:9500${css_url}")"
echo "css headers:"
printf '%s\n' "${hdrs}"
printf '%s\n' "${hdrs}" \
  | tr -d '\r' \
  | awk -F': ' 'tolower($1)=="content-type"{print tolower($2)}' \
  | grep -Eq '^text/css'
