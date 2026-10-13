#!/usr/bin/env bash
# Live check of roadmap step 6d (Studio's SQL editor API), through the gateway (needs just
# deps-up and just dev). Streams are NDJSON read with curl -N:
# - a script as owner (CREATE, INSERT, SELECT: tags and a clean done), then as anon under a row
#   level security policy (the first run grants the role to the owner, the second is fast);
# - a notice, an error with a position;
# - cancel of a pg_sleep through the cancel route, and by closing the stream through the gateway;
# - a 75 s pg_sleep (the stream stays open past the gateway's 60 s idle limit thanks to heartbeats);
# - the guard: DROP needs confirm, a protected branch holds writes;
# - saved queries (create, list, get, a stale PATCH, delete) and the schema document.
# KEEP=1 leaves the database in place.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=sql-editor-e2e
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
GATEWAY="${GATEWAY_URL:-$CHELABASE_ENTRY_API_URL}"
DEMO_TENANT_ID="00000000-0000-4000-8000-000000000001"
DEMO_PROJECT_ID="00000000-0000-4000-8000-000000000002"
tmp="$(mktemp -d)"
db_id=""
bg_pid=""
t0=$SECONDS
step=$SECONDS

pass() {
  echo "PASS $* ($((SECONDS - step)) s, $((SECONDS - t0)) s total)"
  step=$SECONDS
}
die() {
  echo "FAIL $*" >&2
  exit 1
}
cleanup() {
  [[ -z "$bg_pid" ]] || kill "$bg_pid" 2>/dev/null || true
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
trap cleanup EXIT

# ok <method> <path> <expected status> [body]: the call, or die.
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

# stream <out file> <max seconds> <json body>: POSTs the run; the body goes to the file and
# the status is printed. The token goes through stdin (-H @-), never argv.
stream() {
  local out="$1" max="$2" body="$3" token
  token="$(cargo run -q -p chelabase -- dev token)" || die "chelabase dev token"
  printf 'Authorization: Bearer %s\n' "$token" |
    curl -sSN --max-time "$max" -o "$out" -w '%{http_code}' -H @- -H 'content-type: application/json' \
      -X POST -d "$body" "$GATEWAY$branch/sql" || true
}

# run_sql <jq filter on the events array, true to pass> <extra JSON fields…>: runs a script
# (from $SQL) with a fresh run id; checks a 200 whose events (jq -s) satisfy the filter.
# $RUN_MAX is the curl limit (default 60). Events end up in $tmp/events.
run_sql() {
  local filter="$1" extra="${2:-{\}}" body status
  body="$(jq -nc --arg id "$(cat /proc/sys/kernel/random/uuid)" --arg sql "$SQL" --argjson x "$extra" \
    '{run_id: $id, sql: $sql} + $x')"
  status="$(stream "$tmp/events" "${RUN_MAX:-60}" "$body")"
  [[ "$status" == 200 ]] || die "run '$SQL': $status $(cat "$tmp/events")"
  jq -s -e "$filter" "$tmp/events" >/dev/null ||
    die "run '$SQL': events fail $filter: $(jq -sc . "$tmp/events")"
}

# 1. A database.
name="sql_editor_e2e_$(hex 2)"
status="$(api POST /v1/databases "$(jq -nc --arg t "$DEMO_TENANT_ID" --arg p "$DEMO_PROJECT_ID" --arg n "$name" \
  '{tenant_id: $t, project_id: $p, name: $n}')")"
[[ "$status" == 202 || "$status" == 201 ]] || die "POST /v1/databases: $status $(cat "$tmp/body")"
db_id="$(jq -r '.database_id' "$tmp/body")"
[[ -n "$db_id" && "$db_id" != null ]] || die "POST /v1/databases returned no database_id: $(cat "$tmp/body")"
branch="/v1/databases/$db_id/branches/$db_id"
wait_db
pass "database $name ($db_id) is ready"

# 2. A script as the owner: tags and a clean done.
SQL="CREATE TABLE notes(id int primary key, body text); INSERT INTO notes VALUES (1,'a'),(2,'b'); SELECT * FROM notes"
for _ in $(seq 30); do # the compute may still be starting: the first run can fail to connect
  status="$(stream "$tmp/events" 60 "$(jq -nc --arg id "$(cat /proc/sys/kernel/random/uuid)" --arg sql "$SQL" '{run_id: $id, sql: $sql}')")"
  [[ "$status" == 200 ]] && break
  sleep 2
done
[[ "$status" == 200 ]] || die "first run: $status $(cat "$tmp/events")"
jq -s -e '[.[] | select(.t == "complete") | .tag] == ["CREATE TABLE","INSERT 0 2","SELECT 2"]
  and (.[-1] | .t == "done" and .outcome == "ok")' "$tmp/events" >/dev/null ||
  die "owner script: $(jq -sc . "$tmp/events")"
pass "owner script: CREATE TABLE, INSERT 0 2, SELECT 2, done ok"

# 3. As anon under RLS (the first anon run grants the role to the owner).
SQL="ALTER TABLE notes ENABLE ROW LEVEL SECURITY; GRANT SELECT ON notes TO anon;
  CREATE POLICY anon_read ON notes FOR SELECT TO anon USING (id = 1)"
run_sql '.[-1].outcome == "ok"'
SQL="SELECT count(*) FROM notes"
run_sql '(.[-1].outcome == "ok") and ([.[] | select(.t == "rows") | .rows[0][0]] == ["1"])' '{"role":"anon"}'
SQL="SELECT count(*) FROM notes"
t1=$SECONDS
run_sql '(.[-1].outcome == "ok") and ([.[] | select(.t == "rows") | .rows[0][0]] == ["1"])' '{"role":"anon"}'
((SECONDS - t1 <= 5)) || die "the second anon run took $((SECONDS - t1)) s (the grant should be cached)"
SQL="SELECT count(*) FROM notes"
run_sql '[.[] | select(.t == "rows") | .rows[0][0]] == ["2"]'
pass "anon sees 1 row under the policy (the owner sees 2); the second anon run was fast"

# 4. A notice.
SQL="DO \$\$ BEGIN RAISE NOTICE 'hello'; END \$\$"
run_sql 'any(.[]; .t == "notice" and .message == "hello") and (.[-1].outcome == "ok")'
pass "RAISE NOTICE arrives as a notice event"

# 5. An error with a position.
SQL="SELECT nope"
run_sql 'any(.[]; .t == "error" and (.position | type == "number")) and (.[-1].outcome == "error")'
pass "SELECT nope: an error event with a position, done error"

# 6. Cancel a pg_sleep in flight.
run_id="$(cat /proc/sys/kernel/random/uuid)"
body="$(jq -nc --arg id "$run_id" '{run_id: $id, sql: "SELECT pg_sleep(30)"}')"
stream "$tmp/cancelled" 60 "$body" >"$tmp/cancelled.status" &
bg_pid=$!
sleep 3
ok POST "$branch/sql/runs/$run_id/cancel" 202
wait "$bg_pid" || true
bg_pid=""
[[ "$(cat "$tmp/cancelled.status")" == 200 ]] || die "cancelled run: $(cat "$tmp/cancelled.status") $(cat "$tmp/cancelled")"
jq -s -e '.[-1] | .t == "done" and .outcome == "cancelled"' "$tmp/cancelled" >/dev/null ||
  die "cancelled run's events: $(jq -sc . "$tmp/cancelled")"
ok POST "$branch/sql/runs/$run_id/cancel" 404
pass "cancel: 202, the stream ends cancelled; cancelling again is 404"

# 6b. Closing the stream through the gateway cancels the run and frees its slot: the same
# run id is accepted again within seconds (a leaked slot would answer 409 run_id_in_use).
run_id="$(cat /proc/sys/kernel/random/uuid)"
stream "$tmp/closed" 3 "$(jq -nc --arg id "$run_id" '{run_id: $id, sql: "SELECT pg_sleep(30)"}')" >/dev/null
freed=""
for _ in $(seq 10); do
  status="$(stream "$tmp/closed" 30 "$(jq -nc --arg id "$run_id" '{run_id: $id, sql: "SELECT 1"}')")"
  if [[ "$status" == 200 ]]; then
    freed=1
    break
  fi
  [[ "$status" == 409 && "$(jq -r '.error.code' "$tmp/closed")" == run_id_in_use ]] ||
    die "rerun after closing the stream: $status $(cat "$tmp/closed")"
  sleep 1
done
[[ -n "$freed" ]] || die "closing the stream through the gateway didn't free the run's slot within 10 s"
jq -s -e '.[-1] | .t == "done" and .outcome == "ok"' "$tmp/closed" >/dev/null ||
  die "rerun after closing the stream: $(jq -sc . "$tmp/closed")"
pass "closing the stream through the gateway frees the run (its id is accepted again)"

# 7. A 75 s sleep: the stream outlives the gateway's idle limit (heartbeats keep it open).
SQL="SELECT pg_sleep(75)"
t1=$SECONDS
RUN_MAX=110 run_sql 'any(.[]; .t == "heartbeat") and (.[-1].outcome == "ok")' '{"timeout_ms":120000}'
((SECONDS - t1 >= 70)) || die "the 75 s sleep ended after only $((SECONDS - t1)) s"
pass "a $((SECONDS - t1)) s run stayed open through the gateway, with heartbeats, and ended ok"

# 8. The guard: DROP needs confirm.
SQL="DROP TABLE notes"
status="$(stream "$tmp/held" 60 "$(jq -nc --arg id "$(cat /proc/sys/kernel/random/uuid)" --arg sql "$SQL" '{run_id: $id, sql: $sql}')")"
[[ "$status" == 409 && "$(jq -r '.error.code' "$tmp/held")" == sql_confirm_required ]] ||
  die "DROP without confirm: $status $(cat "$tmp/held")"
[[ "$(jq -r '.error.statements[0].reason' "$tmp/held")" == risky ]] || die "DROP's reason: $(cat "$tmp/held")"
run_sql '.[-1].outcome == "ok"' '{"confirm":true}'
pass "DROP TABLE: 409 sql_confirm_required (risky), then ok with confirm"

# 9. A protected branch holds writes.
SQL="CREATE TABLE guarded(id int)"
run_sql '.[-1].outcome == "ok"'
ok PATCH "$branch" 200 '{"protected":true}'
SQL="INSERT INTO guarded VALUES (1)"
status="$(stream "$tmp/held" 60 "$(jq -nc --arg id "$(cat /proc/sys/kernel/random/uuid)" --arg sql "$SQL" '{run_id: $id, sql: $sql}')")"
[[ "$status" == 409 && "$(jq -r '.error.code' "$tmp/held")" == sql_confirm_required &&
  "$(jq -r '.error.statements[0].reason' "$tmp/held")" == protected ]] ||
  die "INSERT on a protected branch: $status $(cat "$tmp/held")"
SQL="SELECT 1"
run_sql '.[-1].outcome == "ok"' # reads still run
ok PATCH "$branch" 200 '{"protected":false}'
SQL="INSERT INTO guarded VALUES (1)"
run_sql '.[-1].outcome == "ok"'
pass "protected branch: INSERT is 409 (reason protected), SELECT runs; unprotected, INSERT runs"

# 10. Saved queries.
base="/v1/databases/$db_id/sql-queries"
ok POST "$base" 201 '{"name":"First","sql":"SELECT 1","pg_database":null,"role":"anon"}'
qid="$(jq -r '.id' "$tmp/body")"
[[ -n "$qid" && "$qid" != null && "$(jq -r '.version' "$tmp/body")" == 1 ]] || die "create saved: $(cat "$tmp/body")"
ok POST "$base" 409 '{"name":"first","sql":"SELECT 2"}'
[[ "$(jq -r '.error.code' "$tmp/body")" == saved_query_name_taken ]] || die "duplicate name: $(cat "$tmp/body")"
ok GET "$base" 200
jq -e --arg id "$qid" 'map(select(.id == $id)) | length == 1 and (.[0] | has("sql") | not)' "$tmp/body" >/dev/null ||
  die "list saved: $(cat "$tmp/body")"
ok GET "$base/$qid" 200
[[ "$(jq -r '.sql' "$tmp/body")" == "SELECT 1" ]] || die "get saved: $(cat "$tmp/body")"
ok PATCH "$base/$qid" 200 '{"version":1,"sql":"SELECT 2"}'
[[ "$(jq -r '.version' "$tmp/body")" == 2 ]] || die "patch saved: $(cat "$tmp/body")"
ok PATCH "$base/$qid" 409 '{"version":1,"sql":"SELECT 3"}'
[[ "$(jq -r '.error.code' "$tmp/body")" == saved_query_conflict && "$(jq -r '.error.current.version' "$tmp/body")" == 2 ]] ||
  die "stale patch: $(cat "$tmp/body")"
ok DELETE "$base/$qid" 204
ok GET "$base/$qid" 404
pass "saved queries: create, duplicate name 409, list, get, patch, stale patch 409, delete"

# 11. The schema document.
ok GET "$branch/sql/schema" 200
jq -e '(.tables | type == "array" and length > 0) and (.functions | type == "array")' "$tmp/body" >/dev/null ||
  die "schema: $(head -c 400 "$tmp/body")"
pass "schema: $(jq '.tables | length' "$tmp/body") tables, $(jq '.functions | length' "$tmp/body") functions, $(wc -c <"$tmp/body") bytes"

# 12. Delete.
if [[ "${KEEP:-}" != 1 ]]; then
  status="$(api DELETE "/v1/databases/$db_id")"
  [[ "$status" == 202 || "$status" == 204 ]] || die "DELETE /v1/databases/$db_id: $status $(cat "$tmp/body")"
  db_id=""
  pass "database deleted"
fi
echo "all passed ($((SECONDS - t0)) s)"
