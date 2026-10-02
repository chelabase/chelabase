#!/usr/bin/env bash
# Live check of roadmap step 6c (the data API across databases, and Studio's console), through
# the gateway and the proxy (needs just deps-up and just dev):
# - CREATE DATABASE analytics in SQL is listed (not served), takes a default schema and is
#   enabled through the API;
# - a secret key allowed only on analytics reads it through the gateway; main answers 404
#   unknown_database; the public key's list is edited too;
# - the console as the owner (rows in analytics and in main: one owner pool per database), with
#   insert, update, delete and a batch;
# - the console as anon under a row-level security policy (it sees, writes and deletes only
#   what the policies allow), as a plain role, as an owner-level role (refused), as a role that
#   doesn't exist (refused) and as one dropped after its check (unknown_role);
# - an owner password set in SQL answers 409 owner_password_unknown until a reset;
# - main can't be switched off; disable answers 409 data_api_disabled in the console and 404
#   to the key at once; remove machinery drops the cheladb schema; delete.
# KEEP=1 leaves the database in place.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=console-e2e
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
GATEWAY="${GATEWAY_URL:-http://127.0.0.1:8000}"
PROXY_PG_PORT="${CHELADB_PROXY_PG_PORT:-4432}"
DEMO_TENANT_ID="00000000-0000-4000-8000-000000000001"
DEMO_PROJECT_ID="00000000-0000-4000-8000-000000000002"
DB2=analytics
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

cargo build -q -p cheladb || die "cargo build -p cheladb"
[[ -f .cheladb/zitadel/demo.env ]] || die ".cheladb/zitadel/demo.env is missing (run just dev)"
set -a
# shellcheck disable=SC1091
source .cheladb/zitadel/demo.env
set +a
ca="$root/.cheladb/neon/proxy-ca.crt"
[[ -s "$ca" ]] || die "$ca is missing (run just deps-up)"
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

# psql_in <database> <psql args…>: through the proxy as the owner, with the branch's current
# password; retries while the compute is still starting.
psql_in() {
  local database="$1" out host pw
  shift
  ok GET "$branch/connection" 200
  host="$(jq -r '.host' "$tmp/body")"
  pw="$(jq -r '.password' "$tmp/body")"
  [[ "$database" == main ]] && database="$(jq -r '.database' "$tmp/body")"
  for _ in $(seq 60); do
    if out="$(PGPASSWORD="$pw" PGCONNECT_TIMEOUT=10 psql \
      "host=$host hostaddr=127.0.0.1 port=${PROXY_PG_PORT} user=postgres dbname=$database sslmode=verify-full sslrootcert=$ca" \
      -v ON_ERROR_STOP=1 -qtA "$@" 2>"$tmp/err")"; then
      printf '%s' "$out"
      return
    fi
    sleep 2
  done
  die "psql in $database: $(cat "$tmp/err")"
}

# db_field <name> <jq field>: the field of the branch's Postgres database <name> (empty when
# it isn't listed).
db_field() {
  ok GET "$branch/databases" 200
  jq -r --arg n "$1" ".[] | select(.name == \$n) | .$2 // empty" "$tmp/body"
}

# console <role JSON: null or "name"> <database JSON: null or "name"> <tree>: the console call;
# prints the status, the body goes to $tmp/body.
console() {
  api POST "$branch/console/query" "$(jq -nc --argjson r "$1" --argjson d "$2" --argjson t "$3" \
    '{role: $r, database: $d, tree: $t}')"
}
# console_ok <role> <database> <tree> <jq check>: a 200 whose body passes the check.
console_ok() {
  local status
  status="$(console "$1" "$2" "$3")"
  [[ "$status" == 200 ]] || die "console as $1 in $2: $status $(cat "$tmp/body") for $3"
  jq -e "$4" "$tmp/body" >/dev/null || die "console as $1 in $2: $(cat "$tmp/body") fails $4 for $3"
}
# console_err <role> <database> <tree> <status> <code>: that error.
console_err() {
  local status
  status="$(console "$1" "$2" "$3")"
  [[ "$status" == "$4" && "$(jq -r '.error.code' "$tmp/body")" == "$5" ]] ||
    die "console as $1 in $2: $status $(cat "$tmp/body") (wanted $4 $5) for $3"
}

