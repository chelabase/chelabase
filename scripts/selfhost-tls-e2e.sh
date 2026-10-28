#!/usr/bin/env bash
# The self-hosted install behind a TLS-terminating proxy with no port in the hosts (#24), end to
# end, on a throwaway Compose project of its own (unique name, free ports and control subnet,
# its own .env). It never touches another project's containers and needs no workbench. Run it
# with `just selfhost-tls-e2e`.
#
#   1. certificates   a throwaway CA and a certificate for the three hosts (https, no port)
#   2. render + up    the base file on local images, plus nginx terminating TLS in front of the
#                     gateway (published on a free loopback port; curl and the socket client
#                     reach the hosts through it with the CA, so every public URL is https)
#   3. public entry   through TLS: the discovery issuer is https://auth.<domain>, Studio 200,
#                     /_gateway/ready 200
#   4. in-network     inside the gateway, realtime and orchestrator containers the public https
#                     auth URL fails (plain HTTP on 443: the bug) and CHELABASE_ZITADEL_INTERNAL_URL
#                     serves the keys (200) with the auth host as Host
#   5. sign in        claim with the setup code, sign in as a Studio user: a token whose iss is
#                     the public https issuer
#   6. gateway        GET /v1/me with it is 200 (tenant admin); a garbage bearer is 401
#   7. realtime       a ticket, a socket, the token in its auth frame, a project channel joined
#                     (realtime and the orchestrator each verify the token)
#   8. control        realtime and the orchestrator recreated without the in-network URL: the
#                     socket closes 1013; then the gateway too: /v1/me is 503 (auth unavailable).
#                     No token passes when the keys can't be fetched
#   9. restored       recreated with it again: /v1/me is 200
#  10. down -v        nothing is left behind (also on failure)
#
# Images: the local release images (`just images`, or your own build): CHELABASE_IMAGE_REPO and
# CHELABASE_IMAGE_TAG (default chelabase:latest); nothing is pulled for them, and the script never
# builds or tags an image. The TLS proxy is nginx (SELFHOST_TLS_E2E_NGINX), pulled if missing.
# SELFHOST_TLS_E2E_KEEP=1 leaves the project up (tear it down with
# `docker compose -p <name> down -v`).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dir="$root/infra/compose"
LOG_PREFIX=selfhost-tls-e2e
NGINX="${SELFHOST_TLS_E2E_NGINX:-nginx:1.29-alpine}"
REPO="${CHELABASE_IMAGE_REPO:-chelabase}"
TAG="${CHELABASE_IMAGE_TAG:-latest}"
t0=$SECONDS
tmp="$(mktemp -d)"
chmod 755 "$tmp" # nginx (uid 101) reads its config and the certificate from here
STEP="start"
PROJECT=""

say() { echo "[$LOG_PREFIX] $*"; }
pass() { echo "ok   $* ($((SECONDS - t0)) s)"; }
step() { STEP="$*"; say "step: $STEP"; }

compose() { # compose <args…>: the project's files and env
  local files=(-f "$dir/docker-compose.yml" -f "$tmp/override.yml")
  # NOFIX=1: realtime and the orchestrator without the in-network URL; NOFIX=2: the gateway too.
  [[ "${NOFIX:-0}" -lt 1 ]] || files+=(-f "$tmp/nofix.yml")
  [[ "${NOFIX:-0}" -lt 2 ]] || files+=(-f "$tmp/nofix-gateway.yml")
  docker compose -p "$PROJECT" --project-directory "$dir" "${files[@]}" --env-file "$tmp/env" "$@"
}

die() {
  echo "FAIL step '$STEP': $*" >&2
  if [[ -n "$PROJECT" ]]; then
    compose ps -a >&2 2>&1 || true
    for svc in tls gateway realtime orchestrator auth; do
      echo "---- logs $svc (tail 12)" >&2
      { compose logs --no-color --tail 12 "$svc" 2>&1 | cut -c1-300 >&2; } || true
    done
  fi
  exit 1
}

