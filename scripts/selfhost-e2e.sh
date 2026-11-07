#!/usr/bin/env bash
# End-to-end test of the self-hosted install (epic selfhost-install, m9): what a self-hoster does,
# on a throwaway Compose project of its own (unique name, free port and control subnet, its own
# .env). It never touches another project's containers and needs no workbench. Run it with
# `just selfhost-e2e`.
#
#   1. render + up      a fresh `docker compose up --wait` from infra/compose
#   2. healthy          every service is healthy or a one-shot that exited 0 (SMTP set: a
#                      throwaway Mailpit, the password file at the documented owner and mode)
#   3. setup code       the code is in `logs init-secrets` and the documented command prints it
#   4. code required    sign-up without the code, and with a wrong one, is 403 setup_code_invalid
#   5. claim            the Acme example (docker-compose.example.yml's `example-seed`) claims the
#                      instance with the code through sign-up, invites and admits every demo person
#                      and seeds Acme; Ada signs in (the session cookie, then /auth/v1/token) and
#                      GET /v1/me says tenant admin and instance admin; a second seed run skips
#   6. claimed          the code no longer opens the instance, and sign-up is closed (403
#                      sign_up_closed: open sign-up is off by default)
#   7. data             create a project and a database through the API; the connection details
#                      name the published proxy (<ep>.localtest.me, CHELABASE_PROXY_PORT); with the
#                      CA fetched as the docs say, psql on the host (verify-full) creates and reads
#                      a row, and SQL over HTTP on the host answers
#   7b. storage         the storage service through the gateway (#152): with bucket and object
#                      policies made as the owner, an app's data key creates a bucket (limit 1000
#                      bytes), uploads, downloads, signs a URL that downloads without a key and
#                      resolves refs (a missing one to null); 2000 bytes get 413; a Studio
#                      console upload through the orchestrator is readable by the app; a webhook
#                      to a private host off the allowlist is refused, one to the allowlisted
#                      receiver (`hooks`, over http: CHELABASE_WEBHOOKS_ALLOW_*) gets the upload's
#                      object.created event, and its Chelabase-Signature verifies with the secret
#   8. email            an invite by email reaches Mailpit through the identity service's SMTP
#                      (the password file read at 10001:0600), with its code
#   9. no secret leaks  no secret file value appears in any container's `docker inspect`
#  10. secret reads     every running service reads its mounted secrets as its own uid and groups
#  11. rerun            a second `up --wait` changes no secret, container or log line of the code
#  11b. upgrade        an install from before object storage (no storage container, NATS user or
#                      password): a recreated init-secrets adds only storage's password and NATS
#                      user, Compose restarts NATS, and storage logs in and delivers an event
#  12. uninstall        the documented uninstall (computes still running) leaves no container,
#                      volume or network
#  13. failing install  in a second project, an SMTP host with no sender fails `up --wait`: the
#                      identity service refuses to start and names the missing setting
#
# Images: by default `-f docker-compose.build.yml` builds them from this checkout (the cache is
# shared with `just images`). SELFHOST_E2E_IMAGES=local runs the base file on the images
# `just images` already built (CHELABASE_IMAGE_REPO / CHELABASE_IMAGE_TAG, default chelabase:latest;
# nothing is pulled for them). For a mixed release set, CHELABASE_IMAGE_TAG_<IMAGE> (e.g.
# CHELABASE_IMAGE_TAG_STUDIO) tags one image; once any is set, the images with neither it nor
# CHELABASE_IMAGE_TAG stay at their compose pins (pulled by digest when missing).
# The default (build) mode runs storage from the all-in-one server image, like the other apps, so
# the storage release image's read-only start is checked by scripts/tests/images-smoke.sh.
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
ADMIN_JAR=""

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
  for svc in init-secrets init-bootstrap identity orchestrator auth gateway storage worker example-seed; do
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
  identity:
    depends_on:
      mailpit:
        condition: service_healthy