# data <key> <tree>: a data API query through the gateway; prints the status, the body goes to
# $tmp/data. The header goes through stdin, so the key never shows in argv.
data() {
  printf 'Authorization: Bearer %s\n' "$1" |
    curl -sS --max-time 30 -o "$tmp/data" -w '%{http_code}' -H @- -H 'content-type: application/json' \
      -d "$2" "$GATEWAY/data/v1/query" || true
}

# 1. A database, and a second Postgres database made in SQL.
name="console_e2e_$(hex 2)"
status="$(api POST /v1/databases "$(jq -nc --arg t "$DEMO_TENANT_ID" --arg p "$DEMO_PROJECT_ID" --arg n "$name" \
  '{tenant_id: $t, project_id: $p, name: $n}')")"
[[ "$status" == 202 || "$status" == 201 ]] || die "POST /v1/databases: $status $(cat "$tmp/body")"
db_id="$(jq -r '.database_id' "$tmp/body")"
[[ -n "$db_id" && "$db_id" != null ]] || die "POST /v1/databases returned no database_id: $(cat "$tmp/body")"
branch="/v1/databases/$db_id/branches/$db_id"
wait_db
pass "database $name ($db_id) is ready"

psql_in main -c "CREATE DATABASE $DB2" >/dev/null
for _ in $(seq 30); do
  [[ -n "$(db_field "$DB2" data_api)" ]] && break
  sleep 1
done
[[ "$(db_field "$DB2" data_api)" == off ]] || die "$DB2 isn't listed as off: $(cat "$tmp/body")"
psql_in "$DB2" -c "CREATE SCHEMA app;
  CREATE TABLE app.items (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, label text NOT NULL,
    visibility text NOT NULL DEFAULT 'private');
  INSERT INTO app.items (label, visibility) VALUES ('first', 'public'), ('second', 'private');" >/dev/null
psql_in main -c "CREATE TABLE notes (id int PRIMARY KEY, body text); INSERT INTO notes VALUES (1, 'in main');" >/dev/null
pass "CREATE DATABASE $DB2 in SQL is listed, not served; tables in both databases"

# 2. Its default schema, then enable it.
ok PATCH "$branch/pg-databases/$DB2" 200 '{"default_schema":"app"}'
[[ "$(jq -r '.default_schema' "$tmp/body")" == app ]] || die "default_schema: $(cat "$tmp/body")"
ok POST "$branch/pg-databases/system_db_check/data-api" 404
ok POST "$branch/pg-databases/postgres/data-api" 422
ok POST "$branch/pg-databases/$DB2/data-api" 202
state=""
for _ in $(seq 60); do
  state="$(db_field "$DB2" data_api)"
  case "$state" in
    on) break ;;
    error) die "enabling $DB2 failed: $(db_field "$DB2" data_api_error)" ;;
  esac
  sleep 1
done
[[ "$state" == on ]] || die "$DB2 is still $state after 60 s"
pass "$DB2 is served with default schema app (postgres: 422, unknown: 404)"

# 3. A secret key allowed only on analytics, through the gateway.
ok POST "$branch/keys" 201 "{\"name\":\"console-e2e\",\"databases\":[\"$DB2\"]}"
KEY="$(jq -r '.key' "$tmp/body")"
[[ "$KEY" == chk_sec_* && "$(jq -c '.databases' "$tmp/body")" == "[\"$DB2\"]" ]] ||
  die "unexpected secret key: $(jq -c 'del(.key)' "$tmp/body")"