remove_leftovers() {
  local ids
  ids="$(docker ps -aq --filter "label=chelabase.dev/stack=$PROJECT" 2>/dev/null || true)"
  # shellcheck disable=SC2086
  [[ -z "$ids" ]] || docker rm -f $ids >/dev/null 2>&1 || true
}

teardown() {
  remove_leftovers
  NOFIX=0 compose down -v --remove-orphans --timeout 20 >/dev/null 2>&1 || true
  remove_leftovers
}

nothing_left() {
  local left
  left="$(docker ps -a --format '{{.Names}}' --filter "label=com.docker.compose.project=$PROJECT"
    docker ps -a --format '{{.Names}}' --filter "label=chelabase.dev/stack=$PROJECT"
    docker volume ls -q --filter "label=com.docker.compose.project=$PROJECT"
    docker network ls --format '{{.Name}}' --filter "label=com.docker.compose.project=$PROJECT")"
  [[ -z "$left" ]] || die "project $PROJECT left behind: $(echo "$left" | tr '\n' ' ')"
}

cleanup() {
  local rc=$?
  trap - EXIT
  if [[ "${SELFHOST_TLS_E2E_KEEP:-}" == 1 ]]; then
    say "SELFHOST_TLS_E2E_KEEP=1: leaving $PROJECT up ($tmp kept)"
  else
    [[ -z "$PROJECT" ]] || teardown
    rm -rf "$tmp"
  fi
  if [[ $rc -eq 0 ]]; then say "PASS in $((SECONDS - t0)) s"; else say "FAILED (exit $rc) in $((SECONDS - t0)) s"; fi
  exit $rc
}
trap cleanup EXIT

for tool in docker jq curl openssl ss node; do
  command -v "$tool" >/dev/null || die "$tool is missing"
done

free_port() {
  local port
  for _ in $(seq 60); do
    port=$((20000 + RANDOM % 12000))
    if ! ss -ltn "sport = :$port" | grep -q LISTEN && ! docker ps --format '{{.Ports}}' | grep -q ":$port->"; then
      echo "$port"
      return
    fi
  done
  die "no free port found"
}

free_octet() {
  local n subnets
  # shellcheck disable=SC2046
  subnets="$(docker network inspect $(docker network ls -q) -f '{{range .IPAM.Config}}{{.Subnet}} {{end}}' 2>/dev/null || true)"
  for _ in $(seq 60); do
    n=$((100 + RANDOM % 150))
    if ! grep -qE "(^| )10\.$n\." <<<"$subnets"; then
      echo "$n"
      return
    fi
  done
  die "no free 10.N.0.0/16 network found"
}

# ---- 1. certificates -----------------------------------------------------------------------------

step "a throwaway CA and a certificate for the three hosts"
id="$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')"
PROJECT="chelabase-selfhost-tls-e2e-$id"
DOMAIN="tls-$id.test"
STUDIO_HOST="studio.$DOMAIN" API_HOST="api.$DOMAIN" AUTH_HOST="auth.$DOMAIN"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=selfhost-tls-e2e CA $id" \
  -keyout "$tmp/ca.key" -out "$tmp/ca.pem" >/dev/null 2>&1 || die "openssl: the CA"
openssl req -newkey rsa:2048 -nodes -subj "/CN=$DOMAIN" -keyout "$tmp/key.pem" -out "$tmp/leaf.csr" \
  >/dev/null 2>&1 || die "openssl: the request"
printf 'subjectAltName=DNS:%s,DNS:%s,DNS:%s\n' "$STUDIO_HOST" "$API_HOST" "$AUTH_HOST" >"$tmp/san.ext"
openssl x509 -req -in "$tmp/leaf.csr" -CA "$tmp/ca.pem" -CAkey "$tmp/ca.key" -CAcreateserial -days 1 \
  -extfile "$tmp/san.ext" -out "$tmp/cert.pem" >/dev/null 2>&1 || die "openssl: the certificate"
