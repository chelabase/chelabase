#!/usr/bin/env bash
# RT1 final fix round (batch A): upgrading a stack made before RT1 keeps its data. RT1 moves the
# stack to three safekeepers (storcon --timeline-safekeeper-count=3), NeonJWT storage auth, a
# compute network, and Neon services that run as the stack's uid (pageserver and safekeepers
# chown /data once). A pre-RT1 stack has timelines placed on safekeeper1 alone and volumes
# written by root; after `just workbench up` on RT1 code its databases must still open, read
# what they had and take writes, and its volumes must belong to the stack's uid.
#
# Two phases, each in the stack's runner (`just workbench run [--stack <slug>] scripts/upgrade-e2e.sh <phase>`):
#   seed    on the pre-RT1 stack: a database with a row on main and on a branch; the ids go to
#           $CHELABASE_STACK_DIR/upgrade-e2e.env.
#   verify  on the same stack after `just workbench up` with RT1 code: see the checks below.
# The coordinator's procedure (it never runs on the plan's own stack):
#   1. a worktree at the pre-RT1 base (`main`), `just workbench up` there (stack S);
#   2. from the RT1 checkout: `just workbench run --stack S scripts/upgrade-e2e.sh seed`
#      (or copy this script into S's checkout and run it there);
#   3. in S's worktree: move to the RT1 branch, `just workbench up` (the upgrade in place);
#   4. there: `just workbench run scripts/upgrade-e2e.sh verify`; then purge S.
# KEEP=1 leaves the database in place after verify.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=upgrade-e2e
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
phase="${1:-}"
GATEWAY="${GATEWAY_URL:-$CHELABASE_ENTRY_API_URL}"
PROXY_PG_PORT="${CHELABASE_PROXY_PG_PORT:-4432}"
PROXY_HOSTADDR="${CHELABASE_PROXY_HOSTADDR:?run through just workbench run}"
project="${CHELABASE_COMPOSE_PROJECT:?run through just workbench run}"
state="${CHELABASE_STACK_DIR:?run through just workbench run}/upgrade-e2e.env"
DEMO_TENANT_ID="00000000-0000-4000-8000-000000000001"
DEMO_PROJECT_ID="00000000-0000-4000-8000-000000000002"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
t0=$SECONDS
failures=0
pass() { echo "[$LOG_PREFIX] ok   $* ($((SECONDS - t0)) s)"; }
fail() {
  echo "[$LOG_PREFIX] FAIL: $*" >&2
  failures=$((failures + 1))
}

[[ "$phase" == seed || "$phase" == verify ]] || die "usage: scripts/upgrade-e2e.sh seed|verify"
[[ -x "${CHELABASE_CLI:-}" ]] || die "CHELABASE_CLI is unset or not executable (run through: just workbench run scripts/<name>.sh)"
demo_env="${CHELABASE_ZITADEL_DIR:?run through just workbench run}/demo.env"
[[ -f "$demo_env" ]] || die "$demo_env is missing (run just workbench up)"
set -a
# shellcheck disable=SC1090
source "$demo_env"
set +a
ca="${CHELABASE_PROXY_CA_FILE:?run through just workbench run}"
[[ -s "$ca" ]] || die "$ca is missing (run just workbench up)"

ok() {
  local status
  status="$(api "$1" "$2" "${4:-}")"
  [[ "$status" == "$3" ]] || die "$1 $2: $status (wanted $3) $(cat "$tmp/body")"
}
branch_field() {
  ok GET "/v1/databases/$db_id/branches" 200
  jq -r --arg id "$1" ".[] | select(.id == \$id) | .$2 // empty" "$tmp/body"
}
wait_branch_ready() {
  local s=""
  for _ in $(seq 180); do
    s="$(branch_field "$1" status)"
    [[ "$s" != ready ]] || return 0
    [[ "$s" != failed ]] || die "branch $1 failed: $(branch_field "$1" error)"
    sleep 1
  done
  die "branch $1 is '$s' after 180 s"
}
# sql_on <branch id> <sql>: through the proxy (starts the compute); prints the rows. Retries for
# up to 3 min: after the upgrade the compute is recreated on the compute network and the
# storage controller may still be placing the timeline.
sql_on() {
  local id="$1" host pw db
  ok GET "/v1/databases/$db_id/branches/$id/connection" 200
  host="$(jq -r '.host' "$tmp/body")"
  pw="$(jq -r '.password' "$tmp/body")"
  db="$(jq -r '.database' "$tmp/body")"
  for _ in $(seq 90); do
    if PGPASSWORD="$pw" PGCONNECT_TIMEOUT=10 psql \
      "host=$host hostaddr=$PROXY_HOSTADDR port=$PROXY_PG_PORT user=postgres dbname=$db sslmode=verify-full sslrootcert=$ca" \
      -v ON_ERROR_STOP=1 -qtA -c "$2" 2>"$tmp/err"; then
      return 0
    fi
    sleep 2
  done
  echo "psql on branch $id: $(cat "$tmp/err")" >&2
  return 1
}
timeline_of() { metadata_sql -v id="$1" <<<"SELECT timeline_id FROM branches WHERE id = :'id';"; }
storcon_sql() { docker exec -i "$project-storcon-db-1" psql -U storcon -d storage_controller -v ON_ERROR_STOP=1 -qtA "$@"; }

