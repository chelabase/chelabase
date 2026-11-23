#!/usr/bin/env bash
# compute-network-reach: the production upgrade, from main's release on the legacy networks to
# this checkout's release on the internal ones, with nothing but the documented
# `docker compose up -d --wait` (no workbench CLI: its own migration removes the computes and
# legacy networks before Compose runs, so it can't test this). Run it with `just upgrade-prod-e2e`
# (it builds this checkout's images, or uses the ones `just images` built with
# UPGRADE_PROD_E2E_IMAGES=local). It needs the network once, to pull main's pinned images.
#
# Case 1, project A:
#   install   `git archive` of the base (the merge base of origin/main and HEAD, or
#             UPGRADE_PROD_E2E_BASE) into a scratch directory, installed at that tree's own image
#             pins, with the setup code claim, a database, a marker row, and a live compute. The
#             compute is on exactly <project>_compute and <project>_control and neither is internal
#             (without this the test proves nothing).
#   (a)       this checkout's compose and the unchanged .env, which still sets the base release's
#             control settings (CHELABASE_CONTROL_SUBNET, CHELABASE_CONTROL_COMPUTE_RANGE,
#             CHELABASE_ORCHESTRATOR_CONTROL_IP, CHELABASE_REALTIME_CONTROL_IP): this release doesn't
#             read them, so `up -d --wait` succeeds and the new networks are on the defaults
#             (10.214.4.0/22 control, 10.214.8.0/22 computes). Nothing else is run.
#   (b)       within UPGRADE_PROD_E2E_BOUND seconds of that `up` (default 240) the old compute is
#             gone, a new one is on exactly <project>_compute-internal and
#             <project>_control-internal, the legacy networks don't exist, the worker logged
#             `removed legacy network` for both, and the marker row reads back from the host and a
#             write lands.
#   (d)       a rerun of `up -d --wait` changes nothing: no container id or start time, the compute
#             included.
#   S6        the base file's `compute-egress` (profile on, blank allow-list) on the release image:
#             healthy, uid 65532, read-only root, every capability 0, ip_forward 0, and only port 53
#             (and the loopback health port) listens; then `rm -sf` as the docs say.
#   (e)       the documented uninstall leaves no container, volume or network, legacy names included.
# Case 2, project B:
#   (c)       the same install, then the new release's settings appended to .env (a custom
#             control range and addresses, a compute range, the egress address; the old lines
#             stay); right after `up -d --wait` returns, the
#             worker is killed as soon as it logs that it removes a compute; then
#             `up -d --wait` again converges to the same end state as (b), with the new networks on
#             the ranges .env sets.
#
# The defaults are fixed, so (a) needs 10.214.4.0/22 and 10.214.8.0/22 free of every other Docker
# network; the run checks that first and stops if they aren't.
#
# Switches: UPGRADE_PROD_E2E_DRY_RUN=1 renders both trees' configs (the old tree, the new tree on
# the unchanged env, the new tree on the edited env) and checks the subnets and addresses that
# decide (a) and (c), starts nothing and builds nothing. UPGRADE_PROD_E2E_KEEP=1 leaves the projects up (tear them down
# with the install docs' uninstall, `-p <name>` added). UPGRADE_PROD_E2E_IMAGES=local uses
# CHELABASE_IMAGE_REPO / CHELABASE_IMAGE_TAG (default chelabase:latest) for this checkout's images.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
new="$root/infra/compose"
LOG_PREFIX=upgrade-prod-e2e
t0=$SECONDS
tmp="$(mktemp -d)"
STEP="start"
PROJECTS=()
PROJECT=""
BOUND="${UPGRADE_PROD_E2E_BOUND:-240}"
DRY="${UPGRADE_PROD_E2E_DRY_RUN:-}"

say() { echo "[$LOG_PREFIX] $*"; }
pass() { echo "ok   $* ($((SECONDS - t0)) s)"; }
step() { STEP="$*"; say "step: $STEP"; }
die() {
  echo "FAIL step '$STEP': $*" >&2
  if [[ -n "$PROJECT" && -z "$DRY" ]]; then
    echo "---- docker ps -a ($PROJECT)" >&2
    docker ps -a --filter "label=com.docker.compose.project=$PROJECT" --format '{{.Names}} {{.Status}}' >&2 2>&1 || true
    docker ps -a --filter "label=chelabase.dev/stack=$PROJECT" --format '{{.Names}} {{.Status}}' >&2 2>&1 || true
    echo "---- networks ($PROJECT)" >&2
    docker network ls --format '{{.Name}}' | grep "^${PROJECT}_" >&2 || true
    echo "---- worker log (tail 30)" >&2
    docker logs --tail 30 "$(docker ps -aq --filter "label=com.docker.compose.project=$PROJECT" --filter label=com.docker.compose.service=worker | head -n1)" 2>&1 | cut -c1-300 >&2 || true
  fi
  exit 1
}

for tool in docker jq curl openssl ss psql git tar; do
  command -v "$tool" >/dev/null || die "$tool is missing"
done

# ---- the two trees -------------------------------------------------------------------------------