EOF
  fi
  if [[ -n "${WITH_HOOKS:-}" ]]; then
    # A webhook receiver on the stack's network (the identity image's node): one JSON line per
    # request on stdout, the Chelabase-Signature header and the body (base64, byte for byte).
    local identity_image
    identity_image="$(compose "$p" config --format json | jq -r '.services.identity.image')"
    cat >>"$tmp/$p.override.yml" <<EOF
  hooks:
    image: $identity_image
    pull_policy: never
    read_only: true
    command:
      - node
      - -e
      - >-
        require('http').createServer((q, r) => { const c = []; q.on('data', d => c.push(d));
        q.on('end', () => { console.log(JSON.stringify({sig: q.headers['chelabase-signature'] || '',
        body: Buffer.concat(c).toString('base64')})); r.writeHead(204); r.end(); }); }).listen(8080)
    depends_on:
      identity:
        condition: service_started
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
    elif [[ "${BASH_REMATCH[3]}" == unpublished ]]; then
      # No pin to fall back to yet: the image `just images` built.
      ref="${CHELABASE_IMAGE_REPO:-chelabase}/$img:latest"
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

# ui <path> <json> [jar]: Studio's login API through the gateway, as Studio sends it (Origin,
# X-Chelabase-Studio); body to stdout, status in $tmp/ui-status. The jar keeps auth's cookies.
ui() {
  local out jar="${3:-$tmp/jar}"
  out="$(curl -sS --max-time 30 -b "$jar" -c "$jar" -H "Origin: $STUDIO" -H 'X-Chelabase-Studio: 1' \
    -H 'Content-Type: application/json' -w '\n%{http_code}' -X POST "$API/auth/v1/$1" -d "$2")" || die "POST /auth/v1/$1: no answer"
  printf '%s' "${out##*$'\n'}" >"$tmp/ui-status"
  printf '%s' "${out%$'\n'*}"
}

sign_up() { # sign_up <email> [code]
  local body
  body="$(jq -nc --arg e "$1" --arg p "$PASSWORD" --arg c "${2:-}" \
    '{email: $e, givenName: "Self", familyName: "Host", password: $p} + (if $c == "" then {} else {setupCode: $c} end)')"
  ui sign-up "$body" "$tmp/jar"
}

# sign_in <email> <password> <jar>: a sign-in that must answer `studio`, its session in the jar.
sign_in() {
  local out
  out="$(ui sign-in "$(jq -nc --arg e "$1" --arg p "$2" '{email: $e, password: $p}')" "$3")"
  [[ "$(cat "$tmp/ui-status")" == 200 && "$(jq -r '.next // empty' <<<"$out")" == studio ]] ||
    die "sign-in as $1: $(cat "$tmp/ui-status") $out"
}

# session_token <jar> → an access token minted from the jar's session (POST /auth/v1/token).
session_token() {
  local out token
  out="$(curl -sS --max-time 30 -b "$1" -H "Origin: $STUDIO" -H 'X-Chelabase-Studio: 1' -X POST "$API/auth/v1/token")" ||
    die "POST /auth/v1/token: no answer"
  token="$(jq -r '.access_token // empty' <<<"$out" 2>/dev/null)"
  [[ -n "$token" ]] || die "POST /auth/v1/token: $out"
  echo "$token"
}

