#!/usr/bin/env bash
# Live check of roadmap step 5 (branching), through the public API and the proxy (needs
# the stack: just workbench up). The same story as services/worker/tests/branch_live.rs:
# - a branch at a past time has the old rows; reset to parent brings it back to its parent's;
# - restore rewinds main and keeps the old data in a backup (no compute) that can be branched;
# - a point outside the history window is refused, naming the oldest available point's time;
# - deleting the database removes every compute, the tenant and the endpoint rows.
# KEEP=1 leaves the database in place.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=branch-e2e
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
STORCON="${STORCON_URL:-http://storage-controller:1234}"
GATEWAY="${GATEWAY_URL:-$CHELABASE_ENTRY_API_URL}"
PROXY_PG_PORT="${CHELABASE_PROXY_PG_PORT:-4432}"
PROXY_HOSTADDR="${CHELABASE_PROXY_HOSTADDR:?run through just workbench run}"
DEMO_TENANT_ID="00000000-0000-4000-8000-000000000001"
DEMO_PROJECT_ID="00000000-0000-4000-8000-000000000002"
tmp="$(mktemp -d)"
db_id=""
t0=$SECONDS

pass() { echo "ok   $* ($((SECONDS - t0)) s)"; }
die() {
  echo "FAIL $*" >&2
  exit 1
}
cleanup() {
  if [[ -n "$db_id" && "${KEEP:-}" != 1 ]]; then
    api DELETE "/v1/databases/$db_id" >/dev/null || true
  fi
  rm -rf "$tmp"
}

[[ -x "${CHELABASE_CLI:-}" ]] || die "CHELABASE_CLI is unset or not executable (run through: just workbench run scripts/<name>.sh)"
demo_env="${CHELABASE_ZITADEL_DIR:?run through just workbench run}/demo.env"
[[ -f "$demo_env" ]] || die "$demo_env is missing (run just workbench up)"
set -a
# shellcheck disable=SC1091
source "$demo_env"
set +a
ca="${CHELABASE_PROXY_CA_FILE:?run through just workbench run}"
[[ -s "$ca" ]] || die "$ca is missing (run just workbench up)"
trap cleanup EXIT

# ok <method> <path> <expected status> [body]: the call, or die.
ok() {
  local status
  status="$(api "$1" "$2" "${4:-}")"
  [[ "$status" == "$3" ]] || die "$1 $2: $status (wanted $3) $(cat "$tmp/body")"
}

# wait_db: until the database is ready.
wait_db() {
  local s=""
  for i in $(seq 120); do
    ok GET "/v1/databases/$db_id" 200
    s="$(jq -r '.status' "$tmp/body")"
    case "$s" in
      ready) return ;;
      failed) die "database $db_id failed: $(jq -r '.error' "$tmp/body")" ;;
    esac
    sleep 1
  done
  die "database $db_id is still $s after 120 s"
}

# branch_field <id> <jq field>: from the branch list (bare array); empty when the branch is not listed.
branch_field() {
  ok GET "/v1/databases/$db_id/branches" 200
  jq -r --arg id "$1" ".[] | select(.id == \$id) | .$2 // empty" "$tmp/body"
}
# wait_branch <id> <status>: until the branch has that status (a failed branch dies, unless it is the wanted one).
wait_branch() {
  local s=""
  for i in $(seq 180); do
    s="$(branch_field "$1" status)"
    [[ "$s" != "$2" ]] || return 0
    [[ "$s" != failed ]] || die "branch $1 failed: $(branch_field "$1" error)"
    sleep 1
  done
  die "branch $1 is '$s' after 180 s (wanted $2)"
}
# wait_gone <id>: until the branch is deleted or no longer listed.
wait_gone() {
  local s=""
  for i in $(seq 120); do
    s="$(branch_field "$1" status)"
    [[ -z "$s" || "$s" == deleted ]] && return 0
    sleep 1
  done
  die "branch $1 is still '$s' after 120 s"
}

# create_branch <name> <parent id> [at json]: prints the new branch's id once it is ready.
create_branch() {
  local body id
  body="$(jq -nc --arg n "$1" --arg p "$2" --argjson at "${3:-null}" '{name: $n, parent_branch_id: $p} + (if $at then {at: $at} else {} end)')"
  ok POST "/v1/databases/$db_id/branches" 202 "$body"
  id="$(jq -r '.id' "$tmp/body")"
  wait_branch "$id" ready
  echo "$id"
}