step "the base release's compose files, from git archive"
base="${UPGRADE_PROD_E2E_BASE:-$(git -C "$root" merge-base origin/main HEAD)}"
[[ -n "$base" ]] || die "no merge base of origin/main and HEAD (set UPGRADE_PROD_E2E_BASE)"
mkdir -p "$tmp/old"
git -C "$root" archive "$base" infra/compose | tar -x -C "$tmp/old"
old="$tmp/old/infra/compose"
[[ -f "$old/docker-compose.yml" ]] || die "the archive of $base has no infra/compose/docker-compose.yml"
# The base must be the release this epic upgrades from: it has the legacy networks and none of the
# new ones.
grep -qE '^  compute: \{\}' "$old/docker-compose.yml" || die "$base's compose file has no legacy 'compute' network"
! grep -q 'compute-internal' "$old/docker-compose.yml" || die "$base's compose file already has compute-internal"
grep -q 'compute-internal' "$new/docker-compose.yml" || die "this checkout's compose file has no compute-internal network"
say "base ${base:0:12}; this checkout $(git -C "$root" rev-parse --short HEAD)"

case "${UPGRADE_PROD_E2E_IMAGES:-build}" in
  build)
    NEW_FILES=(-f "$new/docker-compose.yml" -f "$new/docker-compose.build.yml")
    NEW_ENV=(env)
    ;;
  local)
    NEW_FILES=(-f "$new/docker-compose.yml")
    NEW_ENV=(env "CHELABASE_IMAGE_REPO=${CHELABASE_IMAGE_REPO:-chelabase}" "CHELABASE_IMAGE_TAG=${CHELABASE_IMAGE_TAG:-latest}")
    ;;
  *) die "UPGRADE_PROD_E2E_IMAGES must be unset, 'build' or 'local'" ;;
esac
UP_ARGS=(up -d --wait --wait-timeout 900)

# oldc / newc <args…>: docker compose on the base tree (its pins) or on this checkout, for $PROJECT.
oldc() {
  docker compose -p "$PROJECT" --project-directory "$old" -f "$old/docker-compose.yml" \
    -f "$tmp/$PROJECT.override.yml" --env-file "$tmp/$PROJECT.env" "$@"
}
newc() {
  "${NEW_ENV[@]}" docker compose -p "$PROJECT" --project-directory "$new" "${NEW_FILES[@]}" \
    -f "$tmp/$PROJECT.override.yml" --env-file "$tmp/$PROJECT.env" "$@"
}

# ---- failure and cleanup -------------------------------------------------------------------------

# teardown <project>: the documented uninstall as the new release says it, then whatever it left.
teardown() {
  local p="$1" ids n
  PROJECT="$p"
  # The compute-egress profile too: a run that fails during S6 leaves that container up.
  newc --profile compute-egress stop --timeout 20 >/dev/null 2>&1 || true
  ids="$(docker ps -aq --filter "label=chelabase.dev/stack=$p" 2>/dev/null || true)"
  # shellcheck disable=SC2086
  [[ -z "$ids" ]] || docker rm -f $ids >/dev/null 2>&1 || true
  newc --profile compute-egress down -v --remove-orphans --timeout 20 >/dev/null 2>&1 || true
  for n in "${p}_compute" "${p}_control" "${p}_compute-internal" "${p}_control-internal"; do
    docker network rm "$n" >/dev/null 2>&1 || true
  done
}
cleanup() {
  local rc=$? p
  trap - EXIT
  if [[ "${UPGRADE_PROD_E2E_KEEP:-}" == 1 ]]; then
    say "UPGRADE_PROD_E2E_KEEP=1: leaving ${PROJECTS[*]:-nothing} up ($tmp kept)"
  else
    for p in "${PROJECTS[@]}"; do teardown "$p"; done
    rm -rf "$tmp"
  fi
  if [[ $rc -eq 0 ]]; then say "PASS in $((SECONDS - t0)) s"; else say "FAILED (exit $rc) in $((SECONDS - t0)) s"; fi
  exit $rc
}
trap cleanup EXIT

forget_project() { # forget_project <project>: it is gone, so the cleanup needn't tear it down
  local keep=() q
  for q in "${PROJECTS[@]}"; do [[ "$q" == "$1" ]] || keep+=("$q"); done
  PROJECTS=("${keep[@]}")
}

# ---- helpers -------------------------------------------------------------------------------------

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
# free_octet: N such that 10.N.0.0/16 overlaps no Docker network (every subnet below is in it), and
# never 214, the defaults' octet.
free_octet() {
  local n subnets
  subnets="$(docker network inspect $(docker network ls -q) -f '{{range .IPAM.Config}}{{.Subnet}} {{end}}' 2>/dev/null || true)"
  for _ in $(seq 60); do
    n=$((100 + RANDOM % 150))
    if ((n != 214)) && ! grep -qE "(^| )10\.$n\." <<<"$subnets"; then
      echo "$n"
      return
    fi
  done
  die "no free 10.N.0.0/16 network found"
}

