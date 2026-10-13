#!/usr/bin/env bash
# Live check of roadmap step 6b (projects and app users), through the public API, the SDK and
# the proxy (needs the stack: just workbench up):
# 1. a project with a database, the data API (on by default on main) and app users enabled;
# 2. SDK sign-up: a NOT NULL app column refuses a sign-up without it (422 constraint_violation),
#    and sign-up `data` fills it once the column is granted for INSERT to authenticated;
# 3. the built-in own-row policy: each user reads only their row, the public key alone reads
#    nothing; a developer policy USING (owner = chelabase.user_id()) works the same way;
# 4. a new branch gets the users' rows (within 60 s);
# 5. an email changed in Zitadel (admin API) reaches the row through the webhook (within 30 s);
# 6. refresh rotation; the old refresh token reused after the 10 s grace is 401
#    refresh_token_reused, and the session's access token then gets 401 user_session_revoked
#    (within 35 s);
# 7. a user deleted through the platform API loses their row on both branches (within 60 s);
# 8. deleting the project deletes its databases and its Zitadel org (gone from the org search).
# KEEP=1 leaves everything in place.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=app-users-e2e
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
GATEWAY="${GATEWAY_URL:-$CHELABASE_ENTRY_API_URL}"
PROXY_PG_PORT="${CHELABASE_PROXY_PG_PORT:-4432}"
PROXY_HOSTADDR="${CHELABASE_PROXY_HOSTADDR:?run through just workbench run}"
DEMO_TENANT_ID="00000000-0000-4000-8000-000000000001"
tmp="$(mktemp -d)"
project_id=""
slug=""
db_id=""
t0=$SECONDS

