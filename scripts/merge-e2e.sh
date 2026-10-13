#!/usr/bin/env bash
# Live check of roadmap step 6g (schema merges), through the public API and the proxy (needs just
# deps-up and just dev):
# - a database and a branch; the schema changes on both sides, with one conflict (a column's
#   default changed on both) and changes only the branch made (a new column, a new table);
# - create merge -> draft with the conflict open and no plan; resolve it (take source) -> a plan;
# - rehearse -> rehearsed (no drift); the rehearsal branch never shows in the branch list;
# - main is protected: apply without confirm is 409 merge_confirm_required; with confirm -> done;
# - main's schema checked with psql through the proxy; the plan's .sql export; the merge's log.
# KEEP=1 leaves the database in place.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=merge-e2e
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
GATEWAY="${GATEWAY_URL:-$CHELABASE_ENTRY_API_URL}"
PROXY_PG_PORT="${CHELABASE_PROXY_PG_PORT:-4432}"
DEMO_TENANT_ID="00000000-0000-4000-8000-000000000001"
DEMO_PROJECT_ID="00000000-0000-4000-8000-000000000002"
tmp="$(mktemp -d)"
db_id=""
main_protected=""
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
  if [[ -n "$db_id" && "${KEEP:-}" != 1 ]]; then
    # A protected main can still be deleted with its database, but unprotect it first anyway.
    [[ -z "$main_protected" ]] || api PATCH "/v1/databases/$db_id/branches/$db_id" '{"protected":false}' >/dev/null || true
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

# branch_field <id> <jq field>: from the branch list; empty when the branch isn't listed.
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

# psql_on <branch id> <psql args…>: through the proxy as the owner, with the branch's password;
# retries while the compute is still starting.
psql_on() {
  local id="$1" out host pw db
  shift
  ok GET "/v1/databases/$db_id/branches/$id/connection" 200
  host="$(jq -r '.host' "$tmp/body")"
  pw="$(jq -r '.password' "$tmp/body")"
  db="$(jq -r '.database' "$tmp/body")"
  for _ in $(seq 60); do
    if out="$(PGPASSWORD="$pw" PGCONNECT_TIMEOUT=10 psql \
      "host=$host hostaddr=127.0.0.1 port=${PROXY_PG_PORT} user=postgres dbname=$db sslmode=verify-full sslrootcert=$ca" \
      -X -v ON_ERROR_STOP=1 -qtA "$@" 2>"$tmp/err")"; then
      printf '%s' "$out"
      return
    fi
    sleep 2
  done
  die "psql on branch $id: $(cat "$tmp/err")"
}

# merge_get: the merge (GET …/merges/<id>) into $tmp/merge.json (the merge itself, not the log).
merge_get() {
  ok GET "/v1/databases/$db_id/merges/$merge_id" 200
  jq '.merge' "$tmp/body" >"$tmp/merge.json"
  jq '.events' "$tmp/body" >"$tmp/events.json"
}
# wait_merge <status…>: until the merge leaves its working status; dies unless it lands on one of
# the wanted ones.
wait_merge() {
  local s=""
  for _ in $(seq 300); do
    merge_get
    s="$(jq -r '.status' "$tmp/merge.json")"
    case "$s" in
      computing | rehearsing | applying | online | finishing) sleep 1 ;;
      *)
        for want in "$@"; do [[ "$s" == "$want" ]] && return 0; done
        die "merge $merge_id is $s (wanted $*): $(jq -c '{error, failed_phase, rehearsal, result}' "$tmp/merge.json")"
        ;;
    esac
  done
  die "merge $merge_id is still $s after 300 s"
}

# 1. A database, main's schema, a branch.
name="merge_e2e_$(hex 2)"
status="$(api POST /v1/databases "$(jq -nc --arg t "$DEMO_TENANT_ID" --arg p "$DEMO_PROJECT_ID" --arg n "$name" \
  '{tenant_id: $t, project_id: $p, name: $n}')")"
[[ "$status" == 202 || "$status" == 201 ]] || die "POST /v1/databases: $status $(cat "$tmp/body")"
db_id="$(jq -r '.database_id' "$tmp/body")"
[[ -n "$db_id" && "$db_id" != null ]] || die "POST /v1/databases returned no database_id: $(cat "$tmp/body")"
wait_db
psql_on "$db_id" -c "CREATE TABLE orders (id int PRIMARY KEY, status text DEFAULT 'new'); INSERT INTO orders VALUES (1, 'paid');" >/dev/null
ok POST "/v1/databases/$db_id/branches" 202 "$(jq -nc --arg p "$db_id" '{name: "dev", parent_branch_id: $p}')"
dev_id="$(jq -r '.id' "$tmp/body")"
wait_branch "$dev_id" ready
pass "database $name ($db_id) with orders, branch dev ($dev_id)"

# 2. Both sides change the schema; the column default on both (the conflict).
psql_on "$dev_id" -c "ALTER TABLE orders ALTER COLUMN status SET DEFAULT 'draft';
  ALTER TABLE orders ADD COLUMN note text; CREATE TABLE invoices (id int PRIMARY KEY, order_id int REFERENCES orders (id));" >/dev/null
psql_on "$db_id" -c "ALTER TABLE orders ALTER COLUMN status SET DEFAULT 'pending';" >/dev/null
pass "dev: a default, a column, a table; main: the same default"

# 3. Create the merge: a draft with one open conflict and no plan.
ok POST "/v1/databases/$db_id/branches/$dev_id/merges" 202 '{"direction":"into_parent"}'
merge_id="$(jq -r '.id' "$tmp/body")"
[[ "$(jq -r '.status' "$tmp/body")" == computing ]] || die "create merge: $(cat "$tmp/body")"
wait_merge draft
jq -e '(.analysis.conflicts | length) == 1 and .plan == null and .error == null' "$tmp/merge.json" >/dev/null ||
  die "draft: $(jq -c '{error, conflicts: .analysis.conflicts, plan}' "$tmp/merge.json")"