# DEFAULT_*: this release's defaults, which (a) expects on the unchanged .env.
DEFAULT_CONTROL_SUBNET=10.214.4.0/22
DEFAULT_CONTROL_RANGE=10.214.6.0/23
DEFAULT_ORCHESTRATOR_IP=10.214.4.10
DEFAULT_REALTIME_IP=10.214.4.12
DEFAULT_COMPUTE_SUBNET=10.214.8.0/22
DEFAULT_COMPUTE_RANGE=10.214.10.0/23
DEFAULT_EGRESS_IP=10.214.8.2

ip_int() { local a b c d; IFS=. read -r a b c d <<<"$1"; echo $(((a << 24) | (b << 16) | (c << 8) | d)); }
cidr_overlap() { # cidr_overlap <a/len> <b/len>: whether the two IPv4 networks share an address
  local la="${1#*/}" lb="${2#*/}" l mask
  l=$((la < lb ? la : lb))
  mask=$(((0xffffffff << (32 - l)) & 0xffffffff))
  (((($(ip_int "${1%/*}") ^ $(ip_int "${2%/*}")) & mask) == 0))
}
# defaults_free: no Docker network holds an address of the default subnets.
defaults_free() {
  local subnets s d
  subnets="$(docker network inspect $(docker network ls -q) -f '{{range .IPAM.Config}}{{.Subnet}} {{end}}' 2>/dev/null || true)"
  for s in $subnets; do
    [[ "$s" == *.*.*.*/* ]] || continue
    for d in "$DEFAULT_CONTROL_SUBNET" "$DEFAULT_COMPUTE_SUBNET"; do
      ! cidr_overlap "$s" "$d" || die "the Docker network on $s overlaps this release's default $d, which (a) needs free (remove that network or run elsewhere)"
    done
  done
}

# write_env <project>: the base release's install .env: its .env.example with the hosts moved to a
# free port and a private control subnet (what an operator who set the base release's
# CHELABASE_CONTROL_SUBNET has), nothing for the compute network (the base release has no such
# settings).
write_env() {
  local p="$1"
  PORT="$(free_port)"
  local pg_port http_port
  N="$(free_octet)"
  pg_port="$(free_port)"
  http_port="$(free_port)"
  while [[ "$http_port" == "$pg_port" || "$http_port" == "$PORT" ]]; do http_port="$(free_port)"; done
  {
    sed -E "s/^(CHELABASE_[A-Z]+_HOST=.*):8000\$/\1:$PORT/" "$old/.env.example"
    echo "CHELABASE_STUDIO_HOST_NAME=studio.chelabase.localhost"
    echo "CHELABASE_API_HOST_NAME=api.chelabase.localhost"
    echo "CHELABASE_AUTH_HOST_NAME=auth.chelabase.localhost"
    echo "CHELABASE_PUBLIC_PORT=$PORT"
    echo "CHELABASE_PUBLIC_SECURE=false"
    echo "CHELABASE_CONTROL_SUBNET=10.$N.0.0/22"
    echo "CHELABASE_CONTROL_COMPUTE_RANGE=10.$N.2.0/23"
    echo "CHELABASE_ORCHESTRATOR_CONTROL_IP=10.$N.0.10"
    echo "CHELABASE_REALTIME_CONTROL_IP=10.$N.0.12"
    echo "CHELABASE_PROXY_PORT=$pg_port"
    echo "CHELABASE_PROXY_HTTP_PORT=$http_port"
  } >"$tmp/$p.env"
  chmod 600 "$tmp/$p.env"
  # The seeds' bind of ./.env is this file instead, so no .env is written into either tree.
  cat >"$tmp/$p.override.yml" <<EOF
services:
  init-secrets:
    volumes:
      - {type: bind, source: $tmp/$p.env, target: /run/chelabase/seeds.env, read_only: true, bind: {create_host_path: false}}
EOF
  PROXY_PORT="$pg_port"
  STUDIO="http://studio.chelabase.localhost:$PORT"
  API="http://api.chelabase.localhost:$PORT"
}

# upgrade_env: an operator who wants custom ranges on this release sets its settings: the control
# network on a free range of their own, the compute network's range and egress address. The base
# release's lines stay in .env (this release doesn't read them).
upgrade_env() {
  local f="$tmp/$PROJECT.env"
  {
    echo "CHELABASE_CONTROL_INTERNAL_SUBNET=10.$N.4.0/22"
    echo "CHELABASE_CONTROL_INTERNAL_COMPUTE_RANGE=10.$N.6.0/23"
    echo "CHELABASE_ORCHESTRATOR_CONTROL_INTERNAL_IP=10.$N.4.10"
    echo "CHELABASE_REALTIME_CONTROL_INTERNAL_IP=10.$N.4.12"
    echo "CHELABASE_COMPUTE_SUBNET=10.$N.8.0/22"
    echo "CHELABASE_COMPUTE_RANGE=10.$N.10.0/23"
    echo "CHELABASE_COMPUTE_EGRESS_IP=10.$N.8.2"
  } >>"$f"
}

net_internal() { docker network inspect -f '{{.Internal}}' "$1" 2>/dev/null || echo missing; }
net_exists() { docker network inspect "$1" >/dev/null 2>&1; }
net_subnet() { docker network inspect -f '{{range .IPAM.Config}}{{.Subnet}} {{end}}' "$1" 2>/dev/null | sed 's/ $//'; }
# compute_ids: the running and stopped computes of the project (full ids).
compute_ids() { docker ps -aq --no-trunc --filter "label=chelabase.dev/stack=$PROJECT"; }
nets_of() { docker inspect -f '{{range $n, $_ := .NetworkSettings.Networks}}{{$n}} {{end}}' "$1" | tr ' ' '\n' | sed '/^$/d' | sort | tr '\n' ' '; }
want_new_nets() { printf '%s\n' "${PROJECT}_compute-internal" "${PROJECT}_control-internal" | sort | tr '\n' ' '; }
want_old_nets() { printf '%s\n' "${PROJECT}_compute" "${PROJECT}_control" | sort | tr '\n' ' '; }

# snap <file>: every container of the project (Compose's and the computes): name, id, state and the
# start time of a running one. One-shots keep their id and state only.
snap() {
  local ids
  ids="$({
    docker ps -aq --no-trunc --filter "label=com.docker.compose.project=$PROJECT"
    docker ps -aq --no-trunc --filter "label=chelabase.dev/stack=$PROJECT"
  } | sort -u)"
  [[ -n "$ids" ]] || die "no container of $PROJECT to snapshot"
  # shellcheck disable=SC2086
  docker inspect -f '{{.Name}} {{.Id}} {{.State.Status}} {{if eq .State.Status "running"}}{{.State.StartedAt}}{{end}}' $ids | sort >"$1"
}

# ---- the sign-in client (Studio's login API, no browser), as selfhost-e2e.sh has it ---------------

ui() { # ui <path> <json> <jar>
  local out
  out="$(curl -sS --max-time 30 -b "$3" -c "$3" -H "Origin: $STUDIO" -H 'X-Chelabase-Studio: 1' \
    -H 'Content-Type: application/json' -w '\n%{http_code}' -X POST "$API/auth/v1/$1" -d "$2")" || die "POST /auth/v1/$1: no answer"
  printf '%s' "${out##*$'\n'}" >"$tmp/ui-status"
  printf '%s' "${out%$'\n'*}"
}
session_token() {
  local out token
  out="$(curl -sS --max-time 30 -b "$1" -H "Origin: $STUDIO" -H 'X-Chelabase-Studio: 1' -X POST "$API/auth/v1/token")" ||
    die "POST /auth/v1/token: no answer"
  token="$(jq -r '.access_token // empty' <<<"$out" 2>/dev/null)"
  [[ -n "$token" ]] || die "POST /auth/v1/token: $out"
  echo "$token"
}
api() { # api <method> <path> [json]: status to stdout, body in $tmp/body
  local args=(-sS --max-time 60 -o "$tmp/body" -w '%{http_code}' -X "$1" "$API$2") token
  [[ -z "${3:-}" ]] || args+=(-H 'content-type: application/json' -d "$3")
  token="$(session_token "$tmp/admin.jar")"
  printf 'Authorization: Bearer %s\n' "$token" | curl "${args[@]}" -H @- || echo 000
}

# psql_host <args…>: on the host, through the published proxy, verify-full against the install's CA.
psql_host() { PGCONNECT_TIMEOUT=10 psql "$PG_URI&sslrootcert=$tmp/$PROJECT.ca.crt&hostaddr=127.0.0.1" -v ON_ERROR_STOP=1 -qtAX "$@"; }
# psql_retry <seconds> <args…>: psql_host until it answers (a compute being replaced takes a while).
psql_retry() {
  local secs="$1" deadline out
  shift
  deadline=$((SECONDS + secs))
  while ((SECONDS < deadline)); do
    if out="$(psql_host "$@" 2>"$tmp/psql.err")"; then
      printf '%s\n' "$out"
      return 0
    fi
    sleep 3
  done
  return 1
}

# ---- the install of the base release ---------------------------------------------------------------

# install_old <project>: the base release at its own pins on the legacy networks, claimed, with a
# database holding a marker row and a running compute. Sets OLD_COMPUTE, DB_ID, PG_URI.
install_old() {
  local p="$1" code out status project_id deadline db_status pw email password
  PROJECT="$p"
  PROJECTS+=("$p")
  step "$p: the base release on the legacy networks (its own pins, pulled)"
  write_env "$p"
  oldc config --quiet || die "the base tree's compose config doesn't render"
  oldc "${UP_ARGS[@]}" >"$tmp/$p.old.up" 2>&1 || { tail -n 40 "$tmp/$p.old.up" >&2; die "the base release's up --wait failed"; }
  code="$(oldc logs --no-color init-secrets 2>&1 | sed -nE 's/.*Setup code: ([A-Z0-9]{4}(-[A-Z0-9]{4}){4}).*/\1/p' | head -n1)"
  [[ -n "$code" ]] || die "no setup code in the base release's init-secrets log"
  email="admin-$(openssl rand -hex 3)@example.com"
  password="Upgrade-e2e-$(openssl rand -hex 6)A!"
  : >"$tmp/admin.jar"
  out="$(ui sign-up "$(jq -nc --arg e "$email" --arg p "$password" --arg c "$code" \
    '{email: $e, givenName: "Up", familyName: "Grade", password: $p, setupCode: $c}')" "$tmp/admin.jar")"
  [[ "$(cat "$tmp/ui-status")" == 200 || "$(cat "$tmp/ui-status")" == 201 ]] || die "claiming the instance by sign-up: $(cat "$tmp/ui-status") $out"
  out="$(ui sign-in "$(jq -nc --arg e "$email" --arg p "$password" '{email: $e, password: $p}')" "$tmp/admin.jar")"
  [[ "$(cat "$tmp/ui-status")" == 200 ]] || die "sign-in after the claim: $(cat "$tmp/ui-status") $out"
  [[ "$(api GET /v1/me)" == 200 ]] || die "GET /v1/me: $(cat "$tmp/body")"
  jq -e '.instance_admin == true and .tenant_id != null' "$tmp/body" >/dev/null || die "the claimant isn't an instance admin of a tenant: $(head -c 300 "$tmp/body")"

  status="$(api POST /v1/projects '{"name":"upgrade"}')"
  [[ "$status" == 201 || "$status" == 200 ]] || die "POST /v1/projects: $status $(cat "$tmp/body")"
  project_id="$(jq -r '.id // .project_id' "$tmp/body")"
  status="$(api POST /v1/databases "$(jq -nc --arg p "$project_id" '{project_id: $p, name: "upgrade_db"}')")"
  [[ "$status" == 201 || "$status" == 202 ]] || die "POST /v1/databases: $status $(cat "$tmp/body")"
  DB_ID="$(jq -r '.database_id' "$tmp/body")"
  db_status=""
  deadline=$((SECONDS + 240))
  while ((SECONDS < deadline)); do
    [[ "$(api GET "/v1/databases/$DB_ID")" == 200 ]] || die "GET /v1/databases/$DB_ID: $(cat "$tmp/body")"
    db_status="$(jq -r .status "$tmp/body")"
    [[ "$db_status" != failed ]] || die "database $DB_ID failed: $(jq -r .error "$tmp/body")"
    [[ "$db_status" != ready ]] || break
    sleep 2
  done
  [[ "$db_status" == ready ]] || die "database $DB_ID is still '$db_status' after 240 s"
  [[ "$(api GET "/v1/databases/$DB_ID/connection")" == 200 ]] || die "GET /connection: $(cat "$tmp/body")"
  PG_URI="$(jq -r .uri "$tmp/body")"
  oldc cp proxy:/tls/ca.crt "$tmp/$p.ca.crt" >/dev/null 2>"$tmp/cp.err" || die "docker compose cp of the CA: $(cat "$tmp/cp.err")"
  psql_retry 120 -c 'create table upg(v text); insert into upg values ($$before$$); select count(*) from upg' >/dev/null ||
    die "the database never answered through the published proxy: $(cat "$tmp/psql.err")"

  OLD_COMPUTE="$(compute_ids | head -n1)"
  [[ -n "$OLD_COMPUTE" && "$(compute_ids | wc -l)" == 1 ]] || die "expected one compute of $p, found: $(compute_ids | tr '\n' ' ')"
  [[ "$(docker inspect -f '{{.State.Status}}' "$OLD_COMPUTE")" == running ]] || die "the compute isn't running"
  [[ "$(nets_of "$OLD_COMPUTE")" == "$(want_old_nets)" ]] || die "the compute is on [$(nets_of "$OLD_COMPUTE")], not exactly the legacy [$(want_old_nets)]"
  for n in "${p}_compute" "${p}_control"; do
    [[ "$(net_internal "$n")" == false ]] || die "$n is '$(net_internal "$n")' (the base release's network must be an ordinary, non-internal one)"
  done
  pass "$p: the base release runs a compute (${OLD_COMPUTE:0:12}) on exactly the two legacy, non-internal networks, with a marker row"
}

# data_ok <label>: the marker row reads back and a write lands.
data_ok() {
  local out
  out="$(psql_retry 180 -c 'select string_agg(v, $$,$$ order by v) from upg')" || die "$1: the data doesn't read back: $(cat "$tmp/psql.err")"
  [[ ",$out," == *",before,"* ]] || die "$1: the marker row is gone: '$out'"
  psql_retry 60 -c "insert into upg values (\$\$after-$1\$\$); select count(*) from upg where v = \$\$after-$1\$\$" | tail -n1 | grep -qx 1 ||
    die "$1: a write didn't land: $(cat "$tmp/psql.err")"
}

# converged <label> <old compute id> <seconds>: within the bound the old compute is gone, a new one
# is running on exactly the internal networks, and the legacy networks don't exist. Dies naming what
# is missing at the deadline.
converged() {
  # secs is set before deadline reads it: one `local` expands every word before it assigns any.
  local label="$1" old_id="$2" secs="$3"
  local deadline=$((SECONDS + secs)) why="" new_id="" ids n
  while :; do
    why=""
    docker inspect "$old_id" >/dev/null 2>&1 && why="the old compute ${old_id:0:12} still exists"
    for n in "${PROJECT}_compute" "${PROJECT}_control"; do
      ! net_exists "$n" || why="$why; the legacy network $n still exists"
    done
    new_id="$(docker ps -q --no-trunc --filter "label=chelabase.dev/stack=$PROJECT" --filter status=running | head -n1)"
    if [[ -z "$new_id" ]]; then
      why="$why; no running compute"
    elif [[ "$(nets_of "$new_id")" != "$(want_new_nets)" ]]; then
      why="$why; the compute is on [$(nets_of "$new_id")]"
    fi
    if [[ -z "$why" ]]; then
      ids="$(compute_ids | wc -l)"
      [[ "$ids" == 1 ]] || die "$label: $ids computes of $PROJECT after the migration"
      NEW_COMPUTE="$new_id"
      return 0
    fi
    ((SECONDS < deadline)) || die "$label: not converged within $secs s:${why#;}"
    sleep 3
  done
}

# the_end_state <label>: what every upgraded project looks like.
the_end_state() {
  local n
  for n in "${PROJECT}_compute-internal" "${PROJECT}_control-internal"; do
    [[ "$(net_internal "$n")" == true ]] || die "$1: $n is '$(net_internal "$n")', not internal"
  done
  for n in "${PROJECT}_compute" "${PROJECT}_control"; do
    ! net_exists "$n" || die "$1: the legacy network $n exists"
  done
  [[ "$NEW_COMPUTE" != "$OLD_COMPUTE" ]] || die "$1: the compute kept its container id"
  data_ok "$1"
}

nothing_left() { # nothing_left <project>
  local p="$1" left
  left="$({
    docker ps -a --format '{{.Names}}' --filter "label=com.docker.compose.project=$p"
    docker ps -a --format '{{.Names}}' --filter "label=chelabase.dev/stack=$p"
    docker volume ls -q --filter "label=com.docker.compose.project=$p"
    docker network ls --format '{{.Name}}' | grep -E "^${p}_" || true
  })"
  [[ -z "$left" ]] || die "$p left behind: $(echo "$left" | tr '\n' ' ')"
}

