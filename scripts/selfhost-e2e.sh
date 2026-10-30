#!/usr/bin/env bash
# End-to-end test of the self-hosted install (epic selfhost-install, m9): what a self-hoster does,
# on a throwaway Compose project of its own (unique name, free port and control subnet, its own
# .env). It never touches another project's containers and needs no workbench. Run it with
# `just selfhost-e2e`.
#
#   1. render + up      a fresh `docker compose up --wait` from infra/compose
#   2. healthy          every service is healthy or a one-shot that exited 0; with SMTP set (a
#                      throwaway Mailpit, the password file at the documented owner and mode) the
#                      bootstrap succeeded and Zitadel's SMTP configuration is active
#   3. setup code       the code is in `logs init-secrets` and the documented command prints it
#   4. code required    sign-up without the code, and with a wrong one, is 403 setup_code_invalid
#   5. claim            sign-up with the code, sign-in, GET /v1/me: tenant admin and instance admin
#   6. claimed          the code no longer opens the instance, and sign-up is closed (403
#                      sign_up_closed: open sign-up is off by default)
#   7. data             create a project and a database through the API; the connection details
#                      name the published proxy (<ep>.localtest.me, CHELABASE_PROXY_PORT); with the
#                      CA fetched as the docs say, psql on the host (verify-full) creates and reads
#                      a row, and SQL over HTTP on the host answers
#   8. webhook          a signed Zitadel user.deactivated event is answered 200 (a bad signature 401)
#   9. no secret leaks  no secret file value appears in any container's `docker inspect`
#  10. secret reads     every running service reads its mounted secrets as its own uid and groups
#  11. rerun            a second `up --wait` changes no secret, container or log line of the code
#  12. uninstall        the documented uninstall (computes still running) leaves no container,
#                      volume or network
#  13. failing install  in a second project, an SMTP host with no sender fails `up --wait` at init-bootstrap
#
# Images: by default `-f docker-compose.build.yml` builds them from this checkout (the cache is
# shared with `just images`). SELFHOST_E2E_IMAGES=local runs the base file on the images
# `just images` already built (CHELABASE_IMAGE_REPO / CHELABASE_IMAGE_TAG, default chelabase:latest;
# nothing is pulled for them). For a mixed release set, CHELABASE_IMAGE_TAG_<IMAGE> (e.g.
# CHELABASE_IMAGE_TAG_STUDIO) tags one image; once any is set, the images with neither it nor
# CHELABASE_IMAGE_TAG stay at their compose pins (pulled by digest when missing).
#
# Other switches: SELFHOST_E2E_SKIP_CODE=1 stubs the code check out (step 4 expects 200 instead of
# 403, so the script must fail there: this shows its failure path). SELFHOST_E2E_KEEP=1 leaves the
# project up (tear it down with the install docs' uninstall, `-p <name>` added). SELFHOST_E2E_PORT
# picks the port.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dir="$root/infra/compose"
LOG_PREFIX=selfhost-e2e
t0=$SECONDS
tmp="$(mktemp -d)"
STEP="start"
PROJECT=""
FAIL_PROJECT=""
TORN_DOWN=1
ADMIN_TOKEN=""

say() { echo "[$LOG_PREFIX] $*"; }
pass() { echo "ok   $* ($((SECONDS - t0)) s)"; }
step() { STEP="$*"; say "step: $STEP"; }

# ---- failure and cleanup -------------------------------------------------------------------------

# compose <project> <args…>: docker compose on the checkout's files and the project's env.
compose() {
  local project="$1"
  shift
  docker compose -p "$project" --project-directory "$dir" "${COMPOSE_FILES[@]}" --env-file "$tmp/$project.env" "$@"
}

tail_logs() { # tail_logs <project>
  local p="$1" svc
  echo "---- docker compose ps -a ($p)" >&2
  compose "$p" ps -a >&2 2>&1 || true
  # Whatever is not healthy, plus the app tier, whose logs usually say why a step failed.
  for svc in init-secrets init-bootstrap orchestrator auth gateway; do
    echo "---- logs $svc (tail 12)" >&2
    { compose "$p" logs --no-color --tail 12 "$svc" 2>&1 | cut -c1-300 >&2; } || true
  done
}

die() {
  echo "FAIL step '$STEP': $*" >&2
  [[ -z "$PROJECT" ]] || tail_logs "$PROJECT"
  exit 1
}

# remove_leftovers <project>: containers the stack's worker started (computes) are not Compose's,
# so `down -v` leaves them; they carry the chelabase.dev/stack label (CHELABASE_STACK = the project).
remove_leftovers() {
  local p="$1" ids
  ids="$(docker ps -aq --filter "label=chelabase.dev/stack=$p" 2>/dev/null || true)"
  # shellcheck disable=SC2086
  [[ -z "$ids" ]] || docker rm -f $ids >/dev/null 2>&1 || true
}

# uninstall <project>: the install docs' uninstall ("Stop, tear down and uninstall"), word for
# word but for the project flag: stop the stack, remove the computes by their label, `down -v`.
uninstall() {
  local p="$1"
  compose "$p" stop --timeout 20 &&
    docker ps -aq --filter "label=chelabase.dev/stack=$p" | xargs -r docker rm -f >/dev/null &&
    compose "$p" down -v --timeout 20
}

teardown() { # teardown <project>: the documented uninstall, then a fallback for whatever it left
  local p="$1"
  uninstall "$p" >/dev/null 2>&1 || true
  remove_leftovers "$p"
  compose "$p" down -v --remove-orphans --timeout 20 >/dev/null 2>&1 || true
}