in_db2='{"v":1,"database":"'"$DB2"'","op":"select","from":"items","orderBy":[{"col":"id"}]}'
code="$(data "$KEY" "$in_db2")"
[[ "$code" == 200 && "$(jq -c '[.[].label]' "$tmp/data")" == '["first","second"]' ]] ||
  die "the key in $DB2: $code $(cat "$tmp/data")"
code="$(data "$KEY" '{"v":1,"op":"select","from":"notes"}')"
[[ "$code" == 404 && "$(jq -r '.error.code' "$tmp/data")" == unknown_database ]] ||
  die "the key in main: $code $(cat "$tmp/data") (wanted 404 unknown_database)"
printf 'Authorization: Bearer %s\n' "$KEY" |
  curl -sS --max-time 30 -o "$tmp/data" -H @- "$GATEWAY/data/v1/schema?database=$DB2" || true
[[ "$(jq -c '.databases' "$tmp/data")" == "[\"$DB2\"]" ]] || die "schema databases: $(cat "$tmp/data")"
pass "a key allowed only on $DB2 reads it; main is 404 unknown_database; the schema lists $DB2 only"

# The public key's list is editable too (the gateway's key cache: 30 s, 35 allowed).
ok GET "$branch/keys" 200
PUBLIC_KEY="$(jq -r 'map(select(.kind == "public")) | .[0].key // empty' "$tmp/body")"
public_id="$(jq -r 'map(select(.kind == "public")) | .[0].id // empty' "$tmp/body")"
[[ "$PUBLIC_KEY" == chk_pub_* ]] || die "no public key in the key list"
code="$(data "$PUBLIC_KEY" '{"v":1,"op":"select","from":"notes"}')"
[[ "$code" == 200 ]] || die "the public key in main before the edit: $code $(cat "$tmp/data")"
ok PATCH "$branch/keys/$public_id" 200 "{\"databases\":[\"$DB2\"]}"
[[ "$(jq -c '.databases' "$tmp/body")" == "[\"$DB2\"]" ]] || die "PATCH key: $(cat "$tmp/body")"
r0=$SECONDS
while :; do
  code="$(data "$PUBLIC_KEY" '{"v":1,"op":"select","from":"notes"}')"
  [[ "$code" == 404 ]] && break
  ((SECONDS - r0 <= 35)) || die "the public key still answers $code in main $((SECONDS - r0)) s after the edit"
  sleep 1
done
ok PATCH "$branch/keys/$public_id" 200 '{"databases":null}'
pass "the public key's list narrowed main away after $((SECONDS - r0)) s, and was reset to every database"

# 4. The console as the owner: both databases (one owner pool each), CRUD and a batch.
console_ok null null '{"v":1,"op":"select","from":"notes"}' '. == [{"id":1,"body":"in main"}]'
console_ok null "\"$DB2\"" '{"v":1,"op":"select","from":"items","orderBy":[{"col":"id"}]}' \
  '[.[].label] == ["first","second"]'
console_ok null "\"$DB2\"" '{"v":1,"op":"insert","into":"items","values":[{"label":"third"}],"returning":["id","visibility"]}' \
  'length == 1 and .[0].visibility == "private"'
id="$(jq -r '.[0].id' "$tmp/body")"
console_ok null "\"$DB2\"" "{\"v\":1,\"op\":\"update\",\"table\":\"items\",\"set\":{\"label\":\"third, edited\"},\"where\":{\"col\":\"id\",\"eq\":$id},\"returning\":[\"label\"]}" \
  '. == [{"label":"third, edited"}]'
console_ok null "\"$DB2\"" "{\"v\":1,\"op\":\"delete\",\"from\":\"items\",\"where\":{\"col\":\"id\",\"eq\":$id},\"returning\":[\"id\"]}" \
  "length == 1"
console_ok null null '{"v":1,"op":"batch","ops":[{"op":"insert","into":"notes","values":[{"id":2,"body":"batch"}]},{"op":"select","from":"notes","orderBy":[{"col":"id"}]}]}' \
  '.[1] | length == 2'