# api <method> <path> [json]: the management API as the claimant (a fresh token from the claimant's
# session each call: tokens are short-lived); status to stdout, body in $tmp/body.
api() {
  local args=(-sS --max-time 60 -o "$tmp/body" -w '%{http_code}' -X "$1" "$API$2")
  [[ -z "${3:-}" ]] || args+=(-H 'content-type: application/json' -d "$3")
  ADMIN_TOKEN="$(session_token "$ADMIN_JAR")"
  printf 'Authorization: Bearer %s\n' "$ADMIN_TOKEN" | curl "${args[@]}" -H @- || echo 000
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
WITH_MAILPIT=1 WITH_HOOKS=1 write_env "$PROJECT" "$PORT" CHELABASE_SMTP_HOST=mailpit CHELABASE_SMTP_PORT=1025 CHELABASE_SMTP_TLS=none \
  CHELABASE_SMTP_FROM=chelabase@example.com CHELABASE_SMTP_USER=chelabase "CHELABASE_SMTP_PASSWORD_PATH=$tmp/smtp.password" \
  CHELABASE_WEBHOOKS_ALLOW_HOSTS=hooks CHELABASE_WEBHOOKS_ALLOW_HTTP=true
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

# ---- 5. claim: the Acme example -----------------------------------------------------------------

step "the Acme example claims the instance with the code and seeds it"
# The example overlay as the docs run it, on the image init-secrets runs (built from this
# checkout, or the local release image): the seed is `chelabase-init example`.
INIT_IMAGE="$(jq -r '.services["init-secrets"].image' "$tmp/config.json")"
[[ -n "$INIT_IMAGE" && "$INIT_IMAGE" != null ]] || die "no init-secrets image in the rendered config"
printf 'services:\n  example-seed:\n    image: %s\n    pull_policy: never\n' "$INIT_IMAGE" >"$tmp/example-image.yml"
seed() { # seed <log file>: one run of the example seed
  docker compose -p "$PROJECT" --project-directory "$dir" "${COMPOSE_FILES[@]}" -f "$tmp/$PROJECT.override.yml" \
    -f "$dir/docker-compose.example.yml" -f "$tmp/example-image.yml" --env-file "$tmp/$PROJECT.env" \
    run --rm --no-deps example-seed >"$1" 2>&1
}
seed "$tmp/seed.log" || { tail -n 30 "$tmp/seed.log" >&2; die "the example seed failed"; }
grep -q 'example: acme: .* writes\. Acme/' "$tmp/seed.log" || { tail -n 30 "$tmp/seed.log" >&2; die "the seed didn't finish Acme"; }
grep -qE 'code=|[?&]t=|Bearer|eyJ' "$tmp/seed.log" && die "the seed log carries a code, a token or an invite link"
grep -qF "$CODE" "$tmp/seed.log" && die "the seed log prints the setup code"
DEMO_PASSWORD="$(sed -nE 's/.*these accounts all sign in with the password (.+)$/\1/p' "$tmp/seed.log" | head -n1)"
[[ -n "$DEMO_PASSWORD" ]] || die "the seed log names no demo password"
EMAIL="ada@acme.example"
ADMIN_JAR="$tmp/ada.jar"
sign_in "$EMAIL" "$DEMO_PASSWORD" "$ADMIN_JAR"
status="$(api GET /v1/me)"
[[ "$status" == 200 ]] || die "GET /v1/me: $status $(cat "$tmp/body")"
jq -e --arg e "$EMAIL" '.member.tenant_admin == true and .instance_admin == true and .tenant_id != null and .email == $e' "$tmp/body" >/dev/null ||
  die "Ada is not tenant admin of a bound tenant: $(jq -c '{email, member, instance_admin, tenant_id}' "$tmp/body")"
TENANT_ID="$(jq -r .tenant_id "$tmp/body")"
# Every demo person (crates/chelabase-init/src/example/people.rs) is an active member, and Acme exists.
want_people="$(grep -cE '^\s+email: "[a-z]+@acme\.example",' "$root/crates/chelabase-init/src/example/people.rs")"
status="$(api GET /v1/members)"
[[ "$status" == 200 ]] || die "GET /v1/members: $status"
active="$(jq '[.[] | select((.email // "") | endswith("@acme.example")) | select(.status == "active")] | length' "$tmp/body")"
[[ "$active" == "$want_people" ]] || die "$active of $want_people demo people are active members"
status="$(api GET /v1/projects)"
[[ "$status" == 200 ]] && jq -e 'any((if type == "array" then . else .projects end)[]; .name == "Acme")' "$tmp/body" >/dev/null ||
  die "no project Acme: $status $(head -c 300 "$tmp/body")"
# Bea, a demo person the seed admitted through an invite, signs in with the demo password.
sign_in "bea@acme.example" "$DEMO_PASSWORD" "$tmp/bea.jar"
session_token "$tmp/bea.jar" >/dev/null
seed "$tmp/seed2.log" || { tail -n 30 "$tmp/seed2.log" >&2; die "the second example seed failed"; }
grep -q 'already complete on this instance: skipped, 0 writes' "$tmp/seed2.log" ||
  { tail -n 20 "$tmp/seed2.log" >&2; die "the second seed run didn't skip"; }
pass "the example claimed the instance as $EMAIL (tenant $TENANT_ID), admitted $active demo people and seeded Acme; a second run skipped with 0 writes"

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
jq -e --arg e "$EMAIL" 'any(.[]; .email == $e and .tenant_admin == true)' "$tmp/body" >/dev/null || die "Ada isn't a tenant admin in the member list"
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

# ---- 7b. object storage --------------------------------------------------------------------------

step "object storage through the gateway: an app's data key, a console upload and a webhook"
# Policies as the database owner: `authenticated` may see and create buckets and objects.
psql_host -c "
  select storage.create_policy('b_select', 'buckets', 'SELECT', array['authenticated']::name[], 'true', null);
  select storage.create_policy('b_insert', 'buckets', 'INSERT', array['authenticated']::name[], null, 'true');
  select storage.create_policy('o_select', 'objects', 'SELECT', array['authenticated']::name[], 'true', null);
  select storage.create_policy('o_insert', 'objects', 'INSERT', array['authenticated']::name[], null, 'true');" >/dev/null ||
  die "creating the storage policies as the owner"
[[ "$(api GET "/v1/databases/$db_id/branches")" == 200 ]] || die "GET branches: $(cat "$tmp/body")"
branch_id="$(jq -r '[.[] | select(.parent_branch_id == null)][0].id // empty' "$tmp/body")"
[[ -n "$branch_id" ]] || die "no main branch in $(cat "$tmp/body")"
status="$(api POST "/v1/databases/$db_id/branches/$branch_id/keys" '{"name":"e2e-storage","role":"authenticated"}')"
[[ "$status" == 201 ]] || die "POST keys: $status $(cat "$tmp/body")"
DATA_KEY="$(jq -r .key "$tmp/body")"
# st <method> <path> [curl args…]: the storage API with the data key; status to stdout, body in $tmp/sbody.
st() {
  local m="$1" p="$2"
  shift 2
  printf 'Authorization: Bearer %s\n' "$DATA_KEY" |
    curl -sS --max-time 60 -o "$tmp/sbody" -w '%{http_code}' -X "$m" "$API$p" -H @- "$@" || echo 000
}
# The webhook first (events from its registration on are delivered): a private host off the
# allowlist is refused; the allowlisted receiver is taken.
status="$(api POST "/v1/databases/$db_id/storage/webhooks" '{"url":"http://metadata-db:5432/x","events":["object.created"]}')"
[[ "$status" == 4* ]] || die "a webhook to metadata-db (not allowlisted) was accepted: $status $(cat "$tmp/body")"
status="$(api POST "/v1/databases/$db_id/storage/webhooks" '{"url":"http://hooks:8080/e2e","events":["object.created"],"bucket":"files"}')"
[[ "$status" == 201 ]] || die "POST storage/webhooks: $status $(cat "$tmp/body")"
HOOK_SECRET="$(jq -r .secret "$tmp/body")"
[[ "$HOOK_SECRET" == whsec_* ]] || die "the webhook answer has no secret"
status="$(st POST /storage/v1/bucket -H 'content-type: application/json' -d '{"name":"files","file_size_limit":1000}')"
[[ "$status" == 201 ]] || die "create bucket files: $status $(cat "$tmp/sbody")"
status="$(st POST /storage/v1/object/files/hello.txt -H 'content-type: text/plain' --data-binary 'hello storage')"
[[ "$status" == 200 ]] || die "upload hello.txt: $status $(cat "$tmp/sbody")"
status="$(st GET /storage/v1/object/files/hello.txt)"
[[ "$status" == 200 && "$(cat "$tmp/sbody")" == 'hello storage' ]] || die "download hello.txt: $status $(head -c 300 "$tmp/sbody")"
head -c 2000 /dev/zero >"$tmp/big.bin"
status="$(st POST /storage/v1/object/files/big.bin -H 'content-type: application/octet-stream' --data-binary @"$tmp/big.bin")"
[[ "$status" == 413 ]] || die "2000 bytes into a bucket of 1000: $status (wanted 413) $(head -c 300 "$tmp/sbody")"
status="$(st POST /storage/v1/object/sign/files/hello.txt -H 'content-type: application/json' -d '{"expires_in":60}')"
signed="$(jq -r '.signed_url // empty' "$tmp/sbody" 2>/dev/null || true)"
[[ "$status" == 200 && "$signed" == "/storage/v1/object/sign/files/hello.txt?token="* ]] || die "sign hello.txt: $status $(cat "$tmp/sbody")"
[[ "$(curl -sS --max-time 30 "$API$signed")" == 'hello storage' ]] || die "the signed URL doesn't download hello.txt without a key"
status="$(st POST /storage/v1/object/resolve -H 'content-type: application/json' \
  -d '{"refs":[{"bucket":"files","name":"hello.txt"},{"bucket":"files","name":"nope.txt"}]}')"
[[ "$status" == 200 ]] && jq -e '(.urls | length) == 2 and (.urls[0] | type) == "string" and .urls[1] == null' "$tmp/sbody" >/dev/null ||
  die "resolve refs: $status $(cat "$tmp/sbody")"
[[ "$(curl -sS --max-time 30 "$API$(jq -r '.urls[0]' "$tmp/sbody")")" == 'hello storage' ]] || die "the resolved URL doesn't download hello.txt"
# Studio's console upload (owner mode), through the orchestrator's storage routes to storage's
# internal listener; the app reads it.
status="$(printf 'Authorization: Bearer %s\n' "$(session_token "$ADMIN_JAR")" |
  curl -sS --max-time 60 -o "$tmp/body" -w '%{http_code}' -X POST -H @- -H 'content-type: application/octet-stream' \
    --data-binary 'from the console' "$API/v1/databases/$db_id/branches/$branch_id/storage/object/files/console.txt" || echo 000)"