pass() { echo "ok   $* ($((SECONDS - t0)) s)"; }
die() {
  echo "FAIL $*" >&2
  exit 1
}
cleanup() {
  if [[ -n "$project_id" && "${KEEP:-}" != 1 ]]; then
    # The cascade deletes the databases and the Zitadel org; a database still provisioning
    # makes it refuse (409), so that one is deleted on its own first.
    if [[ -n "$db_id" ]]; then api DELETE "/v1/databases/$db_id" >/dev/null || true; fi
    api DELETE "/v1/projects/$project_id" "$(jq -nc --arg s "$slug" '{confirm: $s}')" >/dev/null || true
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
[[ -n "${CHELABASE_ZITADEL_ADMIN_PAT_FILE:-}" && -s "${CHELABASE_ZITADEL_ADMIN_PAT_FILE}" ]] ||
  die "CHELABASE_ZITADEL_ADMIN_PAT_FILE is unset or empty (run just workbench up: its bootstrap writes demo.env)"
ZITADEL="${CHELABASE_ZITADEL_ISSUER:-$CHELABASE_ENTRY_AUTH_URL}"
command -v pnpm >/dev/null || die "pnpm is missing"
trap cleanup EXIT

# ok <method> <path> <expected status> [body]: the call, or die.
ok() {
  local status
  status="$(api "$1" "$2" "${4:-}")"
  [[ "$status" == "$3" ]] || die "$1 $2: $status (wanted $3) $(cat "$tmp/body")"
}

# zitadel <method> <path> [body]: Zitadel's API with the admin PAT; the status (body in
# $tmp/zbody). The PAT goes through a header file, never argv.
zitadel() {
  printf 'Authorization: Bearer %s\n' "$(tr -d '\n' <"$CHELABASE_ZITADEL_ADMIN_PAT_FILE")" >"$tmp/zauth"
  local args=(-sS --max-time 30 -o "$tmp/zbody" -w '%{http_code}' -X "$1" "$ZITADEL$2" -H @"$tmp/zauth")
  if [[ -n "${3:-}" ]]; then args+=(-H 'content-type: application/json' -d "$3"); fi
  curl "${args[@]}" || true
}

# org_count <id>: how many live Zitadel orgs have this id (v2 search; v1's admin read still
# returns a removed org).
org_count() {
  local status
  status="$(zitadel POST /v2/organizations/_search "$(jq -nc --arg id "$1" '{queries: [{idQuery: {id: $id}}]}')")"
  [[ "$status" == 200 ]] || die "Zitadel org search: $status $(cat "$tmp/zbody")"
  jq -r '.result // [] | length' "$tmp/zbody"
}

# wait_db: until the database is ready.
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
# wait_branch <id>: until the branch is ready.
wait_branch() {
  local s=""
  for _ in $(seq 180); do
    s="$(branch_field "$1" status)"
    [[ "$s" != ready ]] || return 0
    [[ "$s" != failed ]] || die "branch $1 failed: $(branch_field "$1" error)"
    sleep 1
  done
  die "branch $1 is '$s' after 180 s"
}

# psql_on <branch id> <psql args…>: as postgres through the proxy (the owner, so RLS doesn't
# apply); retries while the compute is still starting.
psql_on() {
  local id="$1" out host pw dbname
  shift
  ok GET "/v1/databases/$db_id/branches/$id/connection" 200
  host="$(jq -r '.host' "$tmp/body")"
  pw="$(jq -r '.password' "$tmp/body")"
  dbname="$(jq -r '.database' "$tmp/body")"
  for _ in $(seq 60); do
    if out="$(PGPASSWORD="$pw" PGCONNECT_TIMEOUT=10 psql \
      "host=$host hostaddr=$PROXY_HOSTADDR port=${PROXY_PG_PORT} user=postgres dbname=$dbname sslmode=verify-full sslrootcert=$ca" \
      -X -v ON_ERROR_STOP=1 -qtA "$@" 2>"$tmp/err")"; then
      printf '%s' "$out"
      return
    fi
    grep -qiE 'connection|timeout|starting|terminat' "$tmp/err" || die "psql: $(cat "$tmp/err")"
    sleep 2
  done
  die "psql: $(cat "$tmp/err")"
}

# wait_sql <branch id> <sql> <expected> <seconds> <what>: until the query prints the value.
wait_sql() {
  local got="" start=$SECONDS
  while :; do
    got="$(psql_on "$1" -c "$2")"
    [[ "$got" == "$3" ]] && return 0
    ((SECONDS - start <= $4)) || die "$5: '$got' after $((SECONDS - start)) s (wanted '$3')"
    sleep 1
  done
}

# sdk <phase>: packages/client/scripts/e2e.mjs auth-<phase>.
sdk() { (cd packages/client && node scripts/e2e.mjs "auth-$1") || die "scripts/e2e.mjs auth-$1"; }

# 1. A project, a database in it, app users on its main Postgres database.
name="app-users-e2e-$(hex 2)"
ok POST /v1/projects 201 "$(jq -nc --arg n "$name" '{name: $n}')"
project_id="$(jq -r '.id' "$tmp/body")"
slug="$(jq -r '.slug' "$tmp/body")"
[[ -n "$project_id" && "$project_id" != null && -n "$slug" ]] || die "POST /v1/projects: $(cat "$tmp/body")"
status="$(api POST /v1/databases "$(jq -nc --arg t "$DEMO_TENANT_ID" --arg p "$project_id" --arg n "users_$(hex 2)" \
  '{tenant_id: $t, project_id: $p, name: $n}')")"
[[ "$status" == 202 || "$status" == 201 ]] || die "POST /v1/databases: $status $(cat "$tmp/body")"
db_id="$(jq -r '.database_id' "$tmp/body")"
wait_db
ok GET "/v1/databases/$db_id/branches/$db_id/keys" 200
PUBLIC_KEY="$(jq -r 'map(select(.kind == "public")) | .[0].key // empty' "$tmp/body")"
[[ "$PUBLIC_KEY" == chk_pub_* ]] || die "no public key: $(cat "$tmp/body")"
ok PUT "/v1/projects/$project_id/app-users" 202 "$(jq -nc --arg d "$db_id" '{database_id: $d}')"
s=""
for _ in $(seq 120); do
  ok GET "/v1/projects/$project_id/app-users" 200
  s="$(jq -r '.status' "$tmp/body")"
  case "$s" in
    enabled) break ;;
    failed) die "enabling app users failed: $(jq -r '.error' "$tmp/body")" ;;
  esac
  sleep 1
done
[[ "$s" == enabled ]] || die "app users still $s after 120 s"
[[ "$(jq -r '.zitadel_org_id' "$tmp/body")" == "$project_id" ]] || die "unexpected org: $(cat "$tmp/body")"
[[ "$(org_count "$project_id")" == 1 ]] || die "Zitadel has no org $project_id"
pass "project $slug, database $db_id, app users enabled"

# 2. Sign-up: a NOT NULL app column, then sign-up data for it.
psql_on "$db_id" -c "ALTER TABLE users ADD COLUMN display_name text NOT NULL" >/dev/null
export GATEWAY_URL="$GATEWAY" PUBLIC_KEY STATE_DIR="$tmp" STAMP="$(hex 3)"
sdk signup-refused
pass "sign-up without the NOT NULL column: 422 constraint_violation"
psql_on "$db_id" -c "GRANT INSERT (display_name) ON users TO authenticated" >/dev/null
sdk signup
user_a="$(jq -r '.a.id' "$tmp/users.json")"
user_b="$(jq -r '.b.id' "$tmp/users.json")"
email_a="$(jq -r '.a.email' "$tmp/users.json")"
[[ "$(psql_on "$db_id" -c "SELECT display_name FROM users WHERE id = '$user_a'")" == Alice ]] ||
  die "A's row lacks display_name: $(psql_on "$db_id" -c "TABLE users")"