# ---- the dry run ---------------------------------------------------------------------------------

# rendered_nets <label> <control subnet> <control range> <orchestrator ip> <realtime ip>
#   <compute subnet> <compute range> <egress ip>: this checkout's rendered config (the egress
#   profile on) puts the new networks, the static addresses and every value derived from them there.
rendered_nets() {
  local label="$1" json
  json="$(newc --profile compute-egress config --format json)" || die "$label: this checkout doesn't render"
  local got
  got="$(jq -r '[.networks["control-internal"].ipam.config[0].subnet, .networks["control-internal"].ipam.config[0].ip_range,
    .services.orchestrator.networks["control-internal"].ipv4_address, .services.realtime.networks["control-internal"].ipv4_address,
    .networks["compute-internal"].ipam.config[0].subnet, .networks["compute-internal"].ipam.config[0].ip_range,
    .services["compute-egress"].networks["compute-internal"].ipv4_address] | join(" ")' <<<"$json")"
  [[ "$got" == "$2 $3 $4 $5 $6 $7 $8" ]] ||
    die "$label: networks and addresses are [$got], want [$2 $3 $4 $5 $6 $7 $8]"
  got="$(jq -r '[.services.worker.environment.CHELABASE_MEMBER_LOGIN_CIDRS, .services.worker.environment.CHELABASE_REALTIME_LOGIN_CIDRS,
    .services.worker.environment.CHELABASE_COMPUTE_EGRESS_DNS, .services["compute-egress"].environment.CHELABASE_INTERNAL_SUBNETS] | join(" ")' <<<"$json")"
  [[ "$got" == "$4/32 $5/32 $8 $6,$2" ]] ||
    die "$label: the login networks, resolver and egress deny list are [$got], want [$4/32 $5/32 $8 $6,$2]"
  [[ "$(jq -r '.networks["compute-internal"].internal' <<<"$json")" == true ]] || die "$label: compute-internal isn't internal"
}