[[ "$status" == 200 ]] || die "console upload through the orchestrator: $status $(cat "$tmp/body")"
status="$(st GET /storage/v1/object/files/console.txt)"
[[ "$status" == 200 && "$(cat "$tmp/sbody")" == 'from the console' ]] || die "the app reading the console upload: $status $(head -c 300 "$tmp/sbody")"
# hook_event <name>: waits up to 90 s for the receiver to log files/<name>'s object.created
# event; sets $event to its line ({"sig": …, "body": base64}).
hook_event() {
  local name="$1" line deadline=$((SECONDS + 90))
  event=""
  while ((SECONDS < deadline)); do
    while IFS= read -r line; do
      [[ "$line" == '{"sig":"t='* ]] || continue
      if jq -r .body <<<"$line" | base64 -d 2>/dev/null |
        jq -e --arg n "$name" '.type == "object.created" and .bucket == "files" and .name == $n' >/dev/null 2>&1; then
        event="$line"
        return
      fi
    done < <(compose "$PROJECT" logs --no-color --no-log-prefix hooks 2>/dev/null)
    sleep 2
  done
  die "no object.created event for $name at the receiver after 90 s: $(compose "$PROJECT" logs --no-color --tail 5 hooks 2>&1 | cut -c1-300)"
}
# The receiver got hello.txt's object.created event, signed with the endpoint's secret.
hook_event hello.txt
sig="$(jq -r .sig <<<"$event")"
t="${sig#t=}"
t="${t%%,*}"
v1="${sig#*v1=}"
v1="${v1%%,*}"
jq -r .body <<<"$event" | base64 -d >"$tmp/event.bin"
want="$({ printf '%s.' "$t"; cat "$tmp/event.bin"; } | openssl dgst -sha256 -hmac "$HOOK_SECRET" -r | cut -d' ' -f1)"
[[ -n "$v1" && "$v1" == "$want" ]] || die "the event's Chelabase-Signature doesn't verify with the webhook's secret"
pass "storage: bucket, upload, download, 413 over the limit, a signed URL, refs resolved, a console upload, and a signed object.created webhook delivered"

