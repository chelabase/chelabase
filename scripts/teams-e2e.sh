#!/usr/bin/env bash
# Live check of live-linked teams (members hub step 2, spec Part 5), through the gateway and
# Neon's proxy (needs the stack: just workbench up; it runs in the end-of-step live pass):
# 1. a project, a database with NOLOGIN roles `billing_editor` (owns billing.t, writes it) and
#    `billing_reader` (SELECT (id) only on billing.t), and a member with no own grant;
# 2. a team granting Developer {billing_editor}; the member joins it and runs SQL as
#    billing_editor (allowed), and makes a personal login (Developer through the team);
# 3. the team grant downgraded to Viewer: RUN_SQL as billing_editor is 403, the member role
#    cb_m_<id> loses billing_editor in pg_auth_members after the reconcile (T2-2), and the
#    personal login is refused by the proxy (MH-9);
# 4. a second team grants Viewer {billing_reader}: the member reads a full cell as
#    billing_reader (`id`: 200, through the data API's connection, Q1) and is refused the
#    column it can't select (403 permission_denied);
# 5. removed from both teams: /v1/me lists no project; the teams are deleted;
# 6. a project invite with `team_ids` is 422 (MH-8).
# Everything it makes is removed on exit, also on failure. KEEP=1 leaves the project.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=teams-e2e
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
editors=""
readers=""
t0=$SECONDS
step=$SECONDS

pass() {
  echo "PASS $* ($((SECONDS - step)) s, $((SECONDS - t0)) s total)"
  step=$SECONDS
}

[[ -x "${CHELABASE_CLI:-}" ]] || die "CHELABASE_CLI is unset or not executable (run through: just workbench run scripts/<name>.sh)"
ca="${CHELABASE_PROXY_CA_FILE:?run through just workbench run}"
[[ -s "$ca" ]] || die "$ca is missing (run just workbench up)"
# shellcheck source=lib/member-sign-in.sh
. scripts/lib/member-sign-in.sh