# reveal <branch id>: sets bhost, bpw and bdb from the branch's connection details.
reveal() {
  ok GET "/v1/databases/$db_id/branches/$1/connection" 200
  bhost="$(jq -r '.host' "$tmp/body")"
  bpw="$(jq -r '.password' "$tmp/body")"
  bdb="$(jq -r '.database' "$tmp/body")"
  [[ "${bhost%%.*}" =~ ^ep-[0-9a-f]{12}$ ]] || die "unexpected host '$bhost' for branch $1"
  echo "${bhost%%.*}" >>"$tmp/endpoints" # a file: reveal often runs in a subshell
}
# psql_on <branch id> <psql args…>: through the proxy, with the branch's current password; retries
# while the compute is still starting.
psql_on() {
  local id="$1" out
  shift
  reveal "$id"
  for i in $(seq 60); do
    if out="$(PGPASSWORD="$bpw" PGCONNECT_TIMEOUT=10 psql \
      "host=$bhost hostaddr=$PROXY_HOSTADDR port=${PROXY_PG_PORT} user=postgres dbname=$bdb sslmode=verify-full sslrootcert=$ca" \
      -v ON_ERROR_STOP=1 -qtA "$@" 2>"$tmp/err")"; then
      printf '%s' "$out"
      return
    fi
    sleep 2
  done
  die "psql on branch $id: $(cat "$tmp/err")"
}
# rows <branch id>: the values of t, sorted and comma separated.
rows() { psql_on "$1" -c "SELECT coalesce(string_agg(v, ',' ORDER BY v), '') FROM t"; }
# expect_rows <branch id> <wanted> <what>
expect_rows() {
  local got
  got="$(rows "$1")"
  [[ "$got" == "$2" ]] || die "$3: rows are '$got', wanted '$2'"
}
now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

# 1. A database with main; rows A, then B two seconds after the noted time.
name="e2e_$(hex 2)"
status="$(api POST /v1/databases "$(jq -nc --arg t "$DEMO_TENANT_ID" --arg p "$DEMO_PROJECT_ID" --arg n "$name" \
  '{tenant_id: $t, project_id: $p, name: $n}')")"
[[ "$status" == 202 || "$status" == 201 ]] || die "POST /v1/databases: $status $(cat "$tmp/body")"
db_id="$(jq -r '.database_id' "$tmp/body")"
[[ -n "$db_id" && "$db_id" != null ]] || die "POST /v1/databases returned no database_id: $(cat "$tmp/body")"
wait_db
main_id="$db_id"
pass "database $name ($db_id) is ready"
psql_on "$main_id" -c "CREATE TABLE t (v text); INSERT INTO t VALUES ('A');" >/dev/null
sleep 2
noted="$(now)"
sleep 2
psql_on "$main_id" -c "INSERT INTO t VALUES ('B');" >/dev/null
expect_rows "$main_id" A,B "main"
pass "main has A and B (noted time $noted)"

# 2. A branch at the noted time has A and not B.
dev_id="$(create_branch dev "$main_id" "$(jq -nc --arg t "$noted" '{timestamp: $t}')")"
expect_rows "$dev_id" A "the branch at the noted time"
[[ "$(branch_field "$dev_id" parent_branch_id)" == "$main_id" ]] || die "dev's parent is not main"
pass "a branch at $noted has A and not B"

# 3. C on main; reset the branch to its parent: A, B and C.
psql_on "$main_id" -c "INSERT INTO t VALUES ('C');" >/dev/null
ok POST "/v1/databases/$db_id/branches/$dev_id/reset-to-parent" 202
wait_branch "$dev_id" ready
expect_rows "$dev_id" A,B,C "the branch after reset-to-parent"
pass "reset-to-parent brought the branch to A,B,C"

# A parent with a live child cannot be restored.
ok POST "/v1/databases/$db_id/branches/$main_id/restore" 409 "$(jq -nc --arg t "$noted" '{at: {timestamp: $t}}')"
ok DELETE "/v1/databases/$db_id/branches/$dev_id" 202
wait_gone "$dev_id"