# ---- 8. email through the identity service's SMTP ---------------------------------------------

step "an invite by email reaches Mailpit with its code"
status="$(api GET /v1/invites/delivery)"
[[ "$status" == 200 && "$(jq -c . "$tmp/body")" == '{"email_available":true,"reason":null}' ]] ||
  die "invites can't be emailed: $status $(cat "$tmp/body")"
INVITEE="invitee-$(openssl rand -hex 3)@example.com"
status="$(api POST /v1/invites "$(jq -nc --arg e "$INVITEE" '{email: $e, grants: [], delivery: "email"}')")"
[[ "$status" == 201 ]] || die "POST /v1/invites (email): $status $(cat "$tmp/body")"
# Mailpit isn't published: its API is read from inside the orchestrator's container.
mail=""
deadline=$((SECONDS + 30))
while ((SECONDS < deadline)); do
  mail="$(compose "$PROJECT" exec -T orchestrator curl -sS --max-time 10 -G --data-urlencode "query=to:$INVITEE" \
    http://mailpit:8025/api/v1/search 2>/dev/null |
    jq -r '.messages[0].ID // empty' 2>/dev/null || true)"
  [[ -z "$mail" ]] || break
  sleep 1
done
[[ -n "$mail" ]] || die "no invite mail to $INVITEE in Mailpit after 30 s"
compose "$PROJECT" exec -T orchestrator curl -sS --max-time 10 "http://mailpit:8025/api/v1/message/$mail" | jq -r .Text | tr -d '\r' >"$tmp/invite.txt" ||
  die "reading the invite mail"