# nothing_left <project>: dies, naming what is left of the project.
nothing_left() {
  local p="$1" left
  left="$(docker ps -a --format '{{.Names}}' --filter "label=com.docker.compose.project=$p"
    docker ps -a --format '{{.Names}}' --filter "label=chelabase.dev/stack=$p"
    docker volume ls -q --filter "label=com.docker.compose.project=$p"
    docker network ls --format '{{.Name}}' --filter "label=com.docker.compose.project=$p")"
  [[ -z "$left" ]] || die "project $p left behind: $(echo "$left" | tr '\n' ' ')"
}

cleanup() {
  local rc=$?
  trap - EXIT
  if [[ "${SELFHOST_E2E_KEEP:-}" == 1 ]]; then
    say "SELFHOST_E2E_KEEP=1: leaving $PROJECT up ($tmp kept)"
  else
    for p in "$PROJECT" "$FAIL_PROJECT"; do
      [[ -z "$p" ]] || teardown "$p"
    done
    rm -rf "$tmp"
  fi
  if [[ $rc -eq 0 ]]; then
    say "PASS in $((SECONDS - t0)) s"
  else
    say "FAILED (exit $rc) in $((SECONDS - t0)) s"
  fi
  exit $rc
}
trap cleanup EXIT

# ---- helpers -------------------------------------------------------------------------------------

for tool in docker jq curl openssl ss psql; do
  command -v "$tool" >/dev/null || die "$tool is missing"
done

# free_port: a random high port nobody listens on and no container publishes.
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

# free_octet: a second octet N so that 10.N.0.0/16 overlaps no Docker network (the install's
# control subnet is a /22 in it).
free_octet() {
  local n subnets
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

# write_env <project> <port> [extra lines…]: the install's .env: .env.example with the hosts moved
# to the port, the settings Compose can't derive, a private control subnet and free proxy ports.
write_env() {
  local p="$1" port="$2" n pg_port http_port
  shift 2
  n="$(free_octet)"
  pg_port="$(free_port)"
  http_port="$(free_port)"
  while [[ "$http_port" == "$pg_port" || "$http_port" == "$port" ]]; do http_port="$(free_port)"; done
  {
    sed -E "s/^(CHELABASE_[A-Z]+_HOST=.*):8000\$/\1:$port/" "$dir/.env.example"
    echo "CHELABASE_STUDIO_HOST_NAME=studio.chelabase.localhost"
    echo "CHELABASE_API_HOST_NAME=api.chelabase.localhost"
    echo "CHELABASE_AUTH_HOST_NAME=auth.chelabase.localhost"
    echo "CHELABASE_PUBLIC_PORT=$port"
    echo "CHELABASE_PUBLIC_SECURE=false"
    echo "CHELABASE_ZITADEL_TLS_MODE=disabled"
    echo "CHELABASE_CONTROL_SUBNET=10.$n.0.0/22"
    echo "CHELABASE_CONTROL_COMPUTE_RANGE=10.$n.2.0/23"
    echo "CHELABASE_ORCHESTRATOR_CONTROL_IP=10.$n.0.10"
    echo "CHELABASE_REALTIME_CONTROL_IP=10.$n.0.12"
    echo "CHELABASE_PROXY_PORT=$pg_port"
    echo "CHELABASE_PROXY_HTTP_PORT=$http_port"
    if [[ "${SELFHOST_E2E_IMAGES:-}" == local ]]; then
      echo "CHELABASE_IMAGE_REPO=${CHELABASE_IMAGE_REPO:-chelabase}"
      echo "CHELABASE_IMAGE_TAG=${CHELABASE_IMAGE_TAG:-latest}"
    fi
    for line in "$@"; do echo "$line"; done
  } >"$tmp/$p.env"
  chmod 600 "$tmp/$p.env"
  # The bind of ./.env (the seeds) is this file instead, so no .env is written into the checkout.
  cat >"$tmp/$p.override.yml" <<EOF
services:
  init-secrets:
    volumes:
      - {type: bind, source: $tmp/$p.env, target: /run/chelabase/seeds.env, read_only: true, bind: {create_host_path: false}}
EOF
  if [[ -n "${WITH_MAILPIT:-}" ]]; then
    # A throwaway SMTP server on the stack's network (it accepts any user and password).
    cat >>"$tmp/$p.override.yml" <<EOF
  mailpit:
    image: $MAILPIT
    environment:
      MP_SMTP_AUTH_ACCEPT_ANY: "true"
      MP_SMTP_AUTH_ALLOW_INSECURE: "true"
    healthcheck:
      test: ["CMD", "/mailpit", "readyz"]
      interval: 2s
      timeout: 5s
      retries: 30
  init-bootstrap:
    depends_on:
      mailpit:
        condition: service_healthy
EOF
  fi
}

# The base file's pinned Postgres image: a shell to make root-owned test files with.
FILE_HELPER="$(grep -m1 -oE 'postgres:[^ ]+@sha256:[0-9a-f]{64}' "$dir/docker-compose.yml")"
# The workbench's Mailpit, pinned.
MAILPIT="axllent/mailpit:v1.31.3@sha256:ed9b00c609e77e99c79b93f1178255ebc271868920f2c69a8d166bd5634ed10d"

case "${SELFHOST_E2E_IMAGES:-}" in
  local) COMPOSE_FILES=(-f "$dir/docker-compose.yml") ;;
  "") COMPOSE_FILES=(-f "$dir/docker-compose.yml" -f "$dir/docker-compose.build.yml") ;;
  *) die "SELFHOST_E2E_IMAGES must be unset or 'local' (got '$SELFHOST_E2E_IMAGES')" ;;