# The tree may name the database itself; naming two different ones is refused.
console_ok null null "{\"v\":1,\"database\":\"$DB2\",\"op\":\"select\",\"from\":\"items\"}" 'length == 2'
console_err null '"nope"' '{"v":1,"op":"select","from":"items"}' 404 not_found
pass "owner: reads in main and $DB2, insert, update, delete and a batch"

# 5. Row-level security, as anon: it sees, writes and deletes only what the policies allow.
psql_in "$DB2" -c "ALTER TABLE app.items ENABLE ROW LEVEL SECURITY;
  GRANT INSERT, UPDATE, DELETE ON app.items TO anon;
  CREATE POLICY anon_read ON app.items FOR SELECT TO anon USING (visibility = 'public');
  CREATE POLICY anon_write ON app.items FOR INSERT TO anon WITH CHECK (visibility = 'public');
  CREATE POLICY anon_edit ON app.items FOR UPDATE TO anon USING (visibility = 'public') WITH CHECK (visibility = 'public');
  CREATE POLICY anon_delete ON app.items FOR DELETE TO anon USING (visibility = 'public');" >/dev/null
console_ok '"anon"' "\"$DB2\"" '{"v":1,"op":"select","from":"items"}' '[.[].label] == ["first"]'
console_ok '"anon"' "\"$DB2\"" '{"v":1,"op":"insert","into":"items","values":[{"label":"from anon","visibility":"public"}],"returning":["id"]}' \
  'length == 1'
anon_id="$(jq -r '.[0].id' "$tmp/body")"
console_err '"anon"' "\"$DB2\"" '{"v":1,"op":"insert","into":"items","values":[{"label":"hidden","visibility":"private"}]}' \
  403 permission_denied
console_ok '"anon"' "\"$DB2\"" '{"v":1,"op":"update","table":"items","set":{"label":"touched"},"where":{"col":"label","eq":"second"},"returning":["id"]}' \
  '. == []'
console_ok '"anon"' "\"$DB2\"" "{\"v\":1,\"op\":\"update\",\"table\":\"items\",\"set\":{\"label\":\"from anon, edited\"},\"where\":{\"col\":\"id\",\"eq\":$anon_id},\"returning\":[\"label\"]}" \
  '. == [{"label":"from anon, edited"}]'
console_ok '"anon"' "\"$DB2\"" '{"v":1,"op":"delete","from":"items","all":true,"returning":["label"]}' \
  '[.[].label] | sort == ["first","from anon, edited"]'
console_ok null "\"$DB2\"" '{"v":1,"op":"select","from":"items"}' '[.[].label] == ["second"]'
pass "anon under RLS: reads, inserts, updates and deletes only public rows; the owner sees the rest"

# 6. Other roles: a plain one, an owner-level one, one that doesn't exist, one dropped later.
psql_in "$DB2" -c "CREATE ROLE reporter NOLOGIN; GRANT USAGE ON SCHEMA app TO reporter;
  GRANT SELECT ON app.items TO reporter;
  CREATE POLICY reporter_read ON app.items FOR SELECT TO reporter USING (true);
  CREATE ROLE boss NOLOGIN CREATEDB;" >/dev/null
console_ok '"reporter"' "\"$DB2\"" '{"v":1,"op":"select","from":"items"}' '[.[].label] == ["second"]'
console_err '"reporter"' "\"$DB2\"" '{"v":1,"op":"delete","from":"items","all":true}' 403 permission_denied
console_err '"boss"' "\"$DB2\"" '{"v":1,"op":"select","from":"items"}' 403 role_refused
jq -r '.error.message' "$tmp/body" | grep -q "owner-level" || die "boss: $(cat "$tmp/body")"
console_err '"ghost"' "\"$DB2\"" '{"v":1,"op":"select","from":"items"}' 403 role_refused
jq -r '.error.message' "$tmp/body" | grep -q "does not exist" || die "ghost: $(cat "$tmp/body")"
psql_in "$DB2" -c "DROP POLICY reporter_read ON app.items; REVOKE ALL ON app.items FROM reporter;
  REVOKE ALL ON SCHEMA app FROM reporter; DROP ROLE reporter;" >/dev/null