grep -qE '^[[:space:]]+[0-9]{6}[[:space:]]*$' "$tmp/invite.txt" || die "the invite mail carries no six-digit code"
grep -qF "$STUDIO/accept-invite?user=" "$tmp/invite.txt" || die "the invite mail has no accept link to $STUDIO"
pass "the invite mail reached Mailpit (SMTP with the password file at 10001:0600), with a code and the accept link"

# ---- 9. no secret in docker inspect --------------------------------------------------------------

step "no secret value appears in any container's docker inspect"
# Raw secret files only (a config file that embeds one is not a secret by itself): every
# non-empty line of 12+ characters that is not PEM framing, tagged with its file name.
docker run --rm --user 10001:10001 --network none -v "${PROJECT}_secrets:/s:ro" --entrypoint sh "$HELPER" -c '
  cd /s
  for f in $(find . -type f \( -name "*.password" -o -name "*.token" -o -name "*.jwt" -o -name "*.key" \
      -o -name access-key -o -name secret-key -o -name setup.code -o -name secret -o -name "*.pem" \) \
      ! -name "*.pub.pem" ! -name "*.pub" ! -name ca.crt); do
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

step "auth starts without a mode warning"
if compose "$PROJECT" logs --no-color auth 2>&1 | grep -F 'readable by other users'; then
  die "auth warns that a secret it reads is readable by other users"
fi
pass "auth logs no mode warning"

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

# ---- 11b. an upgrade from before object storage --------------------------------------------------