esac
# Per-image tags: an override naming every app service's image.
if [[ "${SELFHOST_E2E_IMAGES:-}" == local ]] && compgen -v CHELABASE_IMAGE_TAG_ >/dev/null; then
  echo "services:" >"$tmp/images.yml"
  svc=""
  while IFS= read -r line; do
    [[ "$line" =~ ^\ \ ([a-z0-9-]+):$ ]] && svc="${BASH_REMATCH[1]}"
    [[ "$line" =~ image:\ \$\{CHELABASE_IMAGE_REPO:-([^}]*)\}/([a-z]+):\$\{CHELABASE_IMAGE_TAG:-([^}]*)\}$ ]] || continue
    img="${BASH_REMATCH[2]}"
    var="CHELABASE_IMAGE_TAG_${img^^}"
    if [[ -n "${!var:-}${CHELABASE_IMAGE_TAG:-}" ]]; then
      ref="${CHELABASE_IMAGE_REPO:-chelabase}/$img:${!var:-$CHELABASE_IMAGE_TAG}"
    else
      ref="${BASH_REMATCH[1]}/$img:${BASH_REMATCH[3]}"
      docker image inspect "$ref" >/dev/null 2>&1 || docker pull -q "$ref" >/dev/null
    fi
    printf '  %s:\n    image: %s\n' "$svc" "$ref" >>"$tmp/images.yml"
    say "$svc: $ref"
  done <"$dir/docker-compose.yml"
  COMPOSE_FILES+=(-f "$tmp/images.yml")
fi
UP_ARGS=(up -d --wait --wait-timeout 900)
[[ -n "${SELFHOST_E2E_IMAGES:-}" ]] && UP_ARGS+=(--pull never) || UP_ARGS+=(--build)

# compose_up <project>: Compose's own output goes to $tmp/<project>.up; returns its exit code.
compose_up() {
  local p="$1"
  local files=("${COMPOSE_FILES[@]}" -f "$tmp/$p.override.yml")
  docker compose -p "$p" --project-directory "$dir" "${files[@]}" --env-file "$tmp/$p.env" "${UP_ARGS[@]}" >"$tmp/$p.up" 2>&1
}

# From here every `compose` call includes the project's override (the seeds bind).
compose() {
  local project="$1"
  shift
  local files=("${COMPOSE_FILES[@]}")
  [[ ! -f "$tmp/$project.override.yml" ]] || files+=(-f "$tmp/$project.override.yml")
  docker compose -p "$project" --project-directory "$dir" "${files[@]}" --env-file "$tmp/$project.env" "$@"
}

# vol_cat <file in the secrets volume>: its contents, read as the init user (10001).
vol_cat() {
  docker run --rm --user 10001:10001 --network none -v "${PROJECT}_secrets:/s:ro" --entrypoint cat "$HELPER" "/s/$1"
}

# ---- the sign-in client (Studio's login API, no browser) -----------------------------------------

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

# authorize → an authRequest id from Zitadel's redirect to Studio's /login.
authorize() {
  local verifier challenge loc
  verifier="$(openssl rand -hex 32)"
  challenge="$(printf %s "$verifier" | openssl dgst -sha256 -binary | b64url)"
  echo "$verifier" >"$tmp/verifier"
  loc="$(curl -sS --max-time 30 -o /dev/null -w '%{redirect_url}' -G "$AUTH/oauth/v2/authorize" \
    --data-urlencode "client_id=$CLIENT" --data-urlencode "redirect_uri=$STUDIO/callback" \
    --data-urlencode "response_type=code" \
    --data-urlencode "scope=openid profile email urn:zitadel:iam:user:resourceowner urn:zitadel:iam:org:project:id:${PROJECT_ID}:aud" \
    --data-urlencode "code_challenge=$challenge" --data-urlencode "code_challenge_method=S256")" ||
    die "authorize: no answer from $AUTH"
  [[ "$loc" == "$STUDIO/login?authRequest="* ]] || die "authorize redirected to '$loc'"
  echo "${loc#*authRequest=}"
}

# ui <path> <json>: Studio's login API through the gateway; body to stdout, status in $tmp/ui-status.
ui() {
  local out
  out="$(curl -sS --max-time 30 -b "$tmp/jar" -c "$tmp/jar" -H "Origin: $STUDIO" -H 'Content-Type: application/json' \
    -w '\n%{http_code}' -X POST "$API/auth/v1/$1" -d "$2")" || die "POST /auth/v1/$1: no answer"
  printf '%s' "${out##*$'\n'}" >"$tmp/ui-status"
  printf '%s' "${out%$'\n'*}"
}

sign_up() { # sign_up <email> [code]
  local body
  body="$(jq -nc --arg ar "$(authorize)" --arg e "$1" --arg p "$PASSWORD" --arg c "${2:-}" \
    '{authRequest: $ar, email: $e, givenName: "Self", familyName: "Host", password: $p} + (if $c == "" then {} else {setupCode: $c} end)')"
  ui sign-up "$body"
}

sign_in() { # sign_in <email> → an access token
  local out code token
  out="$(ui sign-in "$(jq -nc --arg ar "$(authorize)" --arg e "$1" --arg p "$PASSWORD" '{authRequest: $ar, email: $e, password: $p}')")"
  [[ "$(jq -r '.next // empty' <<<"$out")" == done ]] || die "sign-in as $1: $(cat "$tmp/ui-status") $out"
  code="$(jq -r .redirect <<<"$out" | sed -E 's/.*[?&]code=([^&]+).*/\1/')"
  token="$(curl -sS --max-time 30 -X POST "$AUTH/oauth/v2/token" -d grant_type=authorization_code -d "code=$code" \
    -d "redirect_uri=$STUDIO/callback" -d "client_id=$CLIENT" -d "code_verifier=$(cat "$tmp/verifier")" | jq -r .access_token)"
  [[ -n "$token" && "$token" != null ]] || die "no access token for $1"
  echo "$token"
}

# api <method> <path> [json]: the management API as the claimant; status to stdout, body in $tmp/body.
api() {
  local args=(-sS --max-time 60 -o "$tmp/body" -w '%{http_code}' -X "$1" "$API$2" -H "Authorization: Bearer $ADMIN_TOKEN")
  [[ -z "${3:-}" ]] || args+=(-H 'content-type: application/json' -d "$3")
  curl "${args[@]}" || echo 000
}

