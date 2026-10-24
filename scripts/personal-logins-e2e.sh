#!/usr/bin/env bash
# Live check of personal Postgres logins (members hub step 1, spec Part 6), through the gateway
# and Neon's proxy (needs the stack: just workbench up; it runs in the end-of-step live pass):
# 1. a project, a database with group roles `billing` (owning schema billing and billing.t) and
#    `other_grp`, and a member `dev` (invited by link) with a Developer grant on `billing`;
# 2. dev creates a personal login: the password is shown once (no-store) and never listed;
#    psql through the proxy connects as the login, SET ROLE billing works (INSERT into
#    billing.t), SET ROLE into other_grp or the owner is refused; P33: with
#    `options=-c role=billing` the session runs as billing (session_user the login) and what it
#    creates is billing's, and `role=other_grp` is refused at connect;
# 3. a child branch gets the login (same password) through its reconcile;
# 4. reset: the old password is refused at once on every branch, the new one works;
# 5. the grant deleted while main's compute is stopped: the proxy refuses the login without
#    starting the compute (MH-9);
# 6. re-granted, a new login works; suspended: refused (and the member's API calls are 403
#    member_suspended); reinstated: still refused until dev makes a new login, which works;
# 7. "Revoke Postgres access now": refused; an SQL-editor session opened before it ends, and the
#    next run works with the member role's new password (P16 b);
# 8. an expired VALID UNTIL (set short in the metadata): refused once the proxy's cache entry
#    expires; a reset renews it;
# 9. dev's Zitadel user deactivated in Zitadel (its Actions webhook reaches the orchestrator):
#    refused.
# Everything it makes is removed on exit, also on failure. KEEP=1 leaves the project.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=personal-logins-e2e
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
GATEWAY="${GATEWAY_URL:-$CHELABASE_ENTRY_API_URL}"
# The runner dials the proxy inside the stack network; the connection strings name the host's.
PROXY_PG_PORT="${CHELABASE_PROXY_PG_PORT:-4432}"
PROXY_HOSTADDR="${CHELABASE_PROXY_HOSTADDR:?run through just workbench run}"
tmp="$(mktemp -d)"
project_id=""
slug=""
db_id=""
dbname=""
bg_pid=""
# A compute container this script stopped (started again on exit).
stopped=""
t0=$SECONDS
step=$SECONDS

pass() {
  echo "PASS $* ($((SECONDS - step)) s, $((SECONDS - t0)) s total)"
  step=$SECONDS
}

[[ -x "${CHELABASE_CLI:-}" ]] || die "CHELABASE_CLI is unset or not executable (run through: just workbench run scripts/<name>.sh)"
demo_env="${CHELABASE_ZITADEL_DIR:?run through just workbench run}/demo.env"
[[ -f "$demo_env" ]] || die "$demo_env is missing (run just workbench up)"
set -a
# shellcheck disable=SC1090
source "$demo_env"
set +a
ca="${CHELABASE_PROXY_CA_FILE:?run through just workbench run}"
[[ -s "$ca" ]] || die "$ca is missing (run just workbench up)"
[[ -n "${CHELABASE_ZITADEL_ADMIN_PAT_FILE:-}" && -s "${CHELABASE_ZITADEL_ADMIN_PAT_FILE}" ]] ||
  die "CHELABASE_ZITADEL_ADMIN_PAT_FILE is unset or empty (run just workbench up: its bootstrap writes demo.env)"
ZITADEL="${CHELABASE_ZITADEL_ISSUER:-$CHELABASE_ENTRY_AUTH_URL}"
: "${CHELABASE_STACK:?run through just workbench run}"
command -v docker >/dev/null || die "docker is missing (step 5 stops main's compute)"
# shellcheck source=lib/member-sign-in.sh
. scripts/lib/member-sign-in.sh