# An install from before object storage has no storage container, no storage NATS user and no
# nats/storage.password, and its NATS runs on that auth.conf. The upgrade's new init image (here:
# a label, which changes init-secrets' config the same way) makes the password and re-renders
# auth.conf; Compose must restart NATS (depends_on restart: true) or storage can't log in.
step "an upgrade from before object storage restarts NATS and storage logs in"
compose "$PROJECT" rm -sf storage >/dev/null 2>&1 || die "removing the storage container"
# shellcheck disable=SC2016
docker run --rm --user 10001:10001 --network none -v "${PROJECT}_secrets:/s" --entrypoint sh "$HELPER" -c '
  set -e
  rm /s/nats/storage.password
  awk "{ l[NR] = \$0 } END { for (i = 1; i <= NR; i++) {
      if (!skip && l[i] == \"    {\" && l[i + 1] == \"      user: \\\"storage\\\"\") skip = 1
      if (!skip) print l[i]; else if (l[i] == \"    }\") skip = 0 } }" /s/nats/auth.conf >/tmp/auth.conf
  ! grep -q "user: \"storage\"" /tmp/auth.conf
  [ "$(grep -c "user: " /tmp/auth.conf)" -ge 4 ]
  cat /tmp/auth.conf >/s/nats/auth.conf' || die "making the secrets volume a pre-storage install's"
compose "$PROJECT" restart nats >/dev/null 2>&1 || die "restarting NATS on the pre-storage users"
snapshot pre-upgrade
nats_started="$(docker inspect -f '{{.State.StartedAt}}' "$(compose "$PROJECT" ps -q nats)")"
init_id="$(compose "$PROJECT" ps -aq init-secrets)"
printf 'services:\n  init-secrets:\n    labels:\n      chelabase.e2e/upgrade: "1"\n' >"$tmp/upgrade.yml"
COMPOSE_FILES+=(-f "$tmp/upgrade.yml")
if ! compose_up "$PROJECT"; then tail -n 40 "$tmp/$PROJECT.up" >&2; die "the upgrade's up --wait failed"; fi
snapshot upgraded
[[ "$(compose "$PROJECT" ps -aq init-secrets)" != "$init_id" ]] || die "the upgrade didn't recreate init-secrets"
# Only storage's password is new and only auth.conf changed: nothing else is overwritten.
grep -v ' \./nats/auth\.conf$' "$tmp/pre-upgrade.secrets" >"$tmp/pre-upgrade.rest"
grep -v ' \./nats/auth\.conf$' "$tmp/upgraded.secrets" | grep -v ' \./nats/storage\.password$' >"$tmp/upgraded.rest"
diff -u "$tmp/pre-upgrade.rest" "$tmp/upgraded.rest" >"$tmp/upgrade.diff" ||
  die "the upgrade changed other secret files: $(sed 's/^\([-+]\)[0-9a-f]\{64\}/\1<hash>/' "$tmp/upgrade.diff" | head -20)"
grep -q ' \./nats/storage\.password$' "$tmp/upgraded.secrets" || die "the upgrade made no nats/storage.password"
cmp -s <(grep ' \./nats/auth\.conf$' "$tmp/pre-upgrade.secrets") <(grep ' \./nats/auth\.conf$' "$tmp/upgraded.secrets") &&
  die "the upgrade didn't re-render nats/auth.conf"
vol_cat nats/auth.conf | grep -q 'user: "storage"' || die "the re-rendered auth.conf has no storage user"
new_started="$(docker inspect -f '{{.State.StartedAt}}' "$(compose "$PROJECT" ps -q nats)")"
[[ "$new_started" != "$nats_started" ]] || die "NATS didn't restart after the upgraded init-secrets (still started at $nats_started)"
storage_id="$(compose "$PROJECT" ps -q storage)"
[[ -n "$storage_id" && "$(docker inspect -f '{{.State.Health.Status}}' "$storage_id")" == healthy ]] || die "storage isn't healthy after the upgrade"
# Storage logs in to NATS with its new user (its subscriptions start) …
connected=""
deadline=$((SECONDS + 60))
while ((SECONDS < deadline)); do
  if docker logs "$storage_id" 2>&1 | grep -qF 'listening for NATS cache events'; then connected=1; break; fi
  sleep 2
done
[[ -n "$connected" ]] || die "storage didn't log in to NATS after the upgrade: $(docker logs --tail 5 "$storage_id" 2>&1 | cut -c1-300)"
# … and publishes an object event through it: an upload reaches the webhook receiver.
status="$(st POST /storage/v1/object/files/upgraded.txt -H 'content-type: text/plain' --data-binary 'after the upgrade')"
[[ "$status" == 200 ]] || die "upload after the upgrade: $status $(cat "$tmp/sbody")"
hook_event upgraded.txt
[[ "$(api GET /v1/me)" == 200 ]] || die "the claimant lost access after the upgrade: $(cat "$tmp/body")"
pass "the upgrade made storage's NATS user and password only, NATS restarted, and storage logged in and delivered an event"

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

step "an SMTP host with no sender fails up --wait at the identity service"
FAIL_PROJECT="chelabase-selfhost-e2e-$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')"
write_env "$FAIL_PROJECT" "$(free_port)" "CHELABASE_SMTP_HOST=smtp.example.com"
UP_ARGS=(up -d --wait --wait-timeout 300)
[[ -n "${SELFHOST_E2E_IMAGES:-}" ]] && UP_ARGS+=(--pull never) || UP_ARGS+=(--build)
if compose_up "$FAIL_PROJECT"; then die "up --wait succeeded with CHELABASE_SMTP_HOST set and no CHELABASE_SMTP_FROM"; fi
grep -q 'identity' "$tmp/$FAIL_PROJECT.up" || { tail -n 20 "$tmp/$FAIL_PROJECT.up" >&2; die "the failure does not name the identity service"; }
compose "$FAIL_PROJECT" logs --no-color identity >"$tmp/fail-identity.log" 2>&1 || true
grep -qF 'CHELABASE_SMTP_HOST is set without CHELABASE_SMTP_FROM' "$tmp/fail-identity.log" ||
  { tail -n 20 "$tmp/fail-identity.log" >&2; die "the identity service doesn't name the missing CHELABASE_SMTP_FROM"; }
pass "up --wait failed at the identity service, which names CHELABASE_SMTP_FROM"
uninstall "$FAIL_PROJECT" >"$tmp/uninstall-fail.log" 2>&1 || { tail -n 20 "$tmp/uninstall-fail.log" >&2; die "the documented uninstall failed"; }
nothing_left "$FAIL_PROJECT"
pass "$FAIL_PROJECT: no containers, volumes or networks left"
FAIL_PROJECT=""