if [[ "$phase" == seed ]]; then
  [[ ! -e "$state" ]] || die "$state exists: verify (or remove it) first"
  name="upg_$(hex 2)"
  status="$(api POST /v1/databases "$(jq -nc --arg t "$DEMO_TENANT_ID" --arg p "$DEMO_PROJECT_ID" --arg n "$name" \
    '{tenant_id: $t, project_id: $p, name: $n}')")"
  [[ "$status" == 202 || "$status" == 201 ]] || die "POST /v1/databases: $status $(cat "$tmp/body")"
  db_id="$(jq -r '.database_id' "$tmp/body")"
  for _ in $(seq 120); do
    ok GET "/v1/databases/$db_id" 200
    [[ "$(jq -r '.status' "$tmp/body")" != ready ]] || break
    [[ "$(jq -r '.status' "$tmp/body")" != failed ]] || die "database $db_id failed"
    sleep 1
  done
  main_id="$db_id"
  sql_on "$main_id" "CREATE TABLE upg (v text); INSERT INTO upg VALUES ('main-before');" >/dev/null ||
    die "writing to main before the upgrade"
  ok POST "/v1/databases/$db_id/branches" 202 "$(jq -nc --arg p "$main_id" '{name: "dev", parent_branch_id: $p}')"
  dev_id="$(jq -r '.id' "$tmp/body")"
  wait_branch_ready "$dev_id"
  sql_on "$dev_id" "INSERT INTO upg VALUES ('dev-before');" >/dev/null || die "writing to dev before the upgrade"
  tenant="$(metadata_sql -v id="$db_id" <<<"SELECT neon_tenant_id FROM databases WHERE id = :'id';")"
  {
    echo "UPG_DB_ID=$db_id"
    echo "UPG_MAIN_ID=$main_id"
    echo "UPG_DEV_ID=$dev_id"
    echo "UPG_TENANT=$tenant"
    echo "UPG_MAIN_TL=$(timeline_of "$main_id")"
    echo "UPG_DEV_TL=$(timeline_of "$dev_id")"
    echo "UPG_SEEDED_SAFEKEEPERS=$(docker ps --filter "label=com.docker.compose.project=$project" \
      --filter label=com.docker.compose.service --format '{{.Label "com.docker.compose.service"}}' | grep -c '^safekeeper' || true)"
  } >"$state"
  pass "seeded database $name ($db_id) with rows on main and dev; ids in $state"
  exit 0
fi

