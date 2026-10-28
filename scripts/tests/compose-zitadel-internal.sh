#!/usr/bin/env bash
# Offline check (#24): in the self-hosted install, every app that calls Zitadel (the gateway,
# realtime and the orchestrator for keys; auth, the orchestrator and the worker for its API)
# reaches it inside the network, in plain HTTP, never at the public auth URL. Behind a TLS
# proxy with no port in the hosts (https://auth.example.com) the public URL resolves, inside
# the network, to the gateway's alias on 443, which speaks plain HTTP: the call can't work.
#
# For the default .env.example and for an https/no-port .env, the rendered base config gives
# each of those apps CHELABASE_ZITADEL_INTERNAL_URL = the gateway's own Zitadel upstream (an
# http:// service name, not a gateway alias), no CHELABASE_ZITADEL_JWKS_URL (which would win),
# a network shared with Zitadel, and the public scheme and auth host the issuer comes from. The
# workbench render (base + overlay) gives no app the in-network URL.
#
#   scripts/tests/compose-zitadel-internal.sh
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
dir="$root/infra/compose"
fail=0
err() {
  echo "compose-zitadel-internal: $*" >&2
  fail=1
}
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

render() { # render <env file>
  env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -p chelabase-zitadel-internal \
    --project-directory "$dir" -f "$dir/docker-compose.yml" --env-file "$1" config --format json
}

# check <label> <env file> <scheme> <auth host>
check() {
  local label="$1" json
  json="$(render "$2")" || { err "$label: rendering the base file failed"; return; }
  local scheme="$3" auth="$4"
  local upstream
  upstream="$(jq -r '.services.gateway.environment.CHELABASE_ZITADEL_UPSTREAM // ""' <<<"$json")"
  [[ "$upstream" =~ ^http://[a-z0-9-]+:[0-9]+$ ]] ||
    err "$label: the gateway's CHELABASE_ZITADEL_UPSTREAM is ${upstream:-unset}, not http://<service>:<port>"
  local zhost="${upstream#http://}"
  zhost="${zhost%%:*}"
  jq -e --arg s "$zhost" '.services[$s] != null' <<<"$json" >/dev/null ||
    err "$label: $upstream names no service"
  local aliases
  aliases="$(jq -r '.services.gateway.networks.default.aliases // [] | .[]' <<<"$json")"
  if grep -qxF "$zhost" <<<"$aliases"; then
    err "$label: $zhost is one of the gateway's aliases"
  fi
  for svc in gateway realtime orchestrator auth worker; do
    local e
    e="$(jq -c --arg s "$svc" '.services[$s].environment // {}' <<<"$json")"
    local internal jwks got_scheme got_auth
    internal="$(jq -r '.CHELABASE_ZITADEL_INTERNAL_URL // ""' <<<"$e")"
    jwks="$(jq -r '.CHELABASE_ZITADEL_JWKS_URL // ""' <<<"$e")"
    got_scheme="$(jq -r '.CHELABASE_PUBLIC_SCHEME // ""' <<<"$e")"
    got_auth="$(jq -r '.CHELABASE_AUTH_HOST // ""' <<<"$e")"
    [[ "$internal" == "$upstream" ]] ||
      err "$label: $svc has CHELABASE_ZITADEL_INTERNAL_URL=${internal:-unset}, want $upstream"
    [[ -z "$jwks" ]] || err "$label: $svc sets CHELABASE_ZITADEL_JWKS_URL=$jwks (it wins over the in-network URL)"
    [[ "$got_scheme" == "$scheme" ]] || err "$label: $svc has CHELABASE_PUBLIC_SCHEME=$got_scheme, want $scheme"
    [[ "$got_auth" == "$auth" ]] || err "$label: $svc has CHELABASE_AUTH_HOST=$got_auth, want $auth"
    jq -e --arg s "$svc" --arg z "$zhost" '
      ((.services[$s].networks // {"default": {}}) | keys) as $a
      | ((.services[$z].networks // {"default": {}}) | keys) as $b
      | any($a[]; . as $n | $b | index($n))' <<<"$json" >/dev/null ||
      err "$label: $svc shares no network with $zhost"
  done
}

check "default .env.example" "$dir/.env.example" http auth.chelabase.localhost:8000

# An https install behind a TLS proxy with no port in the hosts.
grep -vE '^(CHELABASE_PUBLIC_SCHEME|CHELABASE_(STUDIO|API|AUTH|DOCS|LANDING|MAIL)_HOST)=' \
  "$dir/.env.example" >"$tmp/https.env"
cat >>"$tmp/https.env" <<'EOF'
CHELABASE_PUBLIC_SCHEME=https
CHELABASE_STUDIO_HOST=studio.example.com
CHELABASE_API_HOST=api.example.com
CHELABASE_AUTH_HOST=auth.example.com
CHELABASE_STUDIO_HOST_NAME=studio.example.com
CHELABASE_API_HOST_NAME=api.example.com
CHELABASE_AUTH_HOST_NAME=auth.example.com
CHELABASE_PUBLIC_PORT=443
CHELABASE_PUBLIC_SECURE=true
CHELABASE_ZITADEL_TLS_MODE=external
EOF
check "https, no port" "$tmp/https.env" https auth.example.com

# The workbench (base + overlay) replaces every app's environment with its own env files
# (stack.env, rendered from the uncommented lines of .env.example): it doesn't get the base
# file's in-network URL, so its apps keep calling Zitadel at the issuer (through the gateway)
# and fetch keys at its CHELABASE_ZITADEL_JWKS_URL, as before #24.
wb="$(env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -p chelabase-zitadel-internal-wb \
  --project-directory "$dir" -f "$dir/docker-compose.yml" -f "$dir/docker-compose.workbench.yml" \
  --env-file "$root/scripts/tests/fixtures/workbench-stack.env" --profile sites --profile runner \
  config --format json)" || err "workbench: rendering base + overlay failed"
leak="$(jq -r '.services | to_entries[] | select(.value.environment.CHELABASE_ZITADEL_INTERNAL_URL != null) | .key' <<<"${wb:-"{}"}")"
[[ -z "$leak" ]] || err "workbench: these services get CHELABASE_ZITADEL_INTERNAL_URL: $leak"
if grep -qE '^CHELABASE_ZITADEL_INTERNAL_URL=' "$dir/.env.example"; then
  err ".env.example sets CHELABASE_ZITADEL_INTERNAL_URL uncommented: it would reach every workbench stack.env"
fi

if ((fail)); then
  exit 1
fi
echo "compose-zitadel-internal: ok"