cleanup() {
  local rc=$?
  set +e
  [[ -z "$bg_pid" ]] || kill "$bg_pid" 2>/dev/null
  [[ -z "$stopped" ]] || docker start "$stopped" >/dev/null 2>&1
  [[ -z "$MEMBER_ID" ]] || api DELETE "/v1/members/$MEMBER_ID" >/dev/null
  if [[ -n "$project_id" && "${KEEP:-}" != 1 ]]; then
    api DELETE "/v1/projects/$project_id" "$(jq -nc --arg s "$slug" '{confirm: $s}')" >/dev/null
  fi
  rm -rf "$tmp"
  exit "$rc"
}
trap cleanup EXIT

# ok <method> <path> <expected status> [body]: the admin's call, or die.
ok() {
  local status
  status="$(api "$1" "$2" "${4:-}")"
  [[ "$status" == "$3" ]] || die "$1 $2: $status (wanted $3) $(cat "$tmp/body")"
}

# uapi <token> <method> <path> [json body]: prints the status; the body goes to $tmp/body and the
# headers to $tmp/headers. The token goes through stdin, never argv.
uapi() {
  local args=(-sS --max-time 60 -D "$tmp/headers" -o "$tmp/body" -w '%{http_code}' -X "$2" "$GATEWAY$3")
  if [[ -n "${4:-}" ]]; then args+=(-H 'content-type: application/json' -d "$4"); fi
  printf 'Authorization: Bearer %s\n' "$1" | curl "${args[@]}" -H @- || true
}

# zitadel <method> <path> [body]: Zitadel's API with the admin PAT; the status (body in
# $tmp/zbody). The PAT goes through a header file, never argv.
zitadel() {
  printf 'Authorization: Bearer %s\n' "$(tr -d '\n' <"$CHELABASE_ZITADEL_ADMIN_PAT_FILE")" >"$tmp/zauth"
  local args=(-sS --max-time 30 -o "$tmp/zbody" -w '%{http_code}' -X "$1" "$ZITADEL$2" -H @"$tmp/zauth")
  if [[ -n "${3:-}" ]]; then args+=(-H 'content-type: application/json' -d "$3"); fi
  curl "${args[@]}" || true
}

# pg <user> <password> <host> <sql>: psql through the proxy (output on stdout, errors in
# $tmp/err). The password goes through the environment, never argv.
pg() {
  PGPASSWORD="$2" PGCONNECT_TIMEOUT=10 psql \
    "host=$3 hostaddr=$PROXY_HOSTADDR port=$PROXY_PG_PORT user=$1 dbname=$dbname sslmode=verify-full sslrootcert=$ca" \
    -v ON_ERROR_STOP=1 -qtA -c "$4" 2>"$tmp/err"
}

# connects <user> <password> <host> <what>: until `select 1` works as the login (it reaches a
# branch with that branch's reconcile, and a compute may be starting), 120 s at most.
connects() {
  for _ in $(seq 60); do
    pg "$1" "$2" "$3" 'select 1' >/dev/null && return 0
    sleep 2
  done
  die "$4: $1 never connected on $3: $(cat "$tmp/err")"
}

# refused <user> <password> <host> <what> [seconds]: until the proxy refuses the password with
# its own auth failure (not a connection error), `seconds` at most (default 10: revocation is
# immediate).
refused() {
  for _ in $(seq "${5:-10}"); do
    if ! pg "$1" "$2" "$3" 'select 1' >/dev/null && grep -q "password authentication failed for user" "$tmp/err"; then
      return 0
    fi
    sleep 1
  done
  die "$4: $1 isn't refused on $3: $(cat "$tmp/err")"
}