dry_run() {
  local p=dry
  PROJECT="$p"
  step "dry run: both trees render, and the subnets and addresses that decide (a) and (c) are the ones the plan needs"
  write_env "$p"
  oldc config --quiet || die "the base tree doesn't render"
  [[ "$(oldc config --format json | jq -r '.networks.control.ipam.config[0].subnet')" == "10.$N.0.0/22" ]] ||
    die "the base release's control network isn't on its CHELABASE_CONTROL_SUBNET"
  rendered_nets "the unchanged .env (the base release's control settings still in it)" \
    "$DEFAULT_CONTROL_SUBNET" "$DEFAULT_CONTROL_RANGE" "$DEFAULT_ORCHESTRATOR_IP" "$DEFAULT_REALTIME_IP" \
    "$DEFAULT_COMPUTE_SUBNET" "$DEFAULT_COMPUTE_RANGE" "$DEFAULT_EGRESS_IP"
  upgrade_env
  rendered_nets "the new release's settings set" \
    "10.$N.4.0/22" "10.$N.6.0/23" "10.$N.4.10" "10.$N.4.12" "10.$N.8.0/22" "10.$N.10.0/23" "10.$N.8.2"
  say "plan: install ${base:0:12} (A, B); (a) up -d --wait on the unchanged .env succeeds onto the defaults; (b) it converges within ${BOUND} s; (d) rerun changes nothing; compute-egress start; (e) uninstall; (c) B with the new settings, the worker killed mid-migration"
  pass "dry run: nothing was started or built"
}
if [[ -n "$DRY" ]]; then
  dry_run
  exit 0