# rest remembers a role's owner-level check for 30 s: until then a dropped role is 503
# unavailable (as a secret key's would be), then 404 unknown_role (35 s allowed).
r0=$SECONDS
while :; do
  status="$(console '"reporter"' "\"$DB2\"" '{"v":1,"op":"select","from":"items"}')"
  [[ "$status" == 404 ]] && break
  [[ "$status" == 503 ]] || die "dropped reporter: $status $(cat "$tmp/body") (wanted 503, then 404)"
  ((SECONDS - r0 <= 35)) || die "dropped reporter is still 503 after $((SECONDS - r0)) s"
  sleep 1
done
[[ "$(jq -r '.error.code' "$tmp/body")" == unknown_role ]] || die "dropped reporter: $(cat "$tmp/body")"
pass "plain role reads under its policy; owner-level and unknown roles refused (403); a dropped role is 404 unknown_role after $((SECONDS - r0)) s"

# 7. An owner password set in SQL: 409 until a reset, then the owner console works at once.
psql_in main -c "ALTER ROLE postgres PASSWORD 'console-e2e-$(hex 8)'" >/dev/null
r0=$SECONDS
while :; do
  status="$(console null "\"$DB2\"" '{"v":1,"op":"select","from":"items"}')"
  [[ "$status" == 409 ]] && break
  ((SECONDS - r0 <= 30)) || die "the owner console still answers $status after the SQL password change"
  sleep 1
done
[[ "$(jq -r '.error.code' "$tmp/body")" == owner_password_unknown ]] || die "owner: $(cat "$tmp/body")"
ok POST "$branch/reset-password" 204
console_ok null "\"$DB2\"" '{"v":1,"op":"select","from":"items"}' 'length == 1'
pass "owner_password_unknown after ALTER ROLE … PASSWORD, and the owner console works right after a reset"

# 8. Disable: main can't be; analytics stops at once (console 409, the key 404); remove the
# machinery.
ok DELETE "$branch/pg-databases/$(psql_in main -c 'SELECT current_database()')/data-api" 409
ok DELETE "$branch/pg-databases/$DB2/data-api" 204
[[ "$(db_field "$DB2" data_api)" == off ]] || die "$DB2 isn't off: $(cat "$tmp/body")"
console_err null "\"$DB2\"" '{"v":1,"op":"select","from":"items"}' 409 data_api_disabled
code="$(data "$KEY" "$in_db2")"
[[ "$code" == 404 && "$(jq -r '.error.code' "$tmp/data")" == unknown_database ]] ||
  die "the key in $DB2 after the disable: $code $(cat "$tmp/data")"
[[ "$(psql_in "$DB2" -c "SELECT count(*) FROM pg_namespace WHERE nspname = 'cheladb'")" == 1 ]] ||
  die "disable dropped the machinery"
ok DELETE "$branch/pg-databases/$DB2/data-api?remove=true" 204
[[ "$(psql_in "$DB2" -c "SELECT count(*) FROM pg_namespace WHERE nspname = 'cheladb'")" == 0 ]] ||
  die "remove left the cheladb schema"
pass "main can't be switched off; disabled $DB2 is 409 in the console and 404 to the key at once; remove dropped cheladb"

# 9. Delete.
if [[ "${KEEP:-}" != 1 ]]; then
  status="$(api DELETE "/v1/databases/$db_id")"
  [[ "$status" == 202 || "$status" == 204 ]] || die "DELETE /v1/databases/$db_id: $status $(cat "$tmp/body")"
  db_id=""
  pass "database deleted"
fi
echo "all passed ($((SECONDS - t0)) s)"