# ---- 1. render + up ------------------------------------------------------------------------------

step "render and start a fresh project"
PROJECT="chelabase-selfhost-e2e-$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')"
PORT="${SELFHOST_E2E_PORT:-$(free_port)}"
# SMTP as the install docs say: the password in a host file owned by uid 10001 at mode 0600 (made
# by a root container, as `sudo chown 10001` would), never in .env.
SMTP_PASSWORD="smtp-$(openssl rand -hex 12)"
docker run --rm --network none -v "$tmp:/w" --entrypoint sh "$FILE_HELPER" -c \
  'umask 077 && printf %s "$1" >/w/smtp.password && chown 10001:10001 /w/smtp.password && chmod 600 /w/smtp.password' sh "$SMTP_PASSWORD" ||
  die "writing the SMTP password file"
[[ "$(stat -c %u:%a "$tmp/smtp.password")" == 10001:600 ]] || die "the SMTP password file is $(stat -c %u:%a "$tmp/smtp.password")"
WITH_MAILPIT=1 write_env "$PROJECT" "$PORT" CHELABASE_SMTP_HOST=mailpit CHELABASE_SMTP_PORT=1025 \
  CHELABASE_SMTP_FROM=chelabase@example.com CHELABASE_SMTP_USER=chelabase "CHELABASE_SMTP_PASSWORD_PATH=$tmp/smtp.password"
PROXY_PORT="$(sed -n 's/^CHELABASE_PROXY_PORT=//p' "$tmp/$PROJECT.env")"
PROXY_HTTP_PORT="$(sed -n 's/^CHELABASE_PROXY_HTTP_PORT=//p' "$tmp/$PROJECT.env")"
STUDIO="http://studio.chelabase.localhost:$PORT"
API="http://api.chelabase.localhost:$PORT"
AUTH="http://auth.chelabase.localhost:$PORT"
PASSWORD="Selfhost-e2e-$(openssl rand -hex 6)A!"
EMAIL="admin-$(openssl rand -hex 3)@example.com"
TORN_DOWN=0
say "project $PROJECT on port $PORT ($([[ -n "${SELFHOST_E2E_IMAGES:-}" ]] && echo "local images" || echo "built from this checkout"))"

compose "$PROJECT" config --format json >"$tmp/config.json" 2>"$tmp/config.err" ||
  die "docker compose config: $(cat "$tmp/config.err")"
HELPER="$(jq -r '.services["metadata-db"].image' "$tmp/config.json")"
[[ -n "$HELPER" && "$HELPER" != null ]] || die "no metadata-db image in the rendered config"

if ! compose_up "$PROJECT"; then
  tail -n 40 "$tmp/$PROJECT.up" >&2
  die "docker compose up --wait failed"
fi
pass "up --wait finished"

# ---- 2. healthy ----------------------------------------------------------------------------------

step "every service is healthy or a one-shot that exited 0"
compose "$PROJECT" ps -a --format json >"$tmp/ps.json" || die "docker compose ps"
bad="$(jq -rs '[.[] | select(
    (.State == "running" and (.Health == "healthy" or .Health == "")) or (.State == "exited" and .ExitCode == 0) | not
  ) | "\(.Service)=\(.State)/\(.Health)/exit\(.ExitCode)"] | join(" ")' "$tmp/ps.json")"
[[ -z "$bad" ]] || die "services not healthy: $bad"
n_services="$(jq -s 'length' "$tmp/ps.json")"
for one_shot in init-secrets init-bootstrap; do
  jq -se --arg s "$one_shot" 'any(.[]; .Service == $s and .State == "exited" and .ExitCode == 0)' "$tmp/ps.json" >/dev/null ||
    die "$one_shot did not exit 0"
done
pass "$n_services containers healthy or done; init-secrets and init-bootstrap exited 0"

step "Zitadel's SMTP configuration is the install's and active"
# Zitadel's admin API with the bootstrap's PAT, from inside the orchestrator (the PAT goes to
# curl in its config on stdin, never its command line); Zitadel's instance is the auth host.
compose "$PROJECT" exec -T orchestrator sh -c '
  printf "header = \"Authorization: Bearer %s\"\n" "$(cat /run/chelabase/secrets/zitadel-pats/admin.pat)" |
    curl -sS --max-time 30 -K - -X POST -H "Host: $1" -H "content-type: application/json" -d "{}" \
      http://zitadel:8080/admin/v1/smtp/_search' sh "auth.chelabase.localhost:$PORT" >"$tmp/smtp.json" 2>&1 ||
  die "listing Zitadel's SMTP configurations: $(cat "$tmp/smtp.json")"
jq -e 'any(.result[]?; .host == "mailpit:1025" and .state == "SMTP_CONFIG_ACTIVE" and .senderAddress == "chelabase@example.com")' \
  "$tmp/smtp.json" >/dev/null || die "no active SMTP configuration for mailpit:1025: $(jq -c '[.result[]? | {host, state}]' "$tmp/smtp.json" 2>&1 || cat "$tmp/smtp.json")"
pass "SMTP mailpit:1025 is active (the password file was read at 10001:0600)"

# ---- 3. setup code -------------------------------------------------------------------------------