fi

# ---- case 1: (a), (b), (d), S6, (e) ----------------------------------------------------------------

defaults_free
A="chelabase-upgrade-prod-$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')"
install_old "$A"
if [[ "${UPGRADE_PROD_E2E_IMAGES:-build}" == build ]]; then
  step "build this checkout's images (once, outside every bound)"
  newc build >"$tmp/build.log" 2>&1 || { tail -n 30 "$tmp/build.log" >&2; die "building this checkout's images"; }
fi

step "(a) the unchanged .env (the base release's control settings still in it): up -d --wait succeeds onto the defaults"
b_start=$SECONDS
newc "${UP_ARGS[@]}" >"$tmp/a.up" 2>&1 || { tail -n 40 "$tmp/a.up" >&2; die "the upgrade's up --wait failed on the unchanged .env"; }
b_up=$((SECONDS - b_start))
[[ "$(net_subnet "${A}_control-internal")" == "$DEFAULT_CONTROL_SUBNET" ]] ||
  die "(a) ${A}_control-internal is on '$(net_subnet "${A}_control-internal")', not the default $DEFAULT_CONTROL_SUBNET"
[[ "$(net_subnet "${A}_compute-internal")" == "$DEFAULT_COMPUTE_SUBNET" ]] ||
  die "(a) ${A}_compute-internal is on '$(net_subnet "${A}_compute-internal")', not the default $DEFAULT_COMPUTE_SUBNET"