chmod 644 "$tmp/key.pem" "$tmp/cert.pem"
pass "CA and certificate for $STUDIO_HOST, $API_HOST, $AUTH_HOST"

# ---- 2. render + up ------------------------------------------------------------------------------

step "render and start a fresh project behind TLS"
TLS_PORT="$(free_port)"
HTTP_PORT="$(free_port)"
[[ "$HTTP_PORT" != "$TLS_PORT" ]] || HTTP_PORT="$(free_port)"
n="$(free_octet)"
{
  grep -vE '^(CHELABASE_PUBLIC_SCHEME|CHELABASE_(STUDIO|API|AUTH|DOCS|LANDING|MAIL)_HOST)=' "$dir/.env.example"
  cat <<EOF
CHELABASE_PUBLIC_SCHEME=https
CHELABASE_STUDIO_HOST=$STUDIO_HOST
CHELABASE_API_HOST=$API_HOST
CHELABASE_AUTH_HOST=$AUTH_HOST
CHELABASE_STUDIO_HOST_NAME=$STUDIO_HOST
CHELABASE_API_HOST_NAME=$API_HOST
CHELABASE_AUTH_HOST_NAME=$AUTH_HOST
CHELABASE_PUBLIC_PORT=443
CHELABASE_PUBLIC_SECURE=true
CHELABASE_ZITADEL_TLS_MODE=external
CHELABASE_HTTP_ADDR=127.0.0.1
CHELABASE_HTTP_PORT=$HTTP_PORT
CHELABASE_IMAGE_REPO=$REPO
CHELABASE_IMAGE_TAG=$TAG
CHELABASE_CONTROL_SUBNET=10.$n.0.0/22
CHELABASE_CONTROL_COMPUTE_RANGE=10.$n.2.0/23
CHELABASE_ORCHESTRATOR_CONTROL_IP=10.$n.0.10
CHELABASE_REALTIME_CONTROL_IP=10.$n.0.12
EOF
} >"$tmp/env"
chmod 600 "$tmp/env"
cat >"$tmp/nginx.conf" <<EOF
events {}
http {
  map \$http_upgrade \$connection_upgrade { default upgrade; '' close; }
  server {
    listen 443 ssl;
    server_name $STUDIO_HOST $API_HOST $AUTH_HOST;
    ssl_certificate /tls/cert.pem;
    ssl_certificate_key /tls/key.pem;
    location / {
      # Resolved per request (Docker's DNS), so a recreated gateway is found again.
      resolver 127.0.0.11 valid=2s;
      set \$gateway http://gateway:443;
      proxy_pass \$gateway;
      proxy_http_version 1.1;
      proxy_set_header Host \$host;
      proxy_set_header Upgrade \$http_upgrade;
      proxy_set_header Connection \$connection_upgrade;
      proxy_read_timeout 120s;
    }
  }
}
EOF
# The seeds bind is this env file (no .env in the checkout); nginx in front, on loopback only.
cat >"$tmp/override.yml" <<EOF
services:
  init-secrets:
    volumes:
      - {type: bind, source: $tmp/env, target: /run/chelabase/seeds.env, read_only: true, bind: {create_host_path: false}}
  tls:
    image: $NGINX
    restart: unless-stopped
    ports:
      - "127.0.0.1:$TLS_PORT:443"
    volumes:
      - {type: bind, source: $tmp/nginx.conf, target: /etc/nginx/nginx.conf, read_only: true}
      - {type: bind, source: $tmp/cert.pem, target: /tls/cert.pem, read_only: true}
      - {type: bind, source: $tmp/key.pem, target: /tls/key.pem, read_only: true}
    depends_on:
      gateway:
        condition: service_healthy
EOF
# The negative control: the verifying services without the in-network URL.
cat >"$tmp/nofix.yml" <<'EOF'
services:
  realtime: {environment: {CHELABASE_ZITADEL_INTERNAL_URL: ""}}
  orchestrator: {environment: {CHELABASE_ZITADEL_INTERNAL_URL: ""}}