step "the setup code is in the init-secrets logs"
compose "$PROJECT" logs --no-color init-secrets >"$tmp/init.log" 2>&1 || die "logs init-secrets"
CODE="$(sed -nE 's/.*Setup code: ([A-Z0-9]{4}(-[A-Z0-9]{4}){4}).*/\1/p' "$tmp/init.log" | head -n1)"
[[ -n "$CODE" ]] || die "no 'Setup code: XXXX-XXXX-XXXX-XXXX-XXXX' line in the logs of init-secrets"
[[ "$(grep -c 'Setup code:' "$tmp/init.log")" == 1 ]] || die "the setup code is printed more than once"
cmd_code="$(compose "$PROJECT" run --rm --no-deps --user 10001:10001 --entrypoint chelabase-init init-secrets \
  setup-code --dir /var/lib/chelabase/secrets 2>"$tmp/cmd.err" | grep -oE '[A-Z0-9]{4}(-[A-Z0-9]{4}){4}' | head -n1)" ||
  die "the setup-code command failed: $(cat "$tmp/cmd.err")"
[[ "$cmd_code" == "$CODE" ]] || die "the setup-code command prints a different code than the logs"
pass "the setup code is in the logs once, and the documented command prints the same one"

step "the instance asks for a setup code"
curl -fsS --max-time 30 "$API/auth/v1/instance" >"$tmp/instance.json" || die "GET $API/auth/v1/instance"
[[ "$(jq -r .setupRequired "$tmp/instance.json")" == true ]] || die "/auth/v1/instance says: $(cat "$tmp/instance.json")"
[[ "$(curl -sS --max-time 30 -o /dev/null -w '%{http_code}' "$STUDIO/")" == 200 ]] || die "Studio does not answer 200 at $STUDIO/"
[[ "$(curl -sS --max-time 30 -o /dev/null -w '%{http_code}' "$API/_gateway/ready")" == 200 ]] || die "/_gateway/ready is not 200"
pass "setupRequired is true, Studio answers 200, the gateway is ready"

PROJECT_ID="$(vol_cat bootstrap/ids/project-id | tr -d '\r\n')"
CLIENT="$(vol_cat bootstrap/ids/studio-client-id | tr -d '\r\n')"
ORG_ID="$(vol_cat bootstrap/ids/org-id | tr -d '\r\n')"
[[ -n "$PROJECT_ID" && -n "$CLIENT" && -n "$ORG_ID" ]] || die "the bootstrap's ids are missing from the secrets volume"

# ---- 4. the code is required ---------------------------------------------------------------------

step "sign-up without the code, and with a wrong one, is refused"
: >"$tmp/jar"
want_no_code=403
[[ "${SELFHOST_E2E_SKIP_CODE:-}" != 1 ]] || want_no_code=200
for attempt in none wrong; do
  if [[ $attempt == none ]]; then out="$(sign_up "nocode-$(openssl rand -hex 3)@example.com")"; else
    out="$(sign_up "wrongcode-$(openssl rand -hex 3)@example.com" "AAAA-BBBB-CCCC-DDDD-EEEE")"
  fi
  status="$(cat "$tmp/ui-status")"
  [[ "$status" == "$want_no_code" ]] || die "sign-up with $attempt code: $status (wanted $want_no_code) $out"
  if [[ "$want_no_code" == 403 ]]; then
    [[ "$(jq -r '.error.code // empty' <<<"$out")" == setup_code_invalid ]] || die "sign-up with $attempt code: not setup_code_invalid: $out"
  fi
done
pass "no code and a wrong code both 403 setup_code_invalid"

# ---- 5. claim ------------------------------------------------------------------------------------

step "claim the instance with the code"
out="$(sign_up "$EMAIL" "$CODE")"
[[ "$(cat "$tmp/ui-status")" == 200 ]] || die "sign-up with the code: $(cat "$tmp/ui-status") $out"
ADMIN_TOKEN="$(sign_in "$EMAIL")"
status="$(api GET /v1/me)"
[[ "$status" == 200 ]] || die "GET /v1/me: $status $(cat "$tmp/body")"
jq -e --arg e "$EMAIL" '.member.tenant_admin == true and .instance_admin == true and .tenant_id != null and .email == $e' "$tmp/body" >/dev/null ||
  die "the claimant is not tenant admin of a bound tenant: $(jq -c '{email, member, instance_admin, tenant_id}' "$tmp/body")"
TENANT_ID="$(jq -r .tenant_id "$tmp/body")"
pass "the claimant signed in and is tenant admin (tenant $TENANT_ID)"

step "the claimed instance no longer takes the code, and sign-up is closed"
[[ "$(curl -fsS --max-time 30 "$API/auth/v1/instance" | jq -r .setupRequired)" == false ]] || die "setupRequired is still true after the claim"
# After the claim, sign-up follows the tenant's open sign-up, off by default (`open_signup
# DEFAULT false`): an admin invites everyone else. The used code changes nothing.
for code in "$CODE" ""; do
  out="$(sign_up "second-$(openssl rand -hex 3)@example.com" "$code")"
  status="$(cat "$tmp/ui-status")"
  [[ "$status" == 403 && "$(jq -r '.error.code // empty' <<<"$out")" == sign_up_closed ]] ||
    die "a second sign-up ${code:+with the used code }after the claim: $status $out (wanted 403 sign_up_closed)"
done
status="$(api GET /v1/members)"
[[ "$status" == 200 ]] || die "GET /v1/members: $status"
[[ "$(jq '[.[] | select(.tenant_admin == true)] | length' "$tmp/body")" == 1 ]] || die "expected exactly one tenant admin: $(cat "$tmp/body")"
pass "setupRequired is false; a second sign-up (with the used code or none) is 403 sign_up_closed"

# ---- 7. a project and a database, through the proxy ----------------------------------------------

