#!/usr/bin/env bash
# Live check that deletes reclaim safekeeper storage (needs the stack: just workbench up). Safekeeper
# timelines are created by the storage controller (--timelines-onto-safekeepers), so deleting a
# database, a branch or a timeline superseded by reset-to-parent deletes the safekeeper's copy too:
# 1. a new database has its tenant/timeline on every safekeeper (safekeeper1..3: the controller places it on all three);
# 2. reset-to-parent removes the branch's old timeline, keeps the new one and main's;
# 3. deleting a branch removes only its timeline;
# 4. restore keeps main's new timeline and the backup's old one;
# 5. deleting the backup branch removes only its timeline;
# 6. deleting the database removes the tenant's directory and its S3 prefix, and the storage
#    controller forgets the tenant: storcon-db keeps no timelines or
#    safekeeper_timeline_pending_ops rows for it.
# "Removed" means: every safekeeper answers 404 for it, /data/<tenant>/<timeline> is gone on each and no object
# is left under s3://neon/safekeeper/<tenant>/<timeline>/, within 60 s.
# S3 uses marker objects: the safekeeper uploads a short-lived timeline's WAL only after
# --partial-backup-timeout (15 min), so the prefix is empty in a short run. Before each delete a
# marker is written under the timeline (and under every timeline that must stay) and seen; the
# delete must remove it (the controller's delete reaches the safekeeper without only_local, which
# removes local files and the S3 prefix: fork safekeeper/src/http/routes.rs:74-94,222-240 and
# storage_controller/src/service/safekeeper_reconciler.rs:403,417); the stayers' markers must stay.
# KEEP=1 leaves the database in place.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=storage-leak-e2e
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
GATEWAY="${GATEWAY_URL:-$CHELABASE_ENTRY_API_URL}"
PROXY_PG_PORT="${CHELABASE_PROXY_PG_PORT:-4432}"
PROXY_HOSTADDR="${CHELABASE_PROXY_HOSTADDR:?run through just workbench run}"
DEMO_TENANT_ID="00000000-0000-4000-8000-000000000001"
DEMO_PROJECT_ID="00000000-0000-4000-8000-000000000002"
tmp="$(mktemp -d)"
db_id=""
t0=$SECONDS

pass() { echo "PASS $* ($((SECONDS - t0)) s)"; }
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
ca="${CHELABASE_PROXY_CA_FILE:?run through just workbench run}"
[[ -s "$ca" ]] || die "$ca is missing (run just workbench up)"
trap cleanup EXIT

