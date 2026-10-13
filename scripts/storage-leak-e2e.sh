#!/usr/bin/env bash
# Live check that deletes reclaim safekeeper storage (needs just deps-up and just dev). Safekeeper
# timelines are created by the storage controller (--timelines-onto-safekeepers), so deleting a
# database, a branch or a timeline superseded by reset-to-parent deletes the safekeeper's copy too:
# 1. a new database has its tenant/timeline on safekeeper1;
# 2. reset-to-parent removes the branch's old timeline, keeps the new one and main's;
# 3. deleting a branch removes only its timeline;
# 4. restore keeps main's new timeline and the backup's old one;
# 5. deleting the backup branch removes only its timeline;
# 6. deleting the database removes the tenant's directory and its S3 prefix, and the storage
#    controller forgets the tenant: storcon-db keeps no timelines or
#    safekeeper_timeline_pending_ops rows for it.
# "Removed" means: safekeeper1 answers 404 for it, /data/<tenant>/<timeline> is gone and no object
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

cargo build -q -p chelabase || die "cargo build -p chelabase"
[[ -f .chelabase/zitadel/demo.env ]] || die ".chelabase/zitadel/demo.env is missing (run just dev)"
set -a
# shellcheck disable=SC1091
source .chelabase/zitadel/demo.env
set +a
ca="$root/.chelabase/neon/proxy-ca.crt"
[[ -s "$ca" ]] || die "$ca is missing (run just deps-up)"
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
      "host=$bhost hostaddr=127.0.0.1 port=${PROXY_PG_PORT} user=postgres dbname=$bdb sslmode=verify-full sslrootcert=$ca" \
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
storcon_sql() { "${compose[@]}" exec -T storcon-db psql -U storcon -d storage_controller -v ON_ERROR_STOP=1 -qtA "$@"; }
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
sk() { "${compose[@]}" exec -T safekeeper1 "$@"; }
# sk_has <tenant> <timeline>: safekeeper1 lists the timeline.
sk_has() {
  [[ "$(sk curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:7676/v1/tenant/$1/timeline/$2")" == 200 ]]
}
# sk_gone <tenant> <timeline>: the safekeeper API answers 404 (a failed call or 000 is not "gone").
sk_gone() {
  [[ "$(sk curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:7676/v1/tenant/$1/timeline/$2")" == 404 ]]
}
# sk_dir_has <path under /data>
sk_dir_has() { [[ "$(sk sh -c 'test -e "$1" && echo y || echo n' _ "/data/$1")" == y ]]; }
# sk_dir_gone <path under /data>: the exec worked and the path is absent.
sk_dir_gone() { [[ "$(sk sh -c 'test -e "$1" && echo y || echo n' _ "/data/$1")" == n ]]; }
# aws_sk <args>: the AWS CLI in the safekeeper container (it has the S3 credentials).
aws_sk() { "${compose[@]}" exec -T safekeeper1 aws --endpoint-url http://seaweedfs:8333 "$@"; }
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
# wait_present <tenant> <timeline>: until safekeeper1 lists it (the compute created or the
# controller placed it), 60 s at most.
wait_present() {
  for _ in $(seq 60); do
    sk_has "$1" "$2" && return 0
    sleep 1
  done
  die "safekeeper1 does not list $1/$2 after 60 s"
}
# wait_removed <what> <tenant> <timeline>: until the timeline (or, with an empty timeline, the whole
# tenant) is gone from safekeeper1's API and disk and from S3, 60 s at most.
wait_removed() {
  local what="$1" tenant="$2" tl="$3" path="$2" api_gone dir_gone s3_gone out
  [[ -z "$tl" ]] || path="$2/$3"
  for _ in $(seq 60); do
    api_gone=yes dir_gone=yes s3_gone=yes
    if [[ -n "$tl" ]]; then
      sk_gone "$tenant" "$tl" || api_gone=no
    else
      [[ "$(sk curl -sf "http://127.0.0.1:7676/v1/tenant/timeline" | jq -r --arg t "$tenant" '[.[] | select(.tenant_id == $t)] | length')" == 0 ]] || api_gone=no
    fi
    sk_dir_gone "$path" || dir_gone=no
    out="$(s3_ls "$path")"
    [[ -z "$out" ]] || s3_gone=no
    if [[ "$api_gone$dir_gone$s3_gone" == yesyesyes ]]; then
      pass "$what: removed from safekeeper1 (API, /data/$path, S3)"
      return 0
    fi
    sleep 1
  done
  die "$what: still on safekeeper1 after 60 s (api gone: $api_gone, /data/$path gone: $dir_gone, S3 empty: $s3_gone)"
}
# stays <what> <tenant> <timeline>
stays() {
  sk_has "$2" "$3" || die "$1: safekeeper1 lost $2/$3"
  pass "$1: $2/$3 is still on safekeeper1"
  marker_stays "$1" "$2" "$3"
}

# 1. A database: main's timeline is on safekeeper1.
sk true || die "cannot exec in safekeeper1"
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
pass "database $name: main's timeline $tenant/$main_tl is on safekeeper1"
put_marker "main" "$tenant" "$main_tl"

# 2. A branch with its own timeline; reset-to-parent supersedes and deletes the old one.
dev_id="$(create_branch dev "$main_id")"
psql_on "$dev_id" "INSERT INTO t VALUES ('B');"
dev_tl="$(timeline_of "$dev_id")"
wait_present "$tenant" "$dev_tl"
pass "branch dev: timeline $dev_tl is on safekeeper1"
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