EOF
cat >"$tmp/nofix-gateway.yml" <<'EOF'
services:
  gateway: {environment: {CHELABASE_ZITADEL_INTERNAL_URL: ""}}
EOF
say "project $PROJECT: https://$STUDIO_HOST through 127.0.0.1:$TLS_PORT (images $REPO/*:$TAG)"
for svc in gateway orchestrator realtime auth init; do
  docker image inspect "$REPO/$svc:$TAG" >/dev/null 2>&1 ||
    die "no local image $REPO/$svc:$TAG: run \`just images\` (or set CHELABASE_IMAGE_REPO/CHELABASE_IMAGE_TAG)"
done
docker image inspect "$NGINX" >/dev/null 2>&1 || docker pull -q "$NGINX" >/dev/null || die "docker pull $NGINX"
compose config --format json >"$tmp/config.json" 2>"$tmp/config.err" || die "docker compose config: $(cat "$tmp/config.err")"
HELPER="$(jq -r '.services["metadata-db"].image' "$tmp/config.json")"
if ! compose up -d --wait --wait-timeout 900 --pull never >"$tmp/up" 2>&1; then
  tail -n 40 "$tmp/up" >&2
  die "docker compose up --wait failed"
fi
pass "up --wait finished"

# Every public request goes through the TLS proxy, verified against the throwaway CA.
CURL=(curl -sS --max-time 60 --cacert "$tmp/ca.pem" --connect-to "::127.0.0.1:$TLS_PORT")
STUDIO="https://$STUDIO_HOST" API="https://$API_HOST" AUTH="https://$AUTH_HOST"

# ---- 3. public entry -----------------------------------------------------------------------------

step "the public entry answers over TLS"
issuer="$("${CURL[@]}" "$AUTH/.well-known/openid-configuration" | jq -r .issuer)" || die "discovery"
[[ "$issuer" == "$AUTH" ]] || die "the discovery issuer is '$issuer', not $AUTH"
[[ "$("${CURL[@]}" -o /dev/null -w '%{http_code}' "$STUDIO/")" == 200 ]] || die "Studio is not 200 at $STUDIO/"
[[ "$("${CURL[@]}" -o /dev/null -w '%{http_code}' "$API/_gateway/ready")" == 200 ]] || die "/_gateway/ready is not 200"
pass "issuer $issuer, Studio 200, the gateway ready"

# ---- 4. in-network -------------------------------------------------------------------------------

step "inside the network the keys come from the in-network URL, not the public one"
for svc in gateway realtime orchestrator; do
  out="$(compose exec -T "$svc" sh -c '
    printf "%s " "$CHELABASE_ZITADEL_INTERNAL_URL"
    curl -sS -o /dev/null -w "%{http_code} " -H "Host: $1" "$CHELABASE_ZITADEL_INTERNAL_URL/oauth/v2/keys" || printf "fail "
    curl -sS -o /dev/null --max-time 10 "https://$1/oauth/v2/keys" 2>/dev/null && echo public-ok || echo public-fails
  ' sh "$AUTH_HOST" 2>&1)" || die "$svc: exec failed: $out"
  [[ "$out" == "http://zitadel:8080 200 public-fails" ]] || die "$svc: '$out' (wanted the in-network URL, 200, the public URL failing)"
done
pass "gateway, realtime, orchestrator: in-network keys 200, the public https URL fails inside"

# ---- 5. sign in ----------------------------------------------------------------------------------

