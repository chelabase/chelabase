#!/usr/bin/env bash
# Live check of roadmap step 6a, revision 2 (login roles managed from SQL), through the gateway
# and the proxy (needs just deps-up and just dev):
# - a realistic migration (scripts/fixtures/role-migration.sql: a group role, two login roles,
#   grants, a database) runs unchanged with psql as the branch owner;
# - listRoles shows its login roles with origin sql and password set_in_sql; the reveal
#   ?role= gives no password; the migration's password logs in;
# - a reset makes the role's password revealable; the old one is refused, the new one works;
# - a secret key on it reads through the data API; ALTER ROLE … CREATEROLE in SQL makes the key
#   answer 403 role_not_allowed, and listRoles shows the key under blocked_keys (E33: BYPASSRLS
#   alone is allowed, so the step escalates with CREATEROLE);
# - a protected branch can't be deleted (409 branch_protected) until it's unprotected;
# - a restore to before the migration removes its roles, and the migration runs again.
# KEEP=1 leaves the database in place.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=role-e2e
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
GATEWAY="${GATEWAY_URL:-http://127.0.0.1:8000}"
PROXY_PG_PORT="${CHELABASE_PROXY_PG_PORT:-4432}"
DEMO_TENANT_ID="00000000-0000-4000-8000-000000000001"
DEMO_PROJECT_ID="00000000-0000-4000-8000-000000000002"
MIGRATION="$root/scripts/fixtures/role-migration.sql"
ROLE=app_web
ROLE_PW=web-migration-pass-1
tmp="$(mktemp -d)"
db_id=""
t0=$SECONDS
step=$SECONDS

pass() {
  echo "ok   $* ($((SECONDS - step)) s, $((SECONDS - t0)) s total)"
  step=$SECONDS
}
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

# ok <method> <path> <expected status> [body]: the call, or die.
ok() {
  local status
  status="$(api "$1" "$2" "${4:-}")"
  [[ "$status" == "$3" ]] || die "$1 $2: $status (wanted $3) $(cat "$tmp/body")"
}

# reveal [query]: sets host, pw and dbname from the main branch's connection details.
reveal() {
  ok GET "$branch/connection${1:-}" 200
  host="$(jq -r '.host' "$tmp/body")"
  pw="$(jq -r '.password' "$tmp/body")"
  dbname="$(jq -r '.database' "$tmp/body")"
}

# sql_as <user> <password> <psql args…>: through the proxy.
sql_as() {
  local user="$1" password="$2"
  shift 2
  PGUSER="$user" PGPASSWORD="$password" PGCONNECT_TIMEOUT=10 psql \
    "host=$host hostaddr=127.0.0.1 port=${PROXY_PG_PORT} dbname=$dbname sslmode=verify-full sslrootcert=$ca" \
    -X -v ON_ERROR_STOP=1 -qtA "$@"
}

# refused <user> <password>: the proxy's own auth failure, not a connection error.
refused() {
  if sql_as "$1" "$2" -c 'select 1' >/dev/null 2>"$tmp/err"; then
    die "$1 connected with a password that should be refused"
  fi
  grep -q "password authentication failed for user '$1'" "$tmp/err" ||
    die "unexpected refusal of $1: $(cat "$tmp/err")"
}

# wait_ready <branch id>: polls the branch list until that branch is ready.
wait_ready() {
  local s=""
  for _ in $(seq 120); do
    ok GET "/v1/databases/$db_id/branches" 200
    s="$(jq -r --arg b "$1" '.[] | select(.id == $b) | .status' "$tmp/body")"
    case "$s" in
      ready) return 0 ;;
      failed) die "branch $1 failed: $(jq -c --arg b "$1" '.[] | select(.id == $b) | .error' "$tmp/body")" ;;
    esac
    sleep 1
  done
  die "branch $1 is still $s after 120 s"
}

# 1. A database, and the owner's connection.
name="e2e_$(hex 2)"
status="$(api POST /v1/databases "$(jq -nc --arg t "$DEMO_TENANT_ID" --arg p "$DEMO_PROJECT_ID" --arg n "$name" \
  '{tenant_id: $t, project_id: $p, name: $n}')")"