# new_login <token> <name>: the member creates their login (201, no-store); sets LOGIN_PW and
# MAIN_HOST (main's host, from the connection string). Run it directly, never in `$(…)`.
new_login() {
  local s uri
  s="$(uapi "$1" PUT "/v1/databases/$db_id/personal-login" "$(jq -nc --arg n "$2" '{name: $n}')")"
  [[ "$s" == 201 ]] || die "create login $2: $s $(cat "$tmp/body")"
  grep -qiE '^cache-control: *no-store' "$tmp/headers" || die "create login $2: the answer isn't no-store"
  LOGIN_PW="$(jq -r .password "$tmp/body")"
  uri="$(jq -r .connection_string "$tmp/body")"
  [[ "$(jq -r .login.role_name "$tmp/body")" == "$2" ]] || die "create login $2: the answer names another role"
  rm -f "$tmp/body" # the password is shown once; it doesn't stay on disk
  [[ ${#LOGIN_PW} -ge 22 ]] || die "create login $2: a short password"
  [[ "$uri" == "postgresql://$2:$LOGIN_PW@"* ]] || die "create login $2: the connection string isn't the login's (not shown)"
  MAIN_HOST="$(sed -E 's#^postgresql://[^@]+@([^:/]+).*#\1#' <<<"$uri")"
  [[ "${MAIN_HOST%%.*}" =~ ^ep-[0-9a-f]{12}$ ]] || die "create login $2: unexpected host $MAIN_HOST"
}

# reset_login <token> <name>: a reset (200, no-store); sets LOGIN_PW.
reset_login() {
  local s
  s="$(uapi "$1" POST "/v1/databases/$db_id/personal-login/reset-password")"
  [[ "$s" == 200 ]] || die "reset login $2: $s $(cat "$tmp/body")"
  grep -qiE '^cache-control: *no-store' "$tmp/headers" || die "reset login $2: the answer isn't no-store"
  LOGIN_PW="$(jq -r .password "$tmp/body")"
  rm -f "$tmp/body"
}

# grant <roles json array>: dev's Developer grant, retried while the roles aren't recorded yet
# (the DDL that made them is forwarded to the metadata as it runs).
grant() {
  local s=""
  for _ in $(seq 30); do
    s="$(api PUT "/v1/projects/$project_id/grants/$MEMBER_ID" "{\"platform_role\":\"developer\",\"pg_roles\":$1}")"
    [[ "$s" == 200 ]] && return 0
    sleep 1
  done
  die "grant $1: $s $(cat "$tmp/body")"
}

# The SQL editor as dev (P16 b's check): run_body <role> <sql> [timeout_ms] → the request body;
# stream <token> <out file> <max seconds> <json body> → the status.
run_body() {
  jq -nc --arg id "$(cat /proc/sys/kernel/random/uuid)" --arg sql "$2" --arg role "$1" --arg t "${3:-}" \
    '{run_id: $id, sql: $sql, role: $role} + (if $t == "" then {} else {timeout_ms: ($t | tonumber)} end)'
}
stream() {
  printf 'Authorization: Bearer %s\n' "$1" |
    curl -sSN --max-time "$3" -o "$2" -w '%{http_code}' -H @- -H 'content-type: application/json' \
      -X POST -d "$4" "$GATEWAY/v1/databases/$db_id/branches/$db_id/sql" || true
}

# --- 1. A project, a database, the group roles, the member ------------------------------------

name="pl_e2e_$(hex 2)"
ok POST /v1/projects 201 "$(jq -nc --arg n "$name" '{name: $n}')"
project_id="$(jq -r '.id' "$tmp/body")"
slug="$(jq -r '.slug' "$tmp/body")"
[[ -n "$project_id" && "$project_id" != null && -n "$slug" ]] || die "POST /v1/projects: $(cat "$tmp/body")"
s="$(api POST /v1/databases "$(jq -nc --arg p "$project_id" --arg n "pl_$(hex 2)" '{project_id: $p, name: $n}')")"
[[ "$s" == 202 || "$s" == 201 ]] || die "POST /v1/databases: $s $(cat "$tmp/body")"
db_id="$(jq -r '.database_id' "$tmp/body")"
[[ -n "$db_id" && "$db_id" != null ]] || die "POST /v1/databases returned no database_id: $(cat "$tmp/body")"
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

ok GET "/v1/databases/$db_id/connection" 200
owner="$(jq -r '.user' "$tmp/body")"
owner_pw="$(jq -r '.password' "$tmp/body")"
owner_host="$(jq -r '.host' "$tmp/body")"
dbname="$(jq -r '.database' "$tmp/body")"
rm -f "$tmp/body"
connects "$owner" "$owner_pw" "$owner_host" "the owner"
pg "$owner" "$owner_pw" "$owner_host" "CREATE ROLE billing NOLOGIN;
  CREATE SCHEMA billing AUTHORIZATION billing;
  CREATE TABLE billing.t(id int PRIMARY KEY, note text);
  ALTER TABLE billing.t OWNER TO billing;
  CREATE ROLE other_grp NOLOGIN" >/dev/null || die "setup: $(cat "$tmp/err")"

new_member "$project_id" '{"platform_role":"developer","pg_roles":[]}'
grant '["billing"]'
DEV_TOKEN="$(member_token)"
s="$(uapi "$DEV_TOKEN" GET /v1/me)"
[[ "$s" == 200 ]] || die "dev's /v1/me: $s $(cat "$tmp/body")"
ZUSER="$(jq -r '.sub' "$tmp/body")"
[[ -n "$ZUSER" && "$ZUSER" != null ]] || die "dev's /v1/me has no sub"
pass "project $name, database $db_id with billing and other_grp, member $MEMBER_ID (Zitadel user $ZUSER) a Developer with billing"

# --- 2. The login: shown once, connects, SET ROLE only into the safe set ------------------------

LOGIN="pl_dev_$(hex 2)"
new_login "$DEV_TOKEN" "$LOGIN"
connects "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" "main"
out="$(pg "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" "SET ROLE billing; INSERT INTO billing.t VALUES (1, 'from a personal login'); SELECT count(*) FROM billing.t")" ||
  die "SET ROLE billing and INSERT: $(cat "$tmp/err")"
[[ "$out" == 1 ]] || die "billing.t after the INSERT: $out"
for role in other_grp "$owner"; do
  if pg "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" "SET ROLE $role" >/dev/null; then die "SET ROLE $role worked"; fi
  grep -q "permission denied to set role" "$tmp/err" || die "SET ROLE $role: $(cat "$tmp/err")"
done
s="$(uapi "$DEV_TOKEN" GET "/v1/members/$MEMBER_ID/personal-logins")"
[[ "$s" == 200 ]] || die "dev's logins: $s $(cat "$tmp/body")"
jq -e --arg n "$LOGIN" 'any(.logins[]; .role_name == $n and .status == "active")' "$tmp/body" >/dev/null ||
  die "the login isn't listed: $(cat "$tmp/body")"
if grep -qF "$LOGIN_PW" "$tmp/body"; then die "the password is listed"; fi
pass "login $LOGIN: shown once (no-store), connects through the proxy; SET ROLE billing works, other_grp and the owner are refused"

# --- 2b. P33: a default role at connect, `options=-c role=<group>` (URL-encoded, as Studio's
# connection string carries it) -----------------------------------------------------------------

# pg_as <user> <password> <host> <group> <sql>: `pg` through a connection URI whose `options`
# set the role at startup. The password goes through the environment, never argv.
pg_as() {
  PGPASSWORD="$2" PGCONNECT_TIMEOUT=10 psql \
    "postgresql://$1@$3:$PROXY_PG_PORT/$dbname?hostaddr=$PROXY_HOSTADDR&sslmode=verify-full&sslrootcert=$ca&options=-c%20role%3D$4" \
    -v ON_ERROR_STOP=1 -qtA -c "$5" 2>"$tmp/err"
}
out="$(pg_as "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" billing "SELECT current_user || ' ' || session_user")" ||
  die "P33: connecting with role=billing: $(cat "$tmp/err")"
[[ "$out" == "billing $LOGIN" ]] || die "P33: current_user and session_user are '$out', not 'billing $LOGIN'"
p33_table="p33_$(hex 2)"
out="$(pg_as "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" billing "CREATE TABLE billing.$p33_table(id int);
  SELECT tableowner FROM pg_catalog.pg_tables WHERE schemaname = 'billing' AND tablename = '$p33_table'")" ||
  die "P33: CREATE TABLE as the default role: $(cat "$tmp/err")"
[[ "$out" == billing ]] || die "P33: billing.$p33_table is owned by '$out', not the group"
if pg_as "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" other_grp 'select 1' >/dev/null; then
  die "P33: a group the login doesn't hold (other_grp) was set at connect"
fi
grep -q "permission denied to set role" "$tmp/err" || die "P33: role=other_grp: $(cat "$tmp/err")"
pass "P33: options=-c role=billing makes billing the current user (session user $LOGIN) and the owner of what it creates; role=other_grp is refused at connect"

# --- 3. A child branch gets the login ------------------------------------------------------------

ok POST "/v1/databases/$db_id/branches" 202 "$(jq -nc --arg p "$db_id" '{name: "pl-child", parent_branch_id: $p}')"
child_id="$(jq -r '.id' "$tmp/body")"
child_host=""
for _ in $(seq 180); do
  ok GET "/v1/databases/$db_id/branches" 200
  s="$(jq -r --arg id "$child_id" '.[] | select(.id == $id) | .status' "$tmp/body")"
  [[ "$s" != failed ]] || die "the child branch failed"
  if [[ "$s" == ready ]]; then
    child_host="$(jq -r --arg id "$child_id" '.[] | select(.id == $id) | .host // empty' "$tmp/body")"
    break
  fi
  sleep 1
done
[[ -n "$child_host" ]] || die "the child branch isn't ready with a host after 180 s"
connects "$LOGIN" "$LOGIN_PW" "$child_host" "the child branch"
pass "the child branch $child_id has the login, with the same password"

# --- 4. Reset: the old password is refused at once, the new one works --------------------------

old_pw="$LOGIN_PW"
reset_login "$DEV_TOKEN" "$LOGIN"
refused "$LOGIN" "$old_pw" "$MAIN_HOST" "the old password on main after a reset"
refused "$LOGIN" "$old_pw" "$child_host" "the old password on the child after a reset"
connects "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" "the new password on main"
connects "$LOGIN" "$LOGIN_PW" "$child_host" "the new password on the child"
pass "reset: the old password is refused at once on both branches, the new one works"

# --- 5. The grant deleted while main's compute is stopped ----------------------------------------

main_ep="${MAIN_HOST%%.*}"
cid="$(docker ps -q --filter "label=chelabase.dev/stack=$CHELABASE_STACK" --filter "label=chelabase.dev/endpoint-id=$main_ep")"
[[ -n "$cid" ]] || die "no running compute container for $main_ep"
docker stop "$cid" >/dev/null
stopped="$cid"
s="$(api DELETE "/v1/projects/$project_id/grants/$MEMBER_ID")"
[[ "$s" == 200 || "$s" == 204 ]] || die "delete grant: $s $(cat "$tmp/body")"
refused "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" "the login after its grant went, compute stopped"
[[ "$(docker inspect -f '{{.State.Running}}' "$cid")" == false ]] ||
  die "the refused login started main's compute"
docker start "$cid" >/dev/null
stopped=""
pass "grant deleted with main's compute stopped: the proxy refuses the login and the compute stays stopped (MH-9)"

# --- 6. Re-granted, a new login; suspend; reinstate ----------------------------------------------

grant '["billing"]'
DEV_TOKEN="$(member_token)"
new_login "$DEV_TOKEN" "$LOGIN"
connects "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" "a new login after the grant came back"
ok POST "/v1/members/$MEMBER_ID/suspend" 200
jq -e '.status == "suspended"' "$tmp/body" >/dev/null || die "suspend: $(cat "$tmp/body")"
refused "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" "the login of a suspended member"
s="$(uapi "$DEV_TOKEN" GET "/v1/projects/$project_id")"
[[ "$s" == 403 && "$(jq -r '.error.code' "$tmp/body")" == member_suspended ]] ||
  die "a suspended member's API call: $s $(cat "$tmp/body")"
ok POST "/v1/members/$MEMBER_ID/reinstate" 200
jq -e '.status == "active"' "$tmp/body" >/dev/null || die "reinstate: $(cat "$tmp/body")"
for _ in $(seq 5); do
  refused "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" "the revoked login after reinstate"
  sleep 2
done
DEV_TOKEN="$(member_token)"
new_login "$DEV_TOKEN" "$LOGIN"
connects "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" "a new login after reinstate"
pass "suspend refuses the login and the API (member_suspended); reinstate keeps it refused until dev makes a new one"

# --- 7. Revoke Postgres access now -----------------------------------------------------------------

for _ in $(seq 30); do # the member role is made on first use
  s="$(stream "$DEV_TOKEN" "$tmp/events" 60 "$(run_body billing "SELECT 1")")"
  [[ "$s" == 200 ]] && break
  sleep 2
done
[[ "$s" == 200 ]] || die "dev's SQL-editor run before revoke-now: $s $(cat "$tmp/events")"
stream "$DEV_TOKEN" "$tmp/long" 110 "$(run_body billing "SELECT pg_sleep(100)" 120000)" >"$tmp/long.status" &
bg_pid=$!
sleep 5
t_revoke=$SECONDS
ok POST "/v1/members/$MEMBER_ID/revoke-postgres-access" 200
jq -e '.revoked == 1 and (.rotated_branches | type) == "number"' "$tmp/body" >/dev/null ||
  die "revoke-now: $(cat "$tmp/body")"
refused "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" "the login after revoke-now"
wait "$bg_pid" || true
bg_pid=""
ended=$((SECONDS - t_revoke))
((ended < 80)) || die "the open SQL-editor session was still running $ended s after revoke-now"
if grep -Eq '"outcome" *: *"ok"' "$tmp/long"; then
  die "the open SQL-editor session finished ok after revoke-now: $(tail -c 400 "$tmp/long")"
fi
s=""
for _ in $(seq 30); do # the next run reconnects with the member role's new password
  s="$(stream "$DEV_TOKEN" "$tmp/events" 60 "$(run_body billing "SELECT 1")")"
  [[ "$s" == 200 ]] && jq -s -e '.[-1].outcome == "ok"' "$tmp/events" >/dev/null && break
  sleep 2
done
jq -s -e '.[-1].outcome == "ok"' "$tmp/events" >/dev/null || die "dev's SQL-editor run after revoke-now: $s $(cat "$tmp/events")"
pass "revoke-now: the login is refused, the open SQL-editor session ended ($ended s after), the next run works"

# --- 8. An expired VALID UNTIL, renewed by a reset -----------------------------------------------

new_login "$DEV_TOKEN" "$LOGIN"
connects "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" "a new login after revoke-now"
metadata_sql -v m="$MEMBER_ID" -v d="$db_id" >/dev/null <<<"UPDATE personal_logins SET valid_until = now() - interval '1 minute' WHERE member_id = :'m' AND database_id = :'d';" ||
  die "couldn't expire the login in the metadata"
# Nobody tells the proxy's cache about this write: its entry expires within its TTL.
refused "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" "an expired login" 120
reset_login "$DEV_TOKEN" "$LOGIN"
connects "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" "the login renewed by a reset"
pass "an expired login is refused; a reset renews it"

# --- 9. Offboarded in Zitadel ------------------------------------------------------------------------

s="$(zitadel POST "/v2/users/$ZUSER/deactivate" '{}')"
[[ "$s" == 200 ]] || die "Zitadel deactivate: $s $(cat "$tmp/zbody")"
refused "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" "the login of a user deactivated in Zitadel" 60
s="$(api GET "/v1/members/$MEMBER_ID/personal-logins")"
[[ "$s" == 200 ]] || die "dev's logins as the admin: $s $(cat "$tmp/body")"
jq -e --arg n "$LOGIN" 'any(.logins[]; .role_name == $n and .status == "revoked" and .revoked_reason == "identity_offboarded")' "$tmp/body" >/dev/null ||
  die "the login isn't revoked as identity_offboarded: $(cat "$tmp/body")"
pass "deactivated in Zitadel: the webhook revoked the login (identity_offboarded)"

echo "all passed ($((SECONDS - t0)) s)"