step "create a project and a database through the API"
status="$(api POST /v1/projects '{"name":"e2e"}')"
[[ "$status" == 201 || "$status" == 200 ]] || die "POST /v1/projects: $status $(cat "$tmp/body")"
project_id="$(jq -r '.id // .project_id' "$tmp/body")"
[[ -n "$project_id" && "$project_id" != null ]] || die "POST /v1/projects returned no id: $(cat "$tmp/body")"
status="$(api POST /v1/databases "$(jq -nc --arg p "$project_id" '{project_id: $p, name: "e2e_db"}')")"
[[ "$status" == 201 || "$status" == 202 ]] || die "POST /v1/databases: $status $(cat "$tmp/body")"
db_id="$(jq -r '.database_id' "$tmp/body")"
[[ -n "$db_id" && "$db_id" != null ]] || die "POST /v1/databases returned no database_id: $(cat "$tmp/body")"
db_status=""
deadline=$((SECONDS + 240))
while ((SECONDS < deadline)); do
  [[ "$(api GET "/v1/databases/$db_id")" == 200 ]] || die "GET /v1/databases/$db_id: $(cat "$tmp/body")"
  db_status="$(jq -r .status "$tmp/body")"
  [[ "$db_status" != failed ]] || die "database $db_id failed: $(jq -r .error "$tmp/body")"
  [[ "$db_status" != ready ]] || break
  sleep 2
done
[[ "$db_status" == ready ]] || die "database $db_id is still '$db_status' after 240 s"
pass "project $project_id and database $db_id are ready"

step "connect to the database from the host, as the connection details say"
[[ "$(api GET "/v1/databases/$db_id/connection")" == 200 ]] || die "GET /connection: $(cat "$tmp/body")"
cp "$tmp/body" "$tmp/connection.json"
pg_host="$(jq -r .host "$tmp/connection.json")"
pg_port="$(jq -r .port "$tmp/connection.json")"
pg_uri="$(jq -r .uri "$tmp/connection.json")"
http_url="$(jq -r .http_url "$tmp/connection.json")"
ca_file="$(jq -r .ca_file "$tmp/connection.json")"
[[ "$pg_host" == ep-*.localtest.me && "$pg_port" == "$PROXY_PORT" ]] ||
  die "the connection details name $pg_host:$pg_port, not <ep>.localtest.me:$PROXY_PORT (the published proxy port)"
[[ "$pg_uri" == "postgresql://"*"@$pg_host:$PROXY_PORT/"*"?sslmode=verify-full" ]] || die "unexpected uri (host or port): ${pg_uri#*@}"
[[ "$http_url" == "https://$pg_host:$PROXY_HTTP_PORT/sql" ]] || die "unexpected http_url '$http_url'"
[[ "$ca_file" == proxy-ca.crt ]] || die "ca_file is '$ca_file' (wanted proxy-ca.crt, the docs' file name)"
# The CA as the install docs fetch it (the project flag added), then psql on the host itself:
# The proxy is published on 127.0.0.1 only, so the connection pins that address (hostaddr) while
# verify-full still checks the certificate against the CA and the host name <ep>.localtest.me, which
# may resolve to ::1 first.
compose "$PROJECT" cp proxy:/tls/ca.crt "$tmp/$ca_file" >/dev/null 2>"$tmp/cp.err" || die "docker compose cp of the CA: $(cat "$tmp/cp.err")"
openssl x509 -in "$tmp/$ca_file" -noout -subject | grep -q 'Chelabase local CA' || die "the fetched CA is not the install's"
psql_host() { PGCONNECT_TIMEOUT=10 psql "$pg_uri&sslrootcert=$tmp/$ca_file&hostaddr=127.0.0.1" -v ON_ERROR_STOP=1 -qtAX "$@"; }
deadline=$((SECONDS + 120))
one=""
while ((SECONDS < deadline)); do
  if one="$(psql_host -c 'select 1' 2>"$tmp/psql.err")"; then break; fi
  sleep 3
done
[[ "$one" == 1 ]] || die "psql on the host never reached the database through the published proxy: $(cat "$tmp/psql.err")"
[[ "$(psql_host -c 'create table e2e(v int); insert into e2e values (42); select v from e2e')" == 42 ]] || die "write and read through the proxy"
pg_user="$(jq -r .user "$tmp/connection.json")"
pg_pass="$(jq -r .password "$tmp/connection.json")"
pg_name="$(jq -r .database "$tmp/connection.json")"
out="$(curl -sS --max-time 30 --resolve "$pg_host:$PROXY_HTTP_PORT:127.0.0.1" --cacert "$tmp/$ca_file" -w '\n%{http_code}' -X POST "$http_url" \
  -H "Neon-Connection-String: postgresql://$pg_user:$pg_pass@$pg_host/$pg_name" -H 'content-type: application/json' \
  -d '{"query":"select v from e2e","params":[]}')" || die "SQL over HTTP: no answer at $http_url"
[[ "${out##*$'\n'}" == 200 ]] && jq -e '.rows[0].v == 42' <<<"${out%$'\n'*}" >/dev/null ||
  die "SQL over HTTP at $http_url: ${out##*$'\n'} $(head -c 300 <<<"${out%$'\n'*}")"
pass "from the host: psql (verify-full, the CA from docker compose cp) created and read a row; SQL over HTTP read it"

# ---- 8. the Zitadel webhook ----------------------------------------------------------------------

step "a signed Zitadel user.deactivated event is answered 200"
signing_key="$(vol_cat bootstrap/actions-signing-key | tr -d '\r\n')"
[[ -n "$signing_key" ]] || die "no Actions signing key in the bootstrap outputs"
event_body="$(jq -nc --arg org "$ORG_ID" '{aggregateID: "999999999999", aggregateType: "user", resourceOwner: $org, instanceID: "i1",
  event_type: "user.deactivated", created_at: (now | todate), userID: "999999999999"}')"