[[ "$status" == 202 || "$status" == 201 ]] || die "POST /v1/databases: $status $(cat "$tmp/body")"
db_id="$(jq -r '.database_id' "$tmp/body")"
[[ -n "$db_id" && "$db_id" != null ]] || die "POST /v1/databases returned no database_id: $(cat "$tmp/body")"
branch="/v1/databases/$db_id/branches/$db_id"
s=""
for _ in $(seq 120); do
  ok GET "/v1/databases/$db_id" 200
  s="$(jq -r '.status' "$tmp/body")"
  case "$s" in
    ready) break ;;
    failed) die "database $db_id failed: $(jq -r '.error' "$tmp/body")" ;;
  esac
  sleep 1
done
[[ "$s" == ready ]] || die "database $db_id is still $s after 120 s"
reveal
owner_pw="$pw"
for i in $(seq 60); do
  if sql_as postgres "$owner_pw" -c 'select 1' >/dev/null 2>"$tmp/err"; then break; fi
  [[ $i -lt 60 ]] || die "the owner never connected: $(cat "$tmp/err")"
  sleep 2
done
pass "database $name ($db_id) is ready; the owner connects"

# The restore point: before the migration.
sleep 2
before="$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)"
sleep 2

# 2. The migration, unchanged, as the owner.
sql_as postgres "$owner_pw" -f "$MIGRATION" >/dev/null 2>"$tmp/err" ||
  die "the migration failed: $(cat "$tmp/err")"
pass "the migration ran through the proxy"

# 3. Its login roles are listed (origin sql, password set_in_sql); the reveal has no password.
ok GET "$branch/roles" 200
for r in app_web app_worker; do
  jq -e --arg r "$r" 'any(.[]; .name == $r and .origin == "sql" and .password == "set_in_sql" and .login)' \
    "$tmp/body" >/dev/null || die "$r isn't listed as an sql role set in SQL: $(cat "$tmp/body")"
done
reveal "?role=$ROLE"
jq -e '.password == null and .password_state == "set_in_sql" and (.uri | test(":[^/]*@") | not)' \
  "$tmp/body" >/dev/null || die "unexpected reveal of $ROLE: $(jq -c 'del(.password)' "$tmp/body")"
[[ "$(sql_as "$ROLE" "$ROLE_PW" -c 'select current_user')" == "$ROLE" ]] ||
  die "psql as $ROLE with the migration's password"
[[ "$(dbname=analytics sql_as app_worker worker-migration-pass-2 -c 'select current_database()')" == analytics ]] ||
  die "psql as app_worker in analytics"
ok GET "$branch/databases" 200
jq -e 'any(.[]; .name == "analytics" and .origin == "sql") and any(.[]; .name == "postgres" and .origin == "system")' \
  "$tmp/body" >/dev/null || die "unexpected databases: $(cat "$tmp/body")"
pass "listRoles shows app_web and app_worker (sql, set_in_sql); ?role= reveals no password; both log in"

# 4. A reset makes it revealable: the migration's password is refused, the new one works.
ok POST "$branch/roles/$ROLE/reset-password" 204
reveal "?role=$ROLE"
new="$pw"
[[ -n "$new" && "$new" != null ]] || die "no password revealed after the reset: $(jq -c 'del(.password)' "$tmp/body")"
[[ "$(jq -r '.password_state' "$tmp/body")" == generated ]] || die "password_state after the reset: $(jq -c 'del(.password, .uri)' "$tmp/body")"
[[ "$(sql_as "$ROLE" "$new" -c 'select current_user')" == "$ROLE" ]] || die "the new password was refused"
refused "$ROLE" "$ROLE_PW"
pass "reset (204): the revealed password works, the migration's is refused"

# 5. A secret key on the role reads through the data API.
sql_as postgres "$owner_pw" -c "INSERT INTO orders (item) VALUES ('from-the-owner')" >/dev/null
ok POST "$branch/keys" 201 "{\"name\":\"role-e2e\",\"role\":\"$ROLE\"}"
SECRET_KEY="$(jq -r '.key' "$tmp/body")"
key_id="$(jq -r '.id' "$tmp/body")"
# read_orders: the data API's status for a select on orders (the key goes through stdin).
read_orders() {
  printf 'Authorization: Bearer %s\n' "$SECRET_KEY" |
    curl -sS --max-time 30 -o "$tmp/data" -w '%{http_code}' -H @- -H 'content-type: application/json' \
      -X POST "$GATEWAY/data/v1/query" -d '{"v":1,"op":"select","from":"orders"}' || true
}
code=""
for _ in $(seq 30); do
  code="$(read_orders)"
  [[ "$code" == 200 ]] && break
  sleep 1