pass "signed up A ($user_a) and B with display_name"

# 2b. Generated types mark the users table (a secret key's role reads the schema).
ok POST "/v1/databases/$db_id/branches/$db_id/keys" 201 '{"name":"app-users-e2e"}'
CHELABASE_DATA_KEY="$(jq -r '.key' "$tmp/body")" "$CHELABASE_CLI" gen types --url "$GATEWAY" --out "$tmp/db.ts" ||
  die "chelabase gen types"
grep -q 'appUsers: { schema: "public"; table: "users" }' "$tmp/db.ts" ||
  die "the generated types don't mark the users table: $(grep -n appUsers "$tmp/db.ts" || echo none)"
pass "chelabase gen types marks public.users as the app users table"

# 3. Row-level security: the built-in policy, and a developer's on another table.
psql_on "$db_id" -c "CREATE TABLE notes (id bigint generated always as identity primary key,
    owner uuid NOT NULL DEFAULT chelabase.user_id(), body text NOT NULL);
  ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
  CREATE POLICY notes_own ON notes FOR ALL TO authenticated
    USING (owner = chelabase.user_id()) WITH CHECK (owner = chelabase.user_id());" >/dev/null
sdk rls
pass "own-row policies: A and B each see only their rows, the public key alone reads nothing"

# 4. A branch gets the rows.
ok POST "/v1/databases/$db_id/branches" 202 '{"name":"dev"}'
dev_id="$(jq -r '.id' "$tmp/body")"
wait_branch "$dev_id"
r0=$SECONDS
wait_sql "$dev_id" "SELECT count(*) FROM users WHERE id IN ('$user_a', '$user_b')" 2 60 "rows on the new branch"
pass "branch dev has both rows after $((SECONDS - r0)) s"

# 5. An email changed in Zitadel reaches the row through the webhook.
new_email="a2-$STAMP@example.com"
status="$(zitadel POST "/v2/users/$user_a/email" "$(jq -nc --arg e "$new_email" '{email: $e, isVerified: true}')")"
[[ "$status" == 200 ]] || die "Zitadel set email: $status $(cat "$tmp/zbody")"
r0=$SECONDS
wait_sql "$db_id" "SELECT email FROM users WHERE id = '$user_a'" "$new_email" 30 "A's email on main"
pass "Zitadel email change reached main after $((SECONDS - r0)) s (was $email_a)"

# 6. Refresh rotation, reuse after the grace, and the session's access token revoked.
sdk refresh
pass "rotation, refresh_token_reused after 11 s, then user_session_revoked"

# 7. Delete A through the platform API: the row goes on both branches.
ok DELETE "/v1/projects/$project_id/users/$user_a" 204
r0=$SECONDS
wait_sql "$db_id" "SELECT count(*) FROM users WHERE id = '$user_a'" 0 60 "A's row on main"
wait_sql "$dev_id" "SELECT count(*) FROM users WHERE id = '$user_a'" 0 60 "A's row on dev"
[[ "$(psql_on "$db_id" -c "SELECT count(*) FROM users WHERE id = '$user_b'")" == 1 ]] || die "B's row went too"
pass "A's row gone on main and dev after $((SECONDS - r0)) s"

# 8. Delete the project: its databases end deleted and its Zitadel org is gone.
[[ "${KEEP:-}" != 1 ]] || {
  echo "KEEP=1: project $slug ($project_id) left in place"
  exit 0
}
ok DELETE "/v1/projects/$project_id" 422 '{"confirm":"not-the-slug"}'
ok DELETE "/v1/projects/$project_id" 202 "$(jq -nc --arg s "$slug" '{confirm: $s}')"
r0=$SECONDS
s=""
for _ in $(seq 180); do
  status="$(api GET "/v1/projects/$project_id")"
  [[ "$status" == 404 ]] && break
  [[ "$status" == 200 ]] || die "GET /v1/projects/$project_id: $status $(cat "$tmp/body")"
  s="$(jq -r '.status + " " + (.error // "")' "$tmp/body")"
  sleep 1
done
[[ "$status" == 404 ]] || die "the project is still '$s' after 180 s"
status="$(api GET "/v1/databases/$db_id")"
if [[ "$status" == 200 ]]; then
  [[ "$(jq -r '.status' "$tmp/body")" == deleted ]] || die "database $db_id: $(cat "$tmp/body")"
else
  [[ "$status" == 404 ]] || die "GET /v1/databases/$db_id: $status $(cat "$tmp/body")"
fi
[[ "$(org_count "$project_id")" == 0 ]] || die "the Zitadel org is still there: $(cat "$tmp/zbody")"
project_id=""
pass "project deleted after $((SECONDS - r0)) s: database deleted, Zitadel org gone"
echo "all passed ($((SECONDS - t0)) s)"