post_event() { # post_event <signature header or ''>
  local args=(-sS --max-time 30 -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:8083/internal/zitadel/events
    -H 'content-type: application/json' -d "$event_body")
  [[ -z "$1" ]] || args+=(-H "zitadel-signature: $1")
  compose "$PROJECT" exec -T orchestrator curl "${args[@]}"
}
ts="$(date +%s)"
sig="t=$ts,v1=$(printf '%s.%s' "$ts" "$event_body" | openssl dgst -sha256 -hmac "$signing_key" -hex | sed 's/^.* //')"
status="$(post_event "$sig")" || die "could not post to the orchestrator"
[[ "$status" == 200 ]] || die "a correctly signed user.deactivated event: $status (wanted 200)"
status="$(post_event "t=$ts,v1=$(printf '0%.0s' $(seq 64))")" || die "could not post to the orchestrator"
[[ "$status" == 401 ]] || die "a wrongly signed event: $status (wanted 401)"
pass "the signed event is 200, a bad signature is 401"

# ---- 9. no secret in docker inspect --------------------------------------------------------------

step "no secret value appears in any container's docker inspect"
# Raw secret files only (a config file that embeds one is not a secret by itself): every
# non-empty line of 12+ characters that is not PEM framing, tagged with its file name.
docker run --rm --user 10001:10001 --network none -v "${PROJECT}_secrets:/s:ro" --entrypoint sh "$HELPER" -c '
  cd /s
  for f in $(find . -type f \( -name "*.password" -o -name "*.token" -o -name "*.jwt" -o -name masterkey \
      -o -name access-key -o -name secret-key -o -name setup.code -o -name secrets.key -o -name actions-signing-key \
      -o -name admin.pat -o -name ticket.key -o -name "*.pem" \) ! -name "*.pub.pem" ! -name "*.pub"); do
    while IFS= read -r line || [ -n "$line" ]; do
      case "$line" in "" | -----*) continue ;; esac
      [ "${#line}" -ge 12 ] && printf "%s\t%s\n" "$f" "$line"
    done < "$f"
  done' >"$tmp/secrets.tsv" || die "listing the secret files failed"
n_secrets="$(wc -l <"$tmp/secrets.tsv")"
((n_secrets >= 30)) || die "only $n_secrets secret values found in the volume (expected 30+): the scan would prove little"
# The setup code is a secret too, with and without its dashes; and the SMTP password.
printf 'setup/setup.code\t%s\n' "$CODE" "${CODE//-/}" >>"$tmp/secrets.tsv"
printf 'smtp.password\t%s\n' "$SMTP_PASSWORD" >>"$tmp/secrets.tsv"
containers=$(compose "$PROJECT" ps -aq)
# shellcheck disable=SC2086
docker inspect $containers >"$tmp/inspect.json" || die "docker inspect"
leaks=""
while IFS=$'\t' read -r file value; do
  if grep -qF -- "$value" "$tmp/inspect.json"; then leaks+=" $file"; fi
done <"$tmp/secrets.tsv"
[[ -z "$leaks" ]] || die "the value of these secret files appears in a docker inspect (not printed):$leaks"
pass "$n_secrets secret values from the volume are in no container's inspect ($(wc -w <<<"$containers") containers)"

# ---- 10. each service reads its secrets as its own uid -------------------------------------------

step "each service reads its mounted secrets as its own uid"
checked=0
for svc in $(jq -r '.services | to_entries[] | select(any(.value.volumes[]?; .source == "secrets")) | .key' "$tmp/config.json"); do
  cid="$(compose "$PROJECT" ps -aq "$svc")"
  [[ -n "$cid" ]] || die "no container for $svc"
  [[ "$(docker inspect -f '{{.State.Running}}' "$cid")" == true ]] || continue # a one-shot that is done
  pid="$(docker inspect -f '{{.State.Pid}}' "$cid")"
  # The main process's real ids (not the configured user: it may start as root and drop).
  uid="$(awk '/^Uid:/ {print $3}' "/proc/$pid/status")"
  gid="$(awk '/^Gid:/ {print $3}' "/proc/$pid/status")"
  groups_args=()
  for g in $(awk '/^Groups:/ {for (i = 2; i <= NF; i++) print $i}' "/proc/$pid/status"); do groups_args+=(--group-add "$g"); done
  mounts=()
  paths=""
  i=0
  while IFS=$'\t' read -r sub; do
    i=$((i + 1))
    mounts+=(--mount "type=volume,src=${PROJECT}_secrets,dst=/m/$i,volume-subpath=$sub,readonly")
    paths+=" /m/$i"
  done < <(jq -r --arg s "$svc" '.services[$s].volumes[] | select(.source == "secrets" and (.volume.subpath // "") != "") | .volume.subpath' "$tmp/config.json")
  [[ -n "$paths" ]] || continue
  # shellcheck disable=SC2016
  if ! docker run --rm --network none --user "$uid:$gid" "${groups_args[@]}" "${mounts[@]}" --entrypoint sh "$HELPER" -c '
      rc=0
      for p in "$@"; do
        if [ -d "$p" ]; then ls "$p" >/dev/null 2>&1 || { echo "cannot list $p" >&2; rc=1; }
        else head -c1 "$p" >/dev/null 2>&1 || { echo "cannot read $p" >&2; rc=1; }; fi
      done
      exit $rc' sh $paths 2>"$tmp/reads.err"; then
    die "$svc (uid $uid gid $gid) cannot read its mounted secrets: $(cat "$tmp/reads.err")"
  fi
  checked=$((checked + 1))
done
((checked >= 15)) || die "only $checked services with secrets were checked"
pass "$checked running services read all their mounted secrets as their own uid and groups"

step "Zitadel's PATs are 0600 and auth starts without a mode warning"
# Zitadel creates a PAT 0644 unless the file exists; init-secrets makes them 0600 first.
modes="$(docker run --rm --user 10001:10001 --network none -v "${PROJECT}_secrets:/s:ro" --entrypoint stat "$HELPER" \
  -c '%a:%u %n' /s/zitadel-pats/admin.pat /s/zitadel-pats/login-client.pat)" || die "stat of the PATs failed"