cleanup() {
  local rc=$?
  set +e
  [[ -z "$editors" ]] || api DELETE "/v1/teams/$editors" >/dev/null
  [[ -z "$readers" ]] || api DELETE "/v1/teams/$readers" >/dev/null
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

# pg <user> <password> <host> <sql>: psql through the proxy (output on stdout, errors in
# $tmp/err). The password goes through the environment, never argv.
pg() {
  PGPASSWORD="$2" PGCONNECT_TIMEOUT=10 psql \
    "host=$3 hostaddr=$PROXY_HOSTADDR port=$PROXY_PG_PORT user=$1 dbname=$dbname sslmode=verify-full sslrootcert=$ca" \
    -v ON_ERROR_STOP=1 -qtA -c "$4" 2>"$tmp/err"
}

# connects <user> <password> <host> <what>: until `select 1` works, 120 s at most.
connects() {
  for _ in $(seq 60); do
    pg "$1" "$2" "$3" 'select 1' >/dev/null && return 0
    sleep 2
  done
  die "$4: $1 never connected on $3: $(cat "$tmp/err")"
}

# refused <user> <password> <host> <what>: until the proxy refuses the password (30 s at most).
refused() {
  for _ in $(seq 30); do
    if ! pg "$1" "$2" "$3" 'select 1' >/dev/null && grep -q "password authentication failed for user" "$tmp/err"; then
      return 0
    fi
    sleep 1
  done
  die "$4: $1 isn't refused on $3: $(cat "$tmp/err")"
}

# run_sql <token> <role> <sql> → the status; the events in $tmp/events.
run_sql() {
  local body
  body="$(jq -nc --arg id "$(cat /proc/sys/kernel/random/uuid)" --arg sql "$3" --arg role "$2" '{run_id: $id, sql: $sql, role: $role}')"
  printf 'Authorization: Bearer %s\n' "$1" |
    curl -sSN --max-time 60 -o "$tmp/events" -w '%{http_code}' -H @- -H 'content-type: application/json' \
      -X POST -d "$body" "$GATEWAY/v1/databases/$db_id/branches/$db_id/sql" || true
}

# team_grant <team> <platform role> <roles json>: the team's grant on the project, retried while
# the roles aren't recorded yet (the DDL that made them is forwarded to the metadata as it runs).
team_grant() {
  local s=""
  for _ in $(seq 30); do
    s="$(api PUT "/v1/teams/$1/grants/$project_id" "{\"platform_role\":\"$2\",\"pg_roles\":$3}")"
    [[ "$s" == 200 ]] && return 0
    sleep 1
  done
  die "team grant $2 $3: $s $(cat "$tmp/body")"
}

# member_holds <role> → 0 when cb_m_<member> is a member of <role> (as the owner).
member_holds() {
  local n
  n="$(pg "$owner" "$owner_pw" "$owner_host" "SELECT count(*) FROM pg_auth_members a
    JOIN pg_roles r ON r.oid = a.roleid JOIN pg_roles u ON u.oid = a.member
    WHERE r.rolname = '$1' AND u.rolname = '$member_role'")" || die "pg_auth_members: $(cat "$tmp/err")"
  [[ "$n" == 1 ]]
}

# --- 1. A project, a database, the roles, a member with no own grant ---------------------------

name="teams_e2e_$(hex 2)"
ok POST /v1/projects 201 "$(jq -nc --arg n "$name" '{name: $n}')"
project_id="$(jq -r '.id' "$tmp/body")"
slug="$(jq -r '.slug' "$tmp/body")"
[[ -n "$project_id" && "$project_id" != null && -n "$slug" ]] || die "POST /v1/projects: $(cat "$tmp/body")"
s="$(api POST /v1/databases "$(jq -nc --arg p "$project_id" --arg n "tm_$(hex 2)" '{project_id: $p, name: $n}')")"
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
pg "$owner" "$owner_pw" "$owner_host" "CREATE ROLE billing_editor NOLOGIN;
  CREATE ROLE billing_reader NOLOGIN;
  CREATE SCHEMA billing AUTHORIZATION billing_editor;
  CREATE TABLE billing.t(id int PRIMARY KEY, secret text);
  ALTER TABLE billing.t OWNER TO billing_editor;
  INSERT INTO billing.t VALUES (1, 'not for readers');
  GRANT USAGE ON SCHEMA billing TO billing_reader;
  GRANT SELECT (id) ON billing.t TO billing_reader" >/dev/null || die "setup: $(cat "$tmp/err")"

# new_member always grants; the member's own grant goes again at once (team-only from here).
new_member "$project_id" '{"platform_role":"viewer","pg_roles":[]}'
s="$(api DELETE "/v1/projects/$project_id/grants/$MEMBER_ID")"
[[ "$s" == 204 ]] || die "delete the member's own grant: $s $(cat "$tmp/body")"
member_role="cb_m_${MEMBER_ID//-/}"
TOKEN="$(member_token)"
pass "project $name, database $db_id with billing_editor and billing_reader, member $MEMBER_ID with no own grant"

# --- 2. A team grants Developer {billing_editor}; the member joins and uses it ------------------

ok POST /v1/teams 201 "$(jq -nc --arg n "Editors $(hex 2)" '{name: $n}')"
editors="$(jq -r '.id' "$tmp/body")"
team_grant "$editors" developer '["billing_editor"]'
ok PUT "/v1/teams/$editors/members/$MEMBER_ID" 200
jq -e --arg m "$MEMBER_ID" '.member_ids | index($m) != null' "$tmp/body" >/dev/null || die "the member isn't in the team: $(cat "$tmp/body")"
s=""
for _ in $(seq 30); do # the member role is made on first use
  s="$(run_sql "$TOKEN" billing_editor "INSERT INTO billing.t VALUES (2, 'by the team')")"
  [[ "$s" == 200 ]] && jq -s -e '.[-1].outcome == "ok"' "$tmp/events" >/dev/null && break
  sleep 2
done
jq -s -e '.[-1].outcome == "ok"' "$tmp/events" >/dev/null || die "SQL as billing_editor through the team: $s $(cat "$tmp/events")"
member_holds billing_editor || die "$member_role isn't a member of billing_editor after the run"
LOGIN="tm_$(hex 2)"
s="$(uapi "$TOKEN" PUT "/v1/databases/$db_id/personal-login" "$(jq -nc --arg n "$LOGIN" '{name: $n}')")"
[[ "$s" == 201 ]] || die "personal login through the team: $s $(cat "$tmp/body")"
LOGIN_PW="$(jq -r .password "$tmp/body")"
MAIN_HOST="$(jq -r .connection_string "$tmp/body" | sed -E 's#^postgresql://[^@]+@([^:/]+).*#\1#')"
rm -f "$tmp/body"
connects "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" "the personal login"
pass "team Editors (Developer {billing_editor}): the member runs SQL as billing_editor and has a personal login"

# --- 3. Downgraded to Viewer: refused, revoked, reconciled ---------------------------------------

team_grant "$editors" viewer '["billing_editor"]'
s="$(run_sql "$TOKEN" billing_editor "SELECT 1")"
[[ "$s" == 403 ]] || die "RUN_SQL as a Viewer-level billing_editor: $s $(cat "$tmp/events")"
refused "$LOGIN" "$LOGIN_PW" "$MAIN_HOST" "the personal login after the downgrade (MH-9)"
for _ in $(seq 60); do
  member_holds billing_editor || break
  sleep 2
done
if member_holds billing_editor; then die "$member_role still holds billing_editor 120 s after the downgrade (T2-2)"; fi
pass "downgraded to Viewer: RUN_SQL is 403, the login is refused, $member_role no longer holds billing_editor"

# --- 4. A Viewer team role reads a full cell through the data API -------------------------------

ok POST /v1/teams 201 "$(jq -nc --arg n "Readers $(hex 2)" '{name: $n}')"
readers="$(jq -r '.id' "$tmp/body")"
team_grant "$readers" viewer '["billing_reader"]'
ok PUT "/v1/teams/$readers/members/$MEMBER_ID" 200
cell="/v1/databases/$db_id/branches/$db_id/tables/billing/t/cell"
s=""
for _ in $(seq 30); do # the role check grants billing_reader to authenticator on first use
  s="$(uapi "$TOKEN" POST "$cell" '{"role":"billing_reader","key":{"id":1},"column":"id"}')"
  [[ "$s" == 200 ]] && break
  sleep 2
done
[[ "$s" == 200 ]] || die "the full cell as billing_reader: $s $(cat "$tmp/body")"
s="$(uapi "$TOKEN" POST "$cell" '{"role":"billing_reader","key":{"id":1},"column":"secret"}')"
[[ "$s" == 403 && "$(jq -r '.error.code' "$tmp/body")" == permission_denied ]] ||
  die "the column billing_reader can't select: $s $(cat "$tmp/body")"
pass "team Readers (Viewer {billing_reader}): a full cell of id reads through the data API, secret is 403 permission_denied"

# --- 5. Out of the teams: no project; the teams deleted -----------------------------------------

ok DELETE "/v1/teams/$editors/members/$MEMBER_ID" 204
ok DELETE "/v1/teams/$readers/members/$MEMBER_ID" 204
TOKEN="$(member_token)"
s="$(uapi "$TOKEN" GET /v1/me)"
[[ "$s" == 200 ]] || die "the member's /v1/me: $s $(cat "$tmp/body")"
jq -e '.projects == []' "$tmp/body" >/dev/null || die "the member still lists projects: $(cat "$tmp/body")"
ok DELETE "/v1/teams/$editors" 204
editors=""
ok DELETE "/v1/teams/$readers" 204
readers=""
pass "removed from both teams: /v1/me lists no project; both teams deleted"

# --- 6. MH-8: a project invite never attaches teams ---------------------------------------------

s="$(api POST "/v1/projects/$project_id/invites" "$(jq -nc --arg e "teams-e2e-$(hex 3)@example.com" \
  '{email: $e, platform_role: "viewer", pg_roles: [], team_ids: []}')")"
[[ "$s" == 422 && "$(jq -r '.error.code' "$tmp/body")" == invalid_request ]] ||
  die "a project invite with team_ids: $s $(cat "$tmp/body")"
pass "a project invite with team_ids is 422 (MH-8)"

echo "all passed ($((SECONDS - t0)) s)"