pass "(a) up -d --wait returned after $b_up s on the unchanged .env; the new networks are on $DEFAULT_CONTROL_SUBNET and $DEFAULT_COMPUTE_SUBNET"

step "(b) nothing else: the stack converges"
converged b "$OLD_COMPUTE" "$BOUND"
b_total=$((SECONDS - b_start))
((b_total <= BOUND)) || die "(b) converged after $b_total s, bound $BOUND s"
the_end_state b
worker_log="$(newc logs --no-color worker 2>&1 | sed $'s/\x1b\\[[0-9;]*m//g')"
for n in "${A}_compute" "${A}_control"; do
  grep -F 'removed legacy network' <<<"$worker_log" | grep -qE "network=$n([^-[:alnum:]_]|\$)" ||
    die "(b) the worker's log has no 'removed legacy network' for $n"
done
pass "(b) converged in $b_total s from the start of (a)'s up: the old compute is replaced by one on exactly the two internal networks, the legacy networks are gone and logged, data reads back and takes a write"

step "(d) a rerun of up -d --wait changes nothing"
snap "$tmp/d.before"
newc "${UP_ARGS[@]}" >"$tmp/d.up" 2>&1 || { tail -n 40 "$tmp/d.up" >&2; die "the rerun's up --wait failed"; }
snap "$tmp/d.after"
diff -u "$tmp/d.before" "$tmp/d.after" >"$tmp/d.diff" || die "a rerun recreated or restarted containers: $(head -20 "$tmp/d.diff")"
[[ "$(compute_ids | head -n1)" == "$NEW_COMPUTE" ]] || die "the rerun replaced the compute"
pass "(d) the rerun changed no container id or start time, the compute included"

step "S6 the base file's compute-egress runs as its release image says"
newc --profile compute-egress up -d --wait --wait-timeout 300 compute-egress >"$tmp/egress.up" 2>&1 ||
  { tail -n 30 "$tmp/egress.up" >&2; die "starting compute-egress on this checkout's image failed"; }
egress="$(newc --profile compute-egress ps -q compute-egress)"
[[ -n "$egress" ]] || die "no compute-egress container"
[[ "$(docker inspect -f '{{.State.Health.Status}}' "$egress")" == healthy ]] || die "compute-egress isn't healthy"
[[ "$(docker inspect -f '{{.HostConfig.ReadonlyRootfs}}' "$egress")" == true ]] || die "compute-egress's root filesystem isn't read-only"
[[ "$(docker inspect -f '{{.Config.User}}' "$egress")" == 65532:65532 ]] || die "compute-egress's configured user is '$(docker inspect -f '{{.Config.User}}' "$egress")'"
# The forwarder's process and network namespace, read from a helper container that joins them (the
# engine's PIDs aren't in this host's /proc when Docker runs in a VM). The helper is the project's own
# pinned metadata-db image, already pulled.
helper="$(newc config --format json | jq -r '.services["metadata-db"].image')"
[[ -n "$helper" && "$helper" != null ]] || die "no metadata-db image in the rendered config"
in_pid() { docker run --rm --network none --pid "container:$egress" --entrypoint cat "$helper" "$@"; }
in_net() { docker run --rm --network "container:$egress" --entrypoint cat "$helper" "$@"; }
estatus="$(in_pid /proc/1/status)" || die "can't read compute-egress's /proc/1/status"
[[ "$(awk '/^Uid:/ { print $2 "," $3 "," $4 "," $5 }' <<<"$estatus")" == 65532,65532,65532,65532 ]] ||
  die "compute-egress's process uids are $(awk '/^Uid:/ { print $2 "," $3 "," $4 "," $5 }' <<<"$estatus")"