open_pats="$(grep -v '^600:10001 ' <<<"$modes" || true)"
[[ -z "$open_pats" ]] || die "a Zitadel PAT is not 0600 and the app user's: $(tr '\n' ' ' <<<"$open_pats")"
for f in admin.pat login-client.pat; do
  [[ -n "$(vol_cat "zitadel-pats/$f" | tr -d '[:space:]')" ]] || die "zitadel-pats/$f is empty: Zitadel did not write it"
done
if compose "$PROJECT" logs --no-color auth 2>&1 | grep -F 'readable by other users'; then
  die "auth warns that a secret it reads is readable by other users"
fi
pass "both PATs are 0600, owned by 10001 and written; auth logs no mode warning"

# ---- 11. a rerun changes nothing -----------------------------------------------------------------

snapshot() { # snapshot <name>: the secrets volume's files, the containers and the code line count
  docker run --rm --user 10001:10001 --network none -v "${PROJECT}_secrets:/s:ro" --entrypoint sh "$HELPER" -c \
    'cd /s && find . -type f | sort | while read -r f; do printf "%s %s %s\n" "$(sha256sum < "$f" | cut -d" " -f1)" "$(stat -c %a:%u:%g "$f")" "$f"; done' >"$tmp/$1.secrets" ||
    die "hashing the secrets volume"
  # shellcheck disable=SC2046
  docker inspect -f '{{.Name}} {{.Id}} {{.State.Status}} {{if eq .State.Status "running"}}{{.State.StartedAt}}{{end}}' $(compose "$PROJECT" ps -aq) | sort >"$tmp/$1.containers"
  compose "$PROJECT" logs --no-color init-secrets 2>&1 | grep -c 'Setup code:' >"$tmp/$1.code" || true
}
# Compose runs the one-shots (init-secrets, init-bootstrap, ...) again on every up: they must
# change nothing. So the snapshot keeps a one-shot's id and state only, a running service's start time too.
step "a second up changes nothing"
snapshot before
if ! compose_up "$PROJECT"; then tail -n 40 "$tmp/$PROJECT.up" >&2; die "the second up --wait failed"; fi
snapshot after
diff -u "$tmp/before.secrets" "$tmp/after.secrets" >"$tmp/secrets.diff" || die "a rerun of up changed the secrets volume (hashes, modes or files): $(sed 's/^\([-+]\)[0-9a-f]\{64\}/\1<hash>/' "$tmp/secrets.diff" | head -20)"
diff -u "$tmp/before.containers" "$tmp/after.containers" >"$tmp/containers.diff" || die "a rerun of up recreated or restarted containers: $(head -20 "$tmp/containers.diff")"
diff -u "$tmp/before.code" "$tmp/after.code" >/dev/null || die "a rerun of up printed the setup code again"
[[ "$(api GET /v1/me)" == 200 ]] || die "the claimant lost access after the rerun: $(cat "$tmp/body")"
pass "no secret file, container or setup-code line changed ($(wc -l <"$tmp/after.secrets") secret files)"

# ---- 12. teardown --------------------------------------------------------------------------------

step "the documented uninstall leaves nothing behind"
# The database's compute (the worker's container, not Compose's) is still running: the documented
# commands alone must remove it, its networks and the volumes. No fallback runs before the check.
[[ -n "$(docker ps -q --filter "label=chelabase.dev/stack=$PROJECT")" ]] || die "no compute is running before the uninstall: the check would prove little"
if [[ "${SELFHOST_E2E_KEEP:-}" == 1 ]]; then
  say "SELFHOST_E2E_KEEP=1: skipping the teardown check"
else
  uninstall "$PROJECT" >"$tmp/uninstall.log" 2>&1 || { tail -n 20 "$tmp/uninstall.log" >&2; die "the documented uninstall failed"; }
  nothing_left "$PROJECT"
  TORN_DOWN=1
  OLD_PROJECT="$PROJECT"
  PROJECT=""
  pass "$OLD_PROJECT: no containers, volumes or networks left"
fi

# ---- 13. a failing install -----------------------------------------------------------------------

step "an SMTP host with no sender fails up --wait at init-bootstrap"
FAIL_PROJECT="chelabase-selfhost-e2e-$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')"
write_env "$FAIL_PROJECT" "$(free_port)" "CHELABASE_SMTP_HOST=smtp.example.com"
UP_ARGS=(up -d --wait --wait-timeout 300)
[[ -n "${SELFHOST_E2E_IMAGES:-}" ]] && UP_ARGS+=(--pull never) || UP_ARGS+=(--build)
if compose_up "$FAIL_PROJECT"; then die "up --wait succeeded with CHELABASE_SMTP_HOST set and no CHELABASE_SMTP_FROM"; fi
grep -q 'init-bootstrap' "$tmp/$FAIL_PROJECT.up" || { tail -n 20 "$tmp/$FAIL_PROJECT.up" >&2; die "the failure does not name init-bootstrap"; }
exit_code="$(compose "$FAIL_PROJECT" ps -a --format json | jq -rs '.[] | select(.Service == "init-bootstrap") | .ExitCode')"
[[ "$exit_code" == 1 ]] || die "init-bootstrap exit code is '$exit_code' (wanted 1)"
pass "up --wait failed naming init-bootstrap (exit 1): $(grep -m1 -E "init-bootstrap.*(exit|didn)" "$tmp/$FAIL_PROJECT.up" || echo "see log")"
uninstall "$FAIL_PROJECT" >"$tmp/uninstall-fail.log" 2>&1 || { tail -n 20 "$tmp/uninstall-fail.log" >&2; die "the documented uninstall failed"; }
nothing_left "$FAIL_PROJECT"
pass "$FAIL_PROJECT: no containers, volumes or networks left"
FAIL_PROJECT=""