step "claim the instance and sign in as a Studio user, through TLS"
vol_cat() {
  docker run --rm --user 10001:10001 --network none -v "${PROJECT}_secrets:/s:ro" --entrypoint cat "$HELPER" "/s/$1"
}
compose logs --no-color init-secrets >"$tmp/init.log" 2>&1 || die "logs init-secrets"
CODE="$(sed -nE 's/.*Setup code: ([A-Z0-9]{4}(-[A-Z0-9]{4}){4}).*/\1/p' "$tmp/init.log" | head -n1)"
[[ -n "$CODE" ]] || die "no setup code in the init-secrets logs"
PROJECT_ID="$(vol_cat bootstrap/project-id | tr -d '\r\n')"
CLIENT="$(vol_cat bootstrap/studio-client-id | tr -d '\r\n')"
[[ -n "$PROJECT_ID" && -n "$CLIENT" ]] || die "the bootstrap's ids are missing"
PASSWORD="Selfhost-tls-$(openssl rand -hex 6)A!"
EMAIL="admin-$(openssl rand -hex 3)@example.com"
b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
authorize() {
  local verifier challenge loc
  verifier="$(openssl rand -hex 32)"
  challenge="$(printf %s "$verifier" | openssl dgst -sha256 -binary | b64url)"
  echo "$verifier" >"$tmp/verifier"
  loc="$("${CURL[@]}" -o /dev/null -w '%{redirect_url}' -G "$AUTH/oauth/v2/authorize" \
    --data-urlencode "client_id=$CLIENT" --data-urlencode "redirect_uri=$STUDIO/callback" \
    --data-urlencode "response_type=code" \
    --data-urlencode "scope=openid profile email urn:zitadel:iam:user:resourceowner urn:zitadel:iam:org:project:id:${PROJECT_ID}:aud" \
    --data-urlencode "code_challenge=$challenge" --data-urlencode "code_challenge_method=S256")" || die "authorize"
  [[ "$loc" == "$STUDIO/login?authRequest="* ]] || die "authorize redirected to '$loc'"
  echo "${loc#*authRequest=}"
}
ui() { # ui <path> <json>: Studio's login API; body to stdout, status in $tmp/ui-status
  local out
  out="$("${CURL[@]}" -b "$tmp/jar" -c "$tmp/jar" -H "Origin: $STUDIO" -H 'Content-Type: application/json' \
    -w '\n%{http_code}' -X POST "$API/auth/v1/$1" -d "$2")" || die "POST /auth/v1/$1"
  printf '%s' "${out##*$'\n'}" >"$tmp/ui-status"
  printf '%s' "${out%$'\n'*}"
}
: >"$tmp/jar"
out="$(ui sign-up "$(jq -nc --arg ar "$(authorize)" --arg e "$EMAIL" --arg p "$PASSWORD" --arg c "$CODE" \
  '{authRequest: $ar, email: $e, givenName: "Self", familyName: "Host", password: $p, setupCode: $c}')")"
[[ "$(cat "$tmp/ui-status")" == 200 ]] || die "sign-up with the code: $(cat "$tmp/ui-status") $out"
out="$(ui sign-in "$(jq -nc --arg ar "$(authorize)" --arg e "$EMAIL" --arg p "$PASSWORD" '{authRequest: $ar, email: $e, password: $p}')")"
[[ "$(jq -r '.next // empty' <<<"$out")" == done ]] || die "sign-in: $(cat "$tmp/ui-status") $out"
code="$(jq -r .redirect <<<"$out" | sed -E 's/.*[?&]code=([^&]+).*/\1/')"
TOKEN="$("${CURL[@]}" -X POST "$AUTH/oauth/v2/token" -d grant_type=authorization_code -d "code=$code" \
  -d "redirect_uri=$STUDIO/callback" -d "client_id=$CLIENT" -d "code_verifier=$(cat "$tmp/verifier")" | jq -r .access_token)"