done
[[ "$code" == 200 ]] || die "the data API read with the key: $code $(cat "$tmp/data")"
grep -q 'from-the-owner' "$tmp/data" || die "the data API read didn't return the row: $(cat "$tmp/data")"
pass "a secret key on $ROLE reads orders through the data API"

# 6. ALTER ROLE … CREATEROLE in SQL: the key answers 403 role_not_allowed (rest's cache: 30 s).
# BYPASSRLS alone would not do: a secret key may use a BYPASSRLS role (E33).
sql_as postgres "$owner_pw" -c "ALTER ROLE $ROLE CREATEROLE" >/dev/null
for _ in $(seq 40); do
  code="$(read_orders)"
  [[ "$code" == 403 ]] && break
  sleep 1
done
[[ "$code" == 403 ]] || die "the key still answers $code after CREATEROLE: $(cat "$tmp/data")"
jq -e '.error.code == "role_not_allowed"' "$tmp/data" >/dev/null || die "unexpected refusal: $(cat "$tmp/data")"
# listRoles' level comes from the resync (an ALTER ROLE … CREATEROLE forwards nothing to
# record): reconcile's next pass, every 30 s.
blocked() {
  ok GET "$branch/roles" 200
  jq -e --arg r "$ROLE" --arg k "$key_id" \
    'any(.[]; .name == $r and .level == "owner_level" and any(.blocked_keys[]; .id == $k and .name == "role-e2e"))' \
    "$tmp/body" >/dev/null
}
for _ in $(seq 60); do
  blocked && break
  sleep 1
done
blocked || die "listRoles doesn't show the blocked key after 60 s: $(cat "$tmp/body")"
pass "ALTER ROLE $ROLE CREATEROLE: the key gets 403 role_not_allowed; listRoles shows it blocked"
ok DELETE "$branch/keys/$key_id" 204

# 7. A protected branch can't be deleted until it's unprotected.
status="$(api POST "/v1/databases/$db_id/branches" '{"name":"staging"}')"
[[ "$status" == 202 || "$status" == 201 ]] || die "POST a branch: $status $(cat "$tmp/body")"
child="$(jq -r '.id' "$tmp/body")"
wait_ready "$child"
ok PATCH "/v1/databases/$db_id/branches/$child" 200 '{"protected":true}'
ok DELETE "/v1/databases/$db_id/branches/$child" 409
jq -e '.error.code == "branch_protected"' "$tmp/body" >/dev/null || die "unexpected refusal: $(cat "$tmp/body")"
ok PATCH "/v1/databases/$db_id/branches/$child" 200 '{"protected":false}'
status="$(api DELETE "/v1/databases/$db_id/branches/$child")"
[[ "$status" == 202 || "$status" == 204 ]] || die "DELETE the unprotected branch: $status $(cat "$tmp/body")"
for _ in $(seq 60); do
  ok GET "/v1/databases/$db_id/branches" 200
  jq -e --arg b "$child" 'all(.[]; .id != $b)' "$tmp/body" >/dev/null && break
  sleep 1
done
jq -e --arg b "$child" 'all(.[]; .id != $b)' "$tmp/body" >/dev/null || die "branch $child is still listed"
pass "protected branch: delete answers 409 branch_protected; unprotected, it deletes"

# 8. A restore to before the migration; the migration then runs again.
ok POST "$branch/restore" 202 "$(jq -nc --arg t "$before" '{at: {timestamp: $t}}')"
backup="$(jq -r '.backup_branch_id' "$tmp/body")"
wait_ready "$db_id"
reveal
[[ "$(sql_as postgres "$owner_pw" -c "SELECT count(*) FROM pg_roles WHERE rolname IN ('app_rw', 'app_web', 'app_worker')")" == 0 ]] ||
  die "the migration's roles survived the restore"
ok GET "$branch/roles" 200
jq -e 'all(.[]; .name != "app_web" and .name != "app_worker")' "$tmp/body" >/dev/null ||
  die "listRoles still has the migration's roles: $(cat "$tmp/body")"
sql_as postgres "$owner_pw" -f "$MIGRATION" >/dev/null 2>"$tmp/err" ||
  die "the migration failed on its re-run: $(cat "$tmp/err")"
[[ "$(sql_as "$ROLE" "$ROLE_PW" -c 'select current_user')" == "$ROLE" ]] ||
  die "psql as $ROLE after the re-run"
pass "restore to before the migration (backup $backup): its roles are gone, and it runs again"
echo "all passed ($((SECONDS - t0)) s)"