conflict="$(jq -r '.analysis.conflicts[0].id' "$tmp/merge.json")"
pass "create merge: draft, conflict $conflict open, no plan"

# 4. Resolve it (take the branch's default): a plan.
version="$(jq -r '.version' "$tmp/merge.json")"
ok PATCH "/v1/databases/$db_id/merges/$merge_id" 200 \
  "$(jq -nc --argjson v "$version" --arg c "$conflict" '{version: $v, resolutions: [{conflict: $c, choice: "take_source"}]}')"
wait_merge draft
# A save with the version from before is refused with the current merge.
ok PATCH "/v1/databases/$db_id/merges/$merge_id" 409 "$(jq -nc --argjson v "$version" '{version: $v, acknowledgements: []}')"
[[ "$(jq -r '.error.code' "$tmp/body")" == merge_version_conflict ]] || die "stale save: $(cat "$tmp/body")"
jq -e '.plan.steps | length > 0' "$tmp/merge.json" >/dev/null || die "no plan: $(jq -c '{error, plan}' "$tmp/merge.json")"
pass "resolved (take source): $(jq '.plan.steps | length' "$tmp/merge.json") steps; a stale save is 409 merge_version_conflict"

# 5. Rehearse; the rehearsal branch is never listed.
ok POST "/v1/databases/$db_id/merges/$merge_id/rehearse" 202
seen=""
for _ in $(seq 300); do
  ok GET "/v1/databases/$db_id/branches" 200
  if jq -e 'any(.[]; .name | startswith("_rehearsal-"))' "$tmp/body" >/dev/null; then seen=1; fi
  merge_get
  [[ "$(jq -r '.status' "$tmp/merge.json")" == rehearsing ]] || break
  sleep 1
done
[[ -z "$seen" ]] || die "the rehearsal branch showed in the branch list"
wait_merge rehearsed
jq -e '(.rehearsal.drift | length) == 0 and .rehearsal.failed_step == null' "$tmp/merge.json" >/dev/null ||
  die "rehearsal: $(jq -c '.rehearsal' "$tmp/merge.json")"
pass "rehearsed, no drift; the rehearsal branch never listed"

# 6. A protected main needs confirm.
ok PATCH "/v1/databases/$db_id/branches/$db_id" 200 '{"protected":true}'
main_protected=1
ok POST "/v1/databases/$db_id/merges/$merge_id/apply" 409 '{}'
[[ "$(jq -r '.error.code' "$tmp/body")" == merge_confirm_required ]] || die "apply without confirm: $(cat "$tmp/body")"
merge_get
[[ "$(jq -r '.status' "$tmp/merge.json")" == rehearsed ]] || die "the refused apply moved the merge: $(cat "$tmp/merge.json")"
ok POST "/v1/databases/$db_id/merges/$merge_id/apply" 202 '{"confirm":true}'
wait_merge done
jq -e '(.result.drift | length) == 0 and .result.after.choice == "keep"' "$tmp/merge.json" >/dev/null ||
  die "result: $(jq -c '.result' "$tmp/merge.json")"
pass "protected main: apply is 409 merge_confirm_required, then done with confirm"

# 7. main's schema, through the proxy.
got="$(psql_on "$db_id" -c "SELECT string_agg(column_name || '=' || coalesce(column_default, '-'), ',' ORDER BY ordinal_position)
  FROM information_schema.columns WHERE table_name = 'orders'")"
[[ "$got" == "id=-,status='draft'::text,note=-" ]] || die "main's orders: $got"
got="$(psql_on "$db_id" -c "SELECT count(*) FROM pg_constraint WHERE conrelid = 'invoices'::regclass AND contype = 'f'")"
[[ "$got" == 1 ]] || die "main's invoices FK: $got"
got="$(psql_on "$db_id" -c "SELECT status FROM orders WHERE id = 1")"
[[ "$got" == paid ]] || die "main's row: $got"
pass "main has dev's default, column and table (with its FK); its row is untouched"

# 8. The plan's export and the log.
status="$(api GET "/v1/databases/$db_id/merges/$merge_id/plan.sql")"
[[ "$status" == 200 ]] || die "plan.sql: $status $(cat "$tmp/body")"
grep -q 'CREATE TABLE' "$tmp/body" || die "plan.sql has no CREATE TABLE: $(head -c 400 "$tmp/body")"
jq -e '[.[] | select(.kind == "status") | .detail.to] | index("done") != null' "$tmp/events.json" >/dev/null ||
  die "log: $(jq -c . "$tmp/events.json")"
jq -e 'any(.[]; .kind == "step" and .step_id == "phase1" and .outcome == "ok")' "$tmp/events.json" >/dev/null ||
  die "log has no phase1 step: $(jq -c . "$tmp/events.json")"
pass "plan.sql exported ($(wc -c <"$tmp/body") bytes); the log ends done"

# 9. Delete.
if [[ "${KEEP:-}" != 1 ]]; then
  ok PATCH "/v1/databases/$db_id/branches/$db_id" 200 '{"protected":false}'
  main_protected=""
  status="$(api DELETE "/v1/databases/$db_id")"
  [[ "$status" == 202 || "$status" == 204 ]] || die "DELETE /v1/databases/$db_id: $status $(cat "$tmp/body")"
  db_id=""
  pass "database deleted"
fi
echo "all passed ($((SECONDS - t0)) s)"