[[ -n "$TOKEN" && "$TOKEN" != null ]] || die "no access token"
payload="$(cut -d. -f2 <<<"$TOKEN" | tr '_-' '/+')"
while ((${#payload} % 4)); do payload+="="; done
iss="$(base64 -d <<<"$payload" 2>/dev/null | jq -r .iss)"
[[ "$iss" == "$AUTH" ]] || die "the token's iss is '$iss', not $AUTH"
pass "signed in; the token's iss is $iss"

# ---- 6. gateway ----------------------------------------------------------------------------------

me() { "${CURL[@]}" -o "$tmp/body" -w '%{http_code}' "$API/v1/me" -H "Authorization: Bearer $1" || echo 000; }
step "the gateway verifies the token"
status="$(me "$TOKEN")"
[[ "$status" == 200 ]] || die "GET /v1/me: $status $(cat "$tmp/body")"
jq -e --arg e "$EMAIL" '.member.tenant_admin == true and .email == $e' "$tmp/body" >/dev/null ||
  die "/v1/me: $(jq -c '{email, member}' "$tmp/body")"
status="$(me "x.y.z")"
[[ "$status" == 401 ]] || die "a garbage bearer: $status (wanted 401) $(cat "$tmp/body")"
pass "GET /v1/me 200 as tenant admin; a garbage bearer 401"

# ---- 7. realtime ---------------------------------------------------------------------------------

# socket_join <ticket> <channel>: the socket's first frames; prints `joined`, `closed <code>` or
# `timeout`. A minimal RFC 6455 client over TLS (Node's own WebSocket can't pin a proxy address).
socket_join() {
  SOCK_CA="$tmp/ca.pem" SOCK_PORT="$TLS_PORT" SOCK_HOST="$API_HOST" SOCK_ORIGIN="$STUDIO" \
    SOCK_TICKET="$1" SOCK_CHANNEL="$2" SOCK_TOKEN="$TOKEN" node -e '
const tls = require("tls"), crypto = require("crypto"), fs = require("fs");
const e = process.env;
const s = tls.connect({ host: "127.0.0.1", port: +e.SOCK_PORT, servername: e.SOCK_HOST, ca: fs.readFileSync(e.SOCK_CA) });
const done = (m) => { console.log(m); process.exit(0); };
setTimeout(() => done("timeout"), 30000);
const send = (obj) => {
  const p = Buffer.from(JSON.stringify(obj)), mask = crypto.randomBytes(4);
  const head = p.length < 126 ? Buffer.from([0x81, 0x80 | p.length])
    : Buffer.from([0x81, 0xfe, p.length >> 8, p.length & 255]);
  const body = Buffer.from(p.map((b, i) => b ^ mask[i % 4]));
  s.write(Buffer.concat([head, mask, body]));
};
let buf = Buffer.alloc(0), upgraded = false;
s.on("secureConnect", () => s.write(
  `GET /v1/realtime/socket?ticket=${encodeURIComponent(e.SOCK_TICKET)} HTTP/1.1\r\nHost: ${e.SOCK_HOST}\r\n` +
  `Origin: ${e.SOCK_ORIGIN}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n` +
  `Sec-WebSocket-Key: ${crypto.randomBytes(16).toString("base64")}\r\nSec-WebSocket-Protocol: chelabase.realtime.v1\r\n\r\n`));
s.on("data", (d) => {
  buf = Buffer.concat([buf, d]);
  if (!upgraded) {
    const end = buf.indexOf("\r\n\r\n");
    if (end < 0) return;
    const head = buf.slice(0, end).toString();
    if (!head.startsWith("HTTP/1.1 101")) done("refused " + head.split("\r\n")[0]);
    upgraded = true; buf = buf.slice(end + 4);
    send({ type: "auth", token: e.SOCK_TOKEN });
    send({ type: "join", ref: "1", channel: e.SOCK_CHANNEL });
  }
  while (buf.length >= 2) {
    let len = buf[1] & 127, off = 2;
    if (len === 126) { if (buf.length < 4) return; len = buf.readUInt16BE(2); off = 4; }
    else if (len === 127) { if (buf.length < 10) return; len = Number(buf.readBigUInt64BE(2)); off = 10; }
    if (buf.length < off + len) return;
    const op = buf[0] & 15, payload = buf.slice(off, off + len);
    buf = buf.slice(off + len);
    if (op === 8) done("closed " + (payload.length >= 2 ? payload.readUInt16BE(0) : "none"));
    if (op === 1) {
      const f = JSON.parse(payload.toString());
      if (f.type === "joined") done("joined");
      if (f.type === "error") done("error " + f.code);
    }
  }
});
s.on("error", (err) => done("error " + err.message));
s.on("close", () => done("closed tcp"));
'
}
ticket() {
  "${CURL[@]}" -o "$tmp/body" -w '%{http_code}' -X POST "$API/v1/realtime/tickets" -H "Authorization: Bearer $TOKEN" \
    -H "Origin: $STUDIO" || echo 000
}
step "realtime and the orchestrator verify the token (a socket joins a project channel)"
status="$("${CURL[@]}" -o "$tmp/body" -w '%{http_code}' -X POST "$API/v1/projects" -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{"name":"tls-e2e"}')" || status=000
[[ "$status" == 201 || "$status" == 200 ]] || die "POST /v1/projects: $status $(cat "$tmp/body")"
project="$(jq -r '.id // .project_id' "$tmp/body")"
status="$(ticket)"
[[ "$status" == 201 ]] || die "POST /v1/realtime/tickets: $status $(cat "$tmp/body")"
got="$(socket_join "$(jq -r .ticket "$tmp/body")" "project:$project")"
[[ "$got" == joined ]] || die "the socket with the token: $got (wanted joined)"
pass "ticket 201; the socket's auth frame verified and project:$project joined"

# ---- 8. negative control -------------------------------------------------------------------------

step "without the in-network URL no token passes (the control)"
# Realtime and the orchestrator first (the gateway still verifies, so a ticket is issued).
NOFIX=1 compose up -d --wait --wait-timeout 300 --pull never realtime orchestrator >"$tmp/nofix.up" 2>&1 ||
  { tail -n 20 "$tmp/nofix.up" >&2; die "recreating realtime and the orchestrator without the in-network URL failed"; }
status="$(ticket)"
[[ "$status" == 201 ]] || die "POST /v1/realtime/tickets (control): $status $(cat "$tmp/body")"
got="$(socket_join "$(jq -r .ticket "$tmp/body")" "project:$project")"
[[ "$got" == "closed 1013" ]] || die "the socket without the in-network URL: $got (wanted closed 1013)"
# Then the gateway.
NOFIX=2 compose up -d --wait --wait-timeout 300 --pull never gateway >"$tmp/nofix-gw.up" 2>&1 ||
  { tail -n 20 "$tmp/nofix-gw.up" >&2; die "recreating the gateway without the in-network URL failed"; }
status="$(me "$TOKEN")"
[[ "$status" == 503 ]] || die "GET /v1/me without the in-network URL: $status (wanted 503) $(cat "$tmp/body")"
jq -e '.error.code == "unavailable"' "$tmp/body" >/dev/null || die "/v1/me: $(cat "$tmp/body")"
compose logs --no-color --since 5m gateway >"$tmp/nofix.log" 2>&1 || true
grep -q "jwks fetch failed" "$tmp/nofix.log" || die "the gateway logged no failed key fetch"
pass "the socket closed 1013 (realtime can't fetch keys); GET /v1/me 503 unavailable (the gateway can't)"

# ---- 9. restored ---------------------------------------------------------------------------------

step "with the in-network URL again, the token verifies"
NOFIX=0 compose up -d --wait --wait-timeout 300 --pull never gateway realtime orchestrator >"$tmp/refix.up" 2>&1 ||
  { tail -n 20 "$tmp/refix.up" >&2; die "recreating with the in-network URL failed"; }
status="$(me "$TOKEN")"
[[ "$status" == 200 ]] || die "GET /v1/me after restoring: $status $(cat "$tmp/body")"
pass "GET /v1/me 200"

# ---- 10. down -v ---------------------------------------------------------------------------------

step "down -v leaves nothing behind"
[[ "${SELFHOST_TLS_E2E_KEEP:-}" == 1 ]] && exit 0
teardown
nothing_left
pass "no container, volume or network of $PROJECT is left"
PROJECT=""