caps="$(awk '/^Cap(Inh|Prm|Eff|Bnd|Amb):/ { print $2 }' <<<"$estatus" | sort -u | tr '\n' ' ')"
[[ "$caps" == "0000000000000000 " ]] || die "compute-egress has capabilities: $(grep '^Cap' <<<"$estatus" | tr '\n' ' ')"
for sc in net.ipv4.ip_forward net.ipv4.conf.all.forwarding; do
  [[ "$(in_net "/proc/sys/${sc//./\/}" 2>/dev/null || echo unreadable)" == 0 ]] || die "compute-egress's $sc isn't 0"
done
# Listeners (TCP): only port 53, and loopback addresses: the health port on 127.0.0.1 and the engine's
# own resolver on 127.0.0.11. /proc/net/tcp writes an IPv4 address as little-endian hex, so a
# 127.0.0.0/8 one ends in 7F.
etcp="$(in_net /proc/net/tcp)" || die "can't read compute-egress's /proc/net/tcp"
bad=""
while read -r addr hexport; do
  [[ "$addr" == ??????7F || $((16#$hexport)) == 53 ]] || bad="$bad $addr:$((16#$hexport))"
done < <(awk 'NR > 1 && $4 == "0A" { split($2, a, ":"); print a[1], a[2] }' <<<"$etcp")
[[ -z "$bad" ]] || die "compute-egress listens on: $bad"
newc --profile compute-egress rm -sf compute-egress >/dev/null 2>&1 || die "removing compute-egress as the docs say"
[[ -z "$(newc --profile compute-egress ps -aq compute-egress)" ]] || die "compute-egress is still there after rm -sf"
pass "S6 compute-egress on the release image: healthy, uid 65532, read-only, no capability, ip_forward 0, only port 53 (and loopback) listens; rm -sf removed it"

step "(e) the documented uninstall leaves no network"
newc stop --timeout 20 >/dev/null 2>&1 || die "docker compose stop"
docker ps -aq --filter "label=chelabase.dev/stack=$A" | xargs -r docker rm -f >/dev/null || die "removing the computes"
newc down -v --timeout 20 >"$tmp/e.down" 2>&1 || { tail -n 20 "$tmp/e.down" >&2; die "docker compose down -v"; }
docker network rm "${A}_compute" "${A}_control" "${A}_compute-egress" >/dev/null 2>&1 || true
nothing_left "$A"
forget_project "$A"
pass "(e) $A: no container, volume or network left (the legacy names included)"

# ---- case 2: (c) the worker is killed mid-migration ------------------------------------------------

B="chelabase-upgrade-prod-$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')"
install_old "$B"
step "(c) kill the worker as soon as it removes a compute, then run up -d --wait again"
upgrade_env
c_start="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
newc "${UP_ARGS[@]}" >"$tmp/c.up" 2>&1 || { tail -n 40 "$tmp/c.up" >&2; die "the upgrade's up --wait failed"; }
worker="$(newc ps -q worker)"
[[ -n "$worker" ]] || die "no worker container"
seen=""
deadline=$((SECONDS + 180))
while ((SECONDS < deadline)); do
  if docker logs --since "$c_start" "$worker" 2>&1 | grep -q 'removing it so reconcile creates it again'; then
    seen=1
    break
  fi
  sleep 1
done
[[ -n "$seen" ]] || die "(c) the worker never logged the removal of a compute within 180 s"
docker kill "$worker" >/dev/null 2>&1 || die "(c) couldn't kill the worker"
say "the worker (${worker:0:12}) was killed mid-migration"
newc "${UP_ARGS[@]}" >"$tmp/c.up2" 2>&1 || { tail -n 40 "$tmp/c.up2" >&2; die "(c) the second up --wait failed"; }
converged c "$OLD_COMPUTE" "$BOUND"
the_end_state c
[[ "$(net_subnet "${B}_control-internal") $(net_subnet "${B}_compute-internal")" == "10.$N.4.0/22 10.$N.8.0/22" ]] ||
  die "(c) the new networks are on '$(net_subnet "${B}_control-internal") $(net_subnet "${B}_compute-internal")', not the ranges .env sets (10.$N.4.0/22 10.$N.8.0/22)"
pass "(c) after the worker was killed and up -d --wait run again, $B converged to the same end state on the ranges .env sets, and its data reads back"

newc stop --timeout 20 >/dev/null 2>&1 || die "docker compose stop"
docker ps -aq --filter "label=chelabase.dev/stack=$B" | xargs -r docker rm -f >/dev/null || die "removing the computes"
newc down -v --timeout 20 >"$tmp/c.down" 2>&1 || die "docker compose down -v"
nothing_left "$B"
forget_project "$B"
pass "$B: uninstalled with nothing left"