# 4. Restore main to the noted time: A only; the backup has no compute and a branch of it has B and C.
ok POST "/v1/databases/$db_id/branches/$main_id/restore" 202 "$(jq -nc --arg t "$noted" '{at: {timestamp: $t}}')"
backup_id="$(jq -r '.backup_branch_id' "$tmp/body")"
backup_name="$(jq -r '.backup_name' "$tmp/body")"
[[ -n "$backup_id" && "$backup_id" != null ]] || die "restore returned no backup_branch_id: $(cat "$tmp/body")"
[[ "$backup_name" =~ ^main-before-restore-[0-9]{8}T[0-9]{6}Z$ ]] ||
  die "unexpected backup name '$backup_name'"
wait_branch "$main_id" ready
wait_branch "$backup_id" ready
expect_rows "$main_id" A "main after restore"
[[ "$(branch_field "$backup_id" kind)" == backup ]] || die "the backup's kind is not 'backup'"
[[ -z "$(branch_field "$backup_id" host)" ]] || die "the backup has a host (a compute)"
ok GET "/v1/databases/$db_id/branches/$backup_id/connection" 409
fb_id="$(create_branch from-backup "$backup_id")"
expect_rows "$fb_id" A,B,C "the branch of the backup"
pass "restore left main at A; backup $backup_name has no compute; its branch has A,B,C"

# 5. A point outside the history window (window 1 h, point 2 h back). The point is older than
# every commit of this young tenant, so Neon answers "past" whatever window it has applied yet:
# one attempt, no race with the settings job.
ok PATCH "/v1/databases/$db_id" 200 '{"history_window": 3600}'
old="$(date -u -d '2 hours ago' +%Y-%m-%dT%H:%M:%SZ)"
ok POST "/v1/databases/$db_id/branches" 202 \
  "$(jq -nc --arg p "$main_id" --arg t "$old" '{name: "too-old", parent_branch_id: $p, at: {timestamp: $t}}')"
bid="$(jq -r '.id' "$tmp/body")"
s=""
for _ in $(seq 60); do
  s="$(branch_field "$bid" status)"
  [[ "$s" == failed || "$s" == ready ]] && break
  sleep 1
done
err="$(branch_field "$bid" error)"
echo "     too-old: $s: $err"
[[ "$s" == failed ]] || die "the branch 2 h back is '$s', wanted failed"
rfc3339='[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]+)?Z'
[[ "$err" =~ ^"resolving the point: outside the history window; the oldest available point is "$rfc3339$ ]] ||
  die "unexpected error for a point 2 h back: $err"
pass "a point 2 h back is refused: $err"

# 6. Delete everything: computes, tenant, endpoint rows and branch rows.
tenant="$(metadata_sql -v id="$db_id" <<<"SELECT neon_tenant_id FROM databases WHERE id = :'id';")"
[[ "$tenant" =~ ^[0-9a-f]{32}$ ]] || die "no tenant for database $db_id (got '$tenant')"
ok DELETE "/v1/databases/$db_id" 202
for i in $(seq 120); do
  status="$(api GET "/v1/databases/$db_id")"
  [[ "$status" != 404 ]] || break
  [[ $i -lt 120 ]] || die "database still answers ($status) 120 s after DELETE"
  sleep 1
done
db_id=""
for ep in $(sort -u "$tmp/endpoints"); do
  [[ -z "$(docker ps -aq --filter "label=chelabase.dev/stack=$CHELABASE_STACK" --filter "label=chelabase.dev/endpoint-id=$ep")" ]] || die "the compute container of endpoint $ep still exists"
  [[ "$(metadata_sql -v ep="$ep" <<<"SELECT count(*) FROM neon_endpoints WHERE id = :'ep';")" == 0 ]] ||
    die "endpoint $ep still has a row in neon_endpoints"
done
status="$(curl -sS --max-time 30 -o /dev/null -w '%{http_code}' "$STORCON/control/v1/tenant/$tenant")" ||
  die "the storage controller didn't answer"
[[ "$status" == 404 ]] || die "tenant $tenant (and its timelines) still exists (HTTP $status)"
left="$(metadata_sql -v id="$main_id" <<<"SELECT count(*) FROM branches WHERE database_id = :'id' AND status <> 'deleted';")"
[[ "$left" == 0 ]] || die "$left branch row(s) are not deleted"
pass "delete removed $(sort -u "$tmp/endpoints" | wc -l) compute(s), tenant $tenant and every branch"
echo "all passed"