ok() {
  local status
  status="$(api "$1" "$2" "${4:-}")"
  [[ "$status" == "$3" ]] || die "$1 $2: $status (wanted $3) $(cat "$tmp/body")"
}
wait_db() {
  local s=""
  for _ in $(seq 120); do
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
branch_field() {
  ok GET "/v1/databases/$db_id/branches" 200
  jq -r --arg id "$1" ".[] | select(.id == \$id) | .$2 // empty" "$tmp/body"
}
wait_branch() {
  local s=""
  for _ in $(seq 180); do
    s="$(branch_field "$1" status)"
    [[ "$s" != "$2" ]] || return 0
    [[ "$s" != failed ]] || die "branch $1 failed: $(branch_field "$1" error)"
    sleep 1
  done
  die "branch $1 is '$s' after 180 s (wanted $2)"
}
wait_gone() {
  local s=""
  for _ in $(seq 120); do
    s="$(branch_field "$1" status)"
    [[ -z "$s" || "$s" == deleted ]] && return 0
    sleep 1
  done
  die "branch $1 is still '$s' after 120 s"
}
create_branch() {
  local body id
  body="$(jq -nc --arg n "$1" --arg p "$2" '{name: $n, parent_branch_id: $p}')"
  ok POST "/v1/databases/$db_id/branches" 202 "$body"
  id="$(jq -r '.id' "$tmp/body")"
  wait_branch "$id" ready
  echo "$id"
}
# psql_on <branch id> <sql>: through the proxy (starts the compute, which then writes WAL).
psql_on() {
  local id="$1" bhost bpw bdb
  ok GET "/v1/databases/$db_id/branches/$id/connection" 200
  bhost="$(jq -r '.host' "$tmp/body")"
  bpw="$(jq -r '.password' "$tmp/body")"
  bdb="$(jq -r '.database' "$tmp/body")"
  for _ in $(seq 60); do
    if PGPASSWORD="$bpw" PGCONNECT_TIMEOUT=10 psql \
      "host=$bhost hostaddr=$PROXY_HOSTADDR port=${PROXY_PG_PORT} user=postgres dbname=$bdb sslmode=verify-full sslrootcert=$ca" \
      -v ON_ERROR_STOP=1 -qtA -c "$2" >/dev/null 2>"$tmp/err"; then
      return
    fi
    sleep 2
  done
  die "psql on branch $id: $(cat "$tmp/err")"
}
# timeline_of <branch id>: its current Neon timeline id.
timeline_of() { metadata_sql -v id="$1" <<<"SELECT timeline_id FROM branches WHERE id = :'id';"; }

# storcon_sql <psql args>: the storage controller's database.
storcon_sql() { docker exec -i "$CHELABASE_COMPOSE_PROJECT-storcon-db-1" psql -U storcon -d storage_controller -v ON_ERROR_STOP=1 -qtA "$@"; }
# wait_storcon_clean <tenant>: until storcon-db has no timelines and no pending-op rows for the
# tenant, 60 s at most. A tenant delete op left behind (timeline_id '') keeps the controller from
# removing the timelines rows (fork storage_controller/src/service/safekeeper_reconciler.rs, `delete_timeline_from_db`).
wait_storcon_clean() {
  local tls="" ops=""
  for _ in $(seq 60); do
    tls="$(storcon_sql -v t="$1" <<<"SELECT count(*) FROM timelines WHERE tenant_id = :'t';")" || die "querying storcon-db failed"
    ops="$(storcon_sql -v t="$1" <<<"SELECT count(*) FROM safekeeper_timeline_pending_ops WHERE tenant_id = :'t';")" || die "querying storcon-db failed"
    if [[ "$tls" == 0 && "$ops" == 0 ]]; then
      pass "database delete: storcon-db has no timelines or pending ops for tenant $1"
      return 0
    fi
    sleep 1
  done
  die "database delete: storcon-db still has $tls timelines row(s) and $ops safekeeper_timeline_pending_ops row(s) for tenant $1 after 60 s"
}

# --- the safekeeper and SeaweedFS views ---
# RT1 (storage auth, user 2026-10-16): the stack runs three safekeepers and the storage controller
# places every timeline on all three (--timeline-safekeeper-count=3), so each check below covers
# every one of them: a timeline left on safekeeper2 or safekeeper3 is a leak too.
SAFEKEEPERS=(safekeeper1 safekeeper2 safekeeper3)
# sk <safekeeper> <cmd…>: run in that safekeeper's container.
sk() {
  local name="$1"
  shift
  docker exec -i "$CHELABASE_COMPOSE_PROJECT-$name-1" "$@"
}
# sk_curl <safekeeper> <curl args>: curl in that safekeeper with its own NeonJWT (its HTTP API
# checks one).
# shellcheck disable=SC2016 # expanded inside the container
sk_curl() {
  local name="$1"
  shift
  sk "$name" sh -c 'exec curl -H "Authorization: Bearer $(cat /run/safekeeper-data.jwt)" "$@"' _ "$@"
}
# sk_status <safekeeper> <tenant> <timeline>: that safekeeper's HTTP status for the timeline.
sk_status() { sk_curl "$1" -s -o /dev/null -w '%{http_code}' "http://localhost:7676/v1/tenant/$2/timeline/$3"; }
# sk_has <tenant> <timeline>: every safekeeper lists the timeline.
sk_has() {
  local s
  for s in "${SAFEKEEPERS[@]}"; do [[ "$(sk_status "$s" "$1" "$2")" == 200 ]] || return 1; done
}
# sk_missing <tenant> <timeline>: the safekeepers that don't list it (for messages).
sk_missing() {
  local s out=()
  for s in "${SAFEKEEPERS[@]}"; do [[ "$(sk_status "$s" "$1" "$2")" == 200 ]] || out+=("$s"); done
  echo "${out[*]}"
}
# sk_gone <tenant> <timeline>: every safekeeper's API answers 404 (a failed call or 000 is not "gone").
sk_gone() {
  local s
  for s in "${SAFEKEEPERS[@]}"; do [[ "$(sk_status "$s" "$1" "$2")" == 404 ]] || return 1; done
}
# sk_tenant_gone <tenant>: no safekeeper lists a timeline of the tenant.
sk_tenant_gone() {
  local s
  for s in "${SAFEKEEPERS[@]}"; do
    [[ "$(sk_curl "$s" -sf "http://localhost:7676/v1/tenant/timeline" | jq -r --arg t "$1" '[.[] | select(.tenant_id == $t)] | length')" == 0 ]] || return 1
  done
}
# sk_dir_gone <path under /data>: on every safekeeper the exec worked and the path is absent.
sk_dir_gone() {
  local s
  for s in "${SAFEKEEPERS[@]}"; do
    [[ "$(sk "$s" sh -c 'test -e "$1" && echo y || echo n' _ "/data/$1")" == n ]] || return 1
  done
}
# aws_sk <args>: the AWS CLI in the safekeeper container (it has the S3 credentials).
aws_sk() { docker exec -i "$CHELABASE_COMPOSE_PROJECT-safekeeper1-1" aws --endpoint-url http://seaweedfs:8333 "$@"; }
# s3_ls <path under safekeeper/>: the objects under that prefix (empty when none); dies when
# the call itself fails (the CLI exits 1 with no message for an empty prefix).
s3_ls() {
  local out rc=0
  out="$(aws_sk s3 ls "s3://neon/safekeeper/$1/" --recursive 2>"$tmp/s3err")" || rc=$?
  if [[ $rc != 0 && ($rc != 1 || -s "$tmp/s3err") ]]; then die "s3 ls $1 failed ($rc): $(cat "$tmp/s3err")"; fi
  printf '%s' "$out"
}
# put_marker <what> <tenant> <timeline>: writes a marker object under the timeline and sees it.
put_marker() {
  echo marker | aws_sk s3 cp - "s3://neon/safekeeper/$2/$3/marker" >/dev/null || die "$1: writing the S3 marker failed"
  [[ "$(s3_ls "$2/$3")" == *marker* ]] || die "$1: the S3 marker under $2/$3 is not listed"
  pass "$1: S3 marker under $2/$3 is listed"
}
# marker_stays <what> <tenant> <timeline>
marker_stays() {
  [[ "$(s3_ls "$2/$3")" == *marker* ]] || die "$1: the S3 marker under $2/$3 is gone"
  pass "$1: S3 marker under $2/$3 is still there"
}
# wait_present <tenant> <timeline>: until every safekeeper lists it (the controller placed it),
# 60 s at most.
wait_present() {
  for _ in $(seq 60); do
    sk_has "$1" "$2" && return 0
    sleep 1
  done
  die "$1/$2 is not on $(sk_missing "$1" "$2") after 60 s (the controller places it on all three)"
}
# wait_removed <what> <tenant> <timeline>: until the timeline (or, with an empty timeline, the whole
# tenant) is gone from every safekeeper's API and disk and from S3, 60 s at most.
wait_removed() {
  local what="$1" tenant="$2" tl="$3" path="$2" api_gone dir_gone s3_gone out
  [[ -z "$tl" ]] || path="$2/$3"
  for _ in $(seq 60); do
    api_gone=yes dir_gone=yes s3_gone=yes
    if [[ -n "$tl" ]]; then
      sk_gone "$tenant" "$tl" || api_gone=no
    else
      sk_tenant_gone "$tenant" || api_gone=no
    fi
    sk_dir_gone "$path" || dir_gone=no
    out="$(s3_ls "$path")"
    [[ -z "$out" ]] || s3_gone=no
    if [[ "$api_gone$dir_gone$s3_gone" == yesyesyes ]]; then
      pass "$what: removed from ${SAFEKEEPERS[*]} (API, /data/$path) and S3"
      return 0
    fi
    sleep 1
  done
  die "$what: still there after 60 s (gone from every safekeeper's API: $api_gone, /data/$path gone from every safekeeper: $dir_gone, S3 empty: $s3_gone)"
}
# stays <what> <tenant> <timeline>
stays() {
  sk_has "$2" "$3" || die "$1: $(sk_missing "$2" "$3") lost $2/$3"
  pass "$1: $2/$3 is still on ${SAFEKEEPERS[*]}"
  marker_stays "$1" "$2" "$3"
}

# 1. A database: main's timeline is on every safekeeper.
for s in "${SAFEKEEPERS[@]}"; do sk "$s" true || die "cannot exec in $s"; done
name="e2e_$(hex 2)"
status="$(api POST /v1/databases "$(jq -nc --arg t "$DEMO_TENANT_ID" --arg p "$DEMO_PROJECT_ID" --arg n "$name" \
  '{tenant_id: $t, project_id: $p, name: $n}')")"
[[ "$status" == 202 || "$status" == 201 ]] || die "POST /v1/databases: $status $(cat "$tmp/body")"
db_id="$(jq -r '.database_id' "$tmp/body")"
[[ -n "$db_id" && "$db_id" != null ]] || die "POST /v1/databases returned no database_id: $(cat "$tmp/body")"
wait_db
main_id="$db_id"
tenant="$(metadata_sql -v id="$db_id" <<<"SELECT neon_tenant_id FROM databases WHERE id = :'id';")"
main_tl="$(timeline_of "$main_id")"
[[ "$tenant" =~ ^[0-9a-f]{32}$ && "$main_tl" =~ ^[0-9a-f]{32}$ ]] || die "no tenant/timeline ids ($tenant, $main_tl)"
psql_on "$main_id" "CREATE TABLE t (v text); INSERT INTO t VALUES ('A');"
wait_present "$tenant" "$main_tl"
pass "database $name: main's timeline $tenant/$main_tl is on ${SAFEKEEPERS[*]}"
put_marker "main" "$tenant" "$main_tl"

# 2. A branch with its own timeline; reset-to-parent supersedes and deletes the old one.
dev_id="$(create_branch dev "$main_id")"
psql_on "$dev_id" "INSERT INTO t VALUES ('B');"
dev_tl="$(timeline_of "$dev_id")"
wait_present "$tenant" "$dev_tl"
pass "branch dev: timeline $dev_tl is on ${SAFEKEEPERS[*]}"
put_marker "branch dev" "$tenant" "$dev_tl"
ok POST "/v1/databases/$db_id/branches/$dev_id/reset-to-parent" 202
wait_branch "$dev_id" ready
dev_tl2="$(timeline_of "$dev_id")"
[[ "$dev_tl2" != "$dev_tl" ]] || die "reset-to-parent kept timeline $dev_tl"
psql_on "$dev_id" "SELECT 1;"
wait_present "$tenant" "$dev_tl2"
wait_removed "reset-to-parent: the superseded timeline $dev_tl" "$tenant" "$dev_tl"
stays "reset-to-parent: main" "$tenant" "$main_tl"

# 3. Deleting a branch removes only its timeline.
noted="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
sleep 2
psql_on "$main_id" "INSERT INTO t VALUES ('C');"
put_marker "branch dev (after reset)" "$tenant" "$dev_tl2"
ok DELETE "/v1/databases/$db_id/branches/$dev_id" 202
wait_gone "$dev_id"
wait_removed "branch delete: dev's timeline $dev_tl2" "$tenant" "$dev_tl2"
stays "branch delete: main" "$tenant" "$main_tl"

# 4. Restore main: the new main timeline and the backup's (old) timeline both stay.
ok POST "/v1/databases/$db_id/branches/$main_id/restore" 202 "$(jq -nc --arg t "$noted" '{at: {timestamp: $t}}')"
backup_id="$(jq -r '.backup_branch_id' "$tmp/body")"
wait_branch "$main_id" ready
wait_branch "$backup_id" ready
psql_on "$main_id" "SELECT 1;"
new_main_tl="$(timeline_of "$main_id")"
backup_tl="$(timeline_of "$backup_id")"
wait_present "$tenant" "$new_main_tl"
put_marker "restore: main's new timeline" "$tenant" "$new_main_tl"
stays "restore: the backup's timeline" "$tenant" "$backup_tl"
pass "restore: main moved to $new_main_tl, backup holds $backup_tl"

# 5. Deleting the backup branch removes only its timeline.
ok DELETE "/v1/databases/$db_id/branches/$backup_id" 202
wait_gone "$backup_id"
wait_removed "branch delete: the backup's timeline $backup_tl" "$tenant" "$backup_tl"
stays "branch delete: main" "$tenant" "$new_main_tl"

# 6. Deleting the database removes the tenant's directory and S3 prefix (the markers go too).
ok DELETE "/v1/databases/$db_id" 202
for i in $(seq 120); do
  status="$(api GET "/v1/databases/$db_id")"
  [[ "$status" != 404 ]] || break
  [[ $i -lt 120 ]] || die "database still answers ($status) 120 s after DELETE"
  sleep 1
done
db_id=""
wait_removed "database delete: tenant $tenant" "$tenant" ""
wait_storcon_clean "$tenant"
echo "all passed ($((SECONDS - t0)) s)"