# --- verify ---
[[ -f "$state" ]] || die "$state is missing: run the seed phase on the pre-RT1 stack first"
# shellcheck disable=SC1090
source "$state"
db_id="$UPG_DB_ID"
cleanup() {
  if [[ "${KEEP:-}" != 1 ]]; then
    api DELETE "/v1/databases/$db_id" >/dev/null || true
    rm -f "$state"
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

# 1. Three safekeepers run and the storage controller has them all, active.
for s in safekeeper1 safekeeper2 safekeeper3; do
  [[ -n "$(docker ps -q --filter "label=com.docker.compose.project=$project" --filter "label=com.docker.compose.service=$s")" ]] &&
    pass "$s runs" || fail "$s isn't running after the upgrade"
done
active="$(storcon_sql <<<"SELECT count(*) FROM safekeepers WHERE scheduling_policy = 'active';")" || active="?"
[[ "$active" == 3 ]] && pass "the storage controller has 3 active safekeepers" ||
  fail "the storage controller has $active active safekeepers (wanted 3)"

# 2. The pre-RT1 data: what main and dev had is there, and both take writes (the existing
#    timelines, placed on one safekeeper before, reach a quorum of the three).
for pair in "main:$UPG_MAIN_ID:main-before" "dev:$UPG_DEV_ID:dev-before"; do
  IFS=: read -r label id want <<<"$pair"
  if rows="$(sql_on "$id" "SELECT string_agg(v, ',' ORDER BY v) FROM upg;")"; then
    [[ ",$rows," == *",$want,"* ]] && pass "$label still has its pre-upgrade row ($rows)" ||
      fail "$label lost its pre-upgrade row: $rows"
  else
    fail "$label (a pre-RT1 timeline) doesn't open after the upgrade"
    continue
  fi
  if sql_on "$id" "INSERT INTO upg VALUES ('$label-after'); SELECT count(*) FROM upg WHERE v = '$label-after';" >"$tmp/w"; then
    [[ "$(tail -n1 "$tmp/w")" == 1 ]] && pass "$label takes writes after the upgrade" || fail "$label: the write didn't land"
  else
    fail "$label (a pre-RT1 timeline) refuses writes after the upgrade: $(cat "$tmp/err")"
  fi
done
for tl in "$UPG_MAIN_TL" "$UPG_DEV_TL"; do
  sk_set="$(storcon_sql -v t="$UPG_TENANT" -v tl="$tl" <<<"SELECT sk_set FROM timelines WHERE tenant_id = :'t' AND timeline_id = :'tl';")" || sk_set="?"
  say "pre-RT1 timeline $UPG_TENANT/$tl: storage controller sk_set = ${sk_set:-<none>} (seeded with $UPG_SEEDED_SAFEKEEPERS safekeeper(s))"
done

# 3. A branch made after the upgrade from a pre-RT1 parent lands on all three safekeepers.
ok POST "/v1/databases/$db_id/branches" 202 "$(jq -nc --arg p "$UPG_MAIN_ID" '{name: "after", parent_branch_id: $p}')"
after_id="$(jq -r '.id' "$tmp/body")"
wait_branch_ready "$after_id"
if sql_on "$after_id" "SELECT count(*) FROM upg;" >/dev/null; then pass "a new branch of the old main opens"; else fail "a new branch of the old main doesn't open"; fi
after_tl="$(timeline_of "$after_id")"
n="$(storcon_sql -v t="$UPG_TENANT" -v tl="$after_tl" <<<"SELECT cardinality(sk_set) FROM timelines WHERE tenant_id = :'t' AND timeline_id = :'tl';")" || n="?"
[[ "$n" == 3 ]] && pass "the new branch's timeline is placed on 3 safekeepers" ||
  fail "the new branch's timeline is placed on $n safekeeper(s) (wanted 3)"

# 4. Volumes and processes belong to the stack's uid (any host uid).
want="${CHELABASE_UID:?}:${CHELABASE_GID:?}"
for s in pageserver safekeeper1 safekeeper2 safekeeper3; do
  c="$project-$s-1"
  owner="$(docker exec "$c" stat -c %u:%g /data 2>/dev/null)" || owner="?"
  [[ "$owner" == "$want" ]] && pass "$s: /data belongs to $want" || fail "$s: /data belongs to $owner (wanted $want)"
  uid="$(docker exec "$c" awk '/^Uid:/ { print $2 }' /proc/1/status 2>/dev/null)" || uid="?"
  [[ "$uid" == "${CHELABASE_UID}" ]] && pass "$s runs as uid $uid" || fail "$s runs as uid $uid (wanted $CHELABASE_UID)"
done

# 5. The pre-RT1 database's computes are on the compute network only (with control), not the
#    default network the stack's services share.
for c in $(docker ps -q --filter "label=chelabase.dev/stack=${CHELABASE_STACK:?}"); do
  nets="$(docker inspect -f '{{range $n, $_ := .NetworkSettings.Networks}}{{$n}} {{end}}' "$c")"
  [[ " $nets " == *" ${project}_compute "* && " $nets " != *" ${project}_default "* ]] &&
    pass "compute $c is on ${project}_compute only ($nets)" || fail "compute $c networks: $nets"
done

if [[ "$failures" -gt 0 ]]; then
  echo "[$LOG_PREFIX] $failures failure(s)" >&2
  exit 1
fi
echo "[$LOG_PREFIX] all passed ($((SECONDS - t0)) s)"
