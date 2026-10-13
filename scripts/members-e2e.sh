#!/usr/bin/env bash
# Live check of the Members step (invites, grants, act-as in the SQL editor, revoke, open
# sign-up and approval), through the gateway and auth (needs just deps-up and just dev, with
# Mailpit as Zitadel's SMTP provider):
# 1. the bootstrap admin (the demo token's user, made a tenant admin by `dev bootstrap`) can
#    invite by email;
# 2. a project with a database, a group role `billing` owning schema `billing` with a table;
# 3. `dev` is invited by email (the link is read from Mailpit's API) and accepts; a second
#    person is invited by link (`delivery: "link"`, `invite_url`) and accepts;
# 4. the admin grants `dev` Developer with `billing`;
# 5. in the SQL editor `dev` acts as billing: INSERT into billing.t works, public and another
#    schema are permission denied, the owner is 403 `forbidden`, and a script that does
#    RESET ROLE then CREATE in billing is denied and in a PUBLIC-writable schema leaves nothing
#    owned by the member's cb_m_* role;
# 6. revoking the grant refuses the next request and ends an open SQL-editor session; removing
#    the member answers 404 `not_found` to their old token (`/v1/me` says `removed`) and ends a
#    running session; invited again, they stay out until they accept (I1);
# 7. with open sign-up off the auth request says signUpAllowed:false and sign-up answers
#    `sign_up_closed`; turned on, a third person signs up as `pending` (403 `pending_approval`),
#    the admin approves them with a grant and they read the project; a fourth is declined and
#    refused; the access log has the events; open sign-up goes off again.
# Invites are also described (POST /auth/v1/invite), resent and revoked (step 3b).
# Everything it makes is removed on exit, also on failure. KEEP=1 leaves the project.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=members-e2e
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
GATEWAY="${GATEWAY_URL:-$CHELABASE_ENTRY_API_URL}"
GW="$GATEWAY"
MAILPIT="${MAILPIT_URL:-$CHELABASE_ENTRY_MAIL_URL}"
STUDIO="$CHELABASE_ENTRY_STUDIO_URL"
tmp="$(mktemp -d)"
JAR="$tmp/jar"
STATUS_FILE="$tmp/ui-status"
project_id=""
slug=""
db_id=""
bg_pid=""
signup_opened=""
member_ids=()
t0=$SECONDS
step=$SECONDS

pass() {
  echo "PASS $* ($((SECONDS - step)) s, $((SECONDS - t0)) s total)"
  step=$SECONDS
}

cleanup() {
  local rc=$?
  set +e
  [[ -z "$bg_pid" ]] || kill "$bg_pid" 2>/dev/null
  if [[ -n "$signup_opened" ]]; then
    api PATCH /v1/instance/settings '{"open_signup":false}' >/dev/null
  fi
  local m
  for m in "${member_ids[@]}"; do
    api DELETE "/v1/members/$m" >/dev/null
  done
  if [[ -n "$project_id" && "${KEEP:-}" != 1 ]]; then
    api DELETE "/v1/projects/$project_id" "$(jq -nc --arg s "$slug" '{confirm: $s}')" >/dev/null
  fi
  rm -rf "$tmp"
  exit "$rc"
}
trap cleanup EXIT

cargo build -q -p chelabase || die "cargo build -p chelabase"
[[ -f .chelabase/zitadel/demo.env ]] || die ".chelabase/zitadel/demo.env is missing (run just dev)"
set -a
# shellcheck disable=SC1091
source .chelabase/zitadel/demo.env
set +a
# Studio's Zitadel app: VITE_ZITADEL_CLIENT_ID when set, else the one in Studio's .env.local.
CLIENT="${VITE_ZITADEL_CLIENT_ID:-$(sed -n 's/^VITE_ZITADEL_CLIENT_ID=//p' "$root/apps/studio/.env.local")}"
ISS="$CHELABASE_ENTRY_AUTH_URL" # the issuer, derived from the entry
suffix="$(date +%s)-$RANDOM"
DEV_EMAIL="members-dev-$suffix@example.com"
LINK_EMAIL="members-link-$suffix@example.com"
SIGNUP_EMAIL="members-signup-$suffix@example.com"
REVOKE_EMAIL="members-revoke-$suffix@example.com"
DECLINE_EMAIL="members-decline-$suffix@example.com"
PASSWORD="E2e-pass-$(openssl rand -hex 4)A!"

# ok <method> <path> <expected status> [body]: the admin's call, or die.
ok() {
  local status
  status="$(api "$1" "$2" "${4:-}")"
  [[ "$status" == "$3" ]] || die "$1 $2: $status (wanted $3) $(cat "$tmp/body")"
}

# --- Sign-in helpers (as scripts/auth-e2e.sh) ---------------------------------------------------

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
VERIFIER="$(openssl rand -hex 32)"
CHALLENGE="$(printf %s "$VERIFIER" | openssl dgst -sha256 -binary | b64url)"

# authorize [prompt] → the authRequest id from Zitadel's redirect to Studio's /login
authorize() {
  local scope="openid profile email urn:zitadel:iam:user:resourceowner urn:zitadel:iam:org:project:id:${CHELABASE_ZITADEL_PROJECT_ID}:aud"
  local loc
  loc="$(curl -sS -o /dev/null -w '%{redirect_url}' -G "$ISS/oauth/v2/authorize" \
    --data-urlencode "client_id=$CLIENT" --data-urlencode "redirect_uri=$STUDIO/callback" \
    --data-urlencode "response_type=code" --data-urlencode "scope=$scope" \
    --data-urlencode "code_challenge=$CHALLENGE" --data-urlencode "code_challenge_method=S256" \
    ${1:+--data-urlencode "prompt=$1"})"
  [[ "$loc" == "$STUDIO/login?authRequest="* ]] || die "authorize redirected to: $loc"
  echo "${loc#*authRequest=}"
}

# ui <path> <json> → body; its status via `status` (a file: `$(ui …)` runs in a subshell)
ui() {
  local out
  out="$(curl -sS -b "$JAR" -c "$JAR" -H "Origin: $STUDIO" -H 'Content-Type: application/json' \
    -w '\n%{http_code}' -X POST "$GW/auth/v1/$1" -d "$2")"
  printf '%s' "${out##*$'\n'}" >"$STATUS_FILE"
  printf '%s' "${out%$'\n'*}"
}
status() { cat "$STATUS_FILE"; }

# mail_link <to> <kind: accept-invite|verify|password-reset> [not this link] → the link in the
# newest such email (waits while that is still the link given as the third argument)
mail_link() {
  local id found
  for _ in $(seq 30); do
    id="$(curl -sS -G "$MAILPIT/api/v1/search" --data-urlencode "query=to:$1" | jq -r '.messages[0].ID // empty')"
    if [[ -n "$id" ]]; then
      # The mail may carry the link HTML-escaped (&amp; between query parameters).
      found="$(curl -sS "$MAILPIT/api/v1/message/$id" | jq -r '.Text' | grep -oE "$STUDIO/$2\?[^ \"<>]+" | head -n1 | sed 's/&amp;/\&/g')" || true
      if [[ -n "$found" && "$found" != "${3:-}" ]]; then
        echo "$found"
        return 0
      fi
    fi
    sleep 1
  done
  die "no $2 email for $1"
}

# exchange <callback url> → access token
exchange() {
  local code
  code="$(sed -E 's/.*[?&]code=([^&]+).*/\1/' <<<"$1")"
  curl -sS -X POST "$ISS/oauth/v2/token" -d grant_type=authorization_code -d "code=$code" \
    -d "redirect_uri=$STUDIO/callback" -d "client_id=$CLIENT" -d "code_verifier=$VERIFIER" | jq -r .access_token
}

# accept_invite <invite link> <password> → access token (accepts the invite and finishes the sign-in)
# The way Studio's /accept-invite does it for a link opened from an email, which carries no
# pending sign-in: describe the invite with the link's token (a closed one stops here), start
# Studio's own sign-in (Zitadel's redirect to /login hands back an authRequest; Studio's /login
# sends it to the accept page), then accept with that authRequest, which ends signed in.
accept_invite() {
  local uid code t ar out redirect token reset
  [[ "$1" != *"authRequest="* ]] || die "an invite link carries a pending sign-in: $1"
  uid="$(sed -E 's/.*[?&]user=([^&]+).*/\1/' <<<"$1")"
  code="$(sed -E 's/.*[&?]code=([^&]+).*/\1/' <<<"$1")"
  t="$(sed -nE 's/.*[?&]t=([^&]+).*/\1/p' <<<"$1")"
  if [[ -n "$t" ]]; then
    ui invite "$(jq -nc --arg u "$uid" --arg t "$t" '{user: $u, t: $t}')" >/dev/null
    [[ "$(status)" == 200 ]] || die "describe-invite for an open invite: $(status)"
  fi
  ar="$(authorize)"
  # reset=1: a password-reset code (someone who already had a password, invited again)
  reset=false
  [[ "$1" == *"reset=1"* ]] && reset=true
  out="$(ui accept-invite "{\"authRequest\":\"$ar\",\"user\":\"$uid\",\"code\":\"$code\",\"reset\":$reset,\"password\":\"$2\"}")"
  [[ "$(status)" == 200 ]] || die "accept-invite: $(status) $out"
  redirect="$(jq -r .redirect <<<"$out")"
  [[ "$redirect" == "$STUDIO/callback?"* ]] || die "accept-invite didn't finish: $out"
  token="$(exchange "$redirect")"
  [[ -n "$token" && "$token" != null ]] || die "no access token after accept-invite"
  echo "$token"
}

# login_token <email> <password> → a fresh access token (tokens are short-lived)
login_token() {
  local out token
  out="$(ui sign-in "{\"authRequest\":\"$(authorize)\",\"email\":\"$1\",\"password\":\"$2\"}")"
  # Sign-in allows 10 attempts a minute per address, and the e2e scripts before this one share
  # it: wait the window out once.
  if [[ "$(jq -r '.error.code // empty' <<<"$out" 2>/dev/null)" == rate_limited ]]; then
    sleep 61
    out="$(ui sign-in "{\"authRequest\":\"$(authorize)\",\"email\":\"$1\",\"password\":\"$2\"}")"
  fi
  [[ "$(jq -r .next <<<"$out")" == done ]] || die "sign-in as $1: $out"
  token="$(exchange "$(jq -r .redirect <<<"$out")")"
  [[ -n "$token" && "$token" != null ]] || die "no access token for $1"
  echo "$token"
}

# --- Calls as a person ---------------------------------------------------------------------------

# uapi <token> <method> <path> [json body]: prints the status; the body goes to $tmp/body.
uapi() {
  local args=(-sS --max-time 60 -o "$tmp/body" -w '%{http_code}' -X "$2" "$GATEWAY$3")
  if [[ -n "${4:-}" ]]; then args+=(-H 'content-type: application/json' -d "$4"); fi
  printf 'Authorization: Bearer %s\n' "$1" | curl "${args[@]}" -H @- || true
}

# stream <token> <out file> <max seconds> <json body>: POSTs a run; prints the status. The token
# goes through stdin, never argv.
stream() {
  printf 'Authorization: Bearer %s\n' "$1" |
    curl -sSN --max-time "$3" -o "$2" -w '%{http_code}' -H @- -H 'content-type: application/json' \
      -X POST -d "$4" "$GATEWAY$branch/sql" || true
}

# run_body <role or ""> <sql> [timeout_ms] → the request body
run_body() {
  jq -nc --arg id "$(cat /proc/sys/kernel/random/uuid)" --arg sql "$2" --arg role "$1" --arg t "${3:-}" \
    '{run_id: $id, sql: $sql} + (if $role == "" then {} else {role: $role} end)
     + (if $t == "" then {} else {timeout_ms: ($t | tonumber)} end)'
}

# run <token> <role or ""> <sql>: prints the status; events go to $tmp/events.
run() { stream "$1" "$tmp/events" 60 "$(run_body "$2" "$3")"; }

# run_ok <token> <role or ""> <sql> <jq filter on the events array>: a 200 whose events pass.
run_ok() {
  local s
  s="$(run "$1" "$2" "$3")"
  [[ "$s" == 200 ]] || die "run '$3' as '${2:-owner}': $s $(cat "$tmp/events")"
  jq -s -e "$4" "$tmp/events" >/dev/null || die "run '$3' as '${2:-owner}': events fail $4: $(jq -sc . "$tmp/events")"
}

# denied <token> <role> <sql>: a 200 whose script ends in a permission denied (42501).
denied() {
  run_ok "$1" "$2" "$3" 'any(.[]; .t == "error" and .code == "42501") and (.[-1].outcome == "error")'
}

# --- 1. The bootstrap admin -----------------------------------------------------------------------

ok GET /v1/me 200
jq -e '.member.tenant_admin == true' "$tmp/body" >/dev/null ||
  die "the demo token's user isn't a tenant admin: re-run just zitadel-bootstrap"
ok GET /v1/invites/delivery 200
jq -e '.email_available == true' "$tmp/body" >/dev/null ||
  die "invites can't be emailed: is Mailpit the active SMTP provider? $(cat "$tmp/body")"
pass "the demo user is a tenant admin and invites can be emailed"

# --- 2. A project, a database, the group role billing ----------------------------------------------

name="members_e2e_$(hex 2)"
ok POST /v1/projects 201 "$(jq -nc --arg n "$name" '{name: $n}')"
project_id="$(jq -r '.id' "$tmp/body")"
slug="$(jq -r '.slug' "$tmp/body")"
[[ -n "$project_id" && "$project_id" != null && -n "$slug" ]] || die "POST /v1/projects: $(cat "$tmp/body")"
status="$(api POST /v1/databases "$(jq -nc --arg p "$project_id" --arg n "m_$(hex 2)" \
  '{project_id: $p, name: $n}')")"
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

admin_token() { cargo run -q -p chelabase -- dev token || die "chelabase dev token"; }
SETUP="CREATE ROLE billing NOLOGIN;
  CREATE SCHEMA billing AUTHORIZATION billing;
  CREATE TABLE billing.t(id int PRIMARY KEY, note text);
  ALTER TABLE billing.t OWNER TO billing;
  CREATE SCHEMA other;
  CREATE TABLE other.t(id int);
  CREATE SCHEMA open;
  GRANT USAGE, CREATE ON SCHEMA open TO PUBLIC"
atok="$(admin_token)"
for _ in $(seq 30); do # the compute may still be starting: the first run can fail to connect
  s="$(run "$atok" "" "$SETUP")"
  [[ "$s" == 200 ]] && break
  sleep 2
done
[[ "$s" == 200 ]] || die "setup run: $s $(cat "$tmp/events")"
jq -s -e '.[-1].outcome == "ok"' "$tmp/events" >/dev/null || die "setup script: $(jq -sc . "$tmp/events")"
pass "project $name ($project_id), database $db_id, group role billing owning schema billing with billing.t"

# --- 3. Invites: by email (Mailpit) and by link ----------------------------------------------------

ok POST /v1/invites 201 "$(jq -nc --arg e "$DEV_EMAIL" '{email: $e, grants: [], delivery: "email"}')"
jq -e '.outcome == "invited" and .member.status == "invited" and (.invite_url == null)' "$tmp/body" >/dev/null ||
  die "email invite: $(cat "$tmp/body")"
dev_member="$(jq -r '.member.id' "$tmp/body")"
member_ids+=("$dev_member")
link="$(mail_link "$DEV_EMAIL" accept-invite)"
DEV_TOKEN="$(accept_invite "$link" "$PASSWORD")"
s="$(uapi "$DEV_TOKEN" GET /v1/me)"
[[ "$s" == 200 ]] || die "dev /v1/me: $s $(cat "$tmp/body")"
jq -e --arg m "$dev_member" '.member.id == $m and .member.status == "active" and .member.tenant_admin == false' "$tmp/body" >/dev/null ||
  die "dev's membership after accepting: $(cat "$tmp/body")"
pass "$DEV_EMAIL invited by email, the link read from Mailpit, accepted: an active member"

ok POST /v1/invites 201 "$(jq -nc --arg e "$LINK_EMAIL" '{email: $e, grants: [], delivery: "link"}')"
url="$(jq -r '.invite_url // empty' "$tmp/body")"
[[ "$url" == "$STUDIO/accept-invite?user="* ]] || die "no invite link: $(cat "$tmp/body")"
link_member="$(jq -r '.member.id' "$tmp/body")"
member_ids+=("$link_member")
LINK_TOKEN="$(accept_invite "$url" "$PASSWORD")"
s="$(uapi "$LINK_TOKEN" GET /v1/me)"
[[ "$s" == 200 ]] || die "link invitee /v1/me: $s $(cat "$tmp/body")"
jq -e --arg m "$link_member" '.member.id == $m and .member.status == "active"' "$tmp/body" >/dev/null ||
  die "link invitee's membership: $(cat "$tmp/body")"
s="$(uapi "$LINK_TOKEN" GET "/v1/projects/$project_id")"
[[ "$s" == 403 || "$s" == 404 ]] || die "a member without a grant read the project: $s $(cat "$tmp/body")"
pass "$LINK_EMAIL invited by link (invite_url), accepted: active, and no access to the project yet"

# --- 3b. Describe, resend and revoke an invite -------------------------------------------------------

# query_param <url> <name> → the value of that query parameter
query_param() { sed -nE "s/.*[?&]$2=([^&]+).*/\\1/p" <<<"$1"; }

ok POST /v1/invites 201 "$(jq -nc --arg e "$REVOKE_EMAIL" '{email: $e, grants: [], delivery: "link"}')"
revoke_url="$(jq -r '.invite_url // empty' "$tmp/body")"
revoke_member="$(jq -r '.member.id' "$tmp/body")"
member_ids+=("$revoke_member")
revoke_invite="$(jq -r '.member.invite.id // empty' "$tmp/body")"
[[ -n "$revoke_url" && -n "$revoke_invite" ]] || die "link invite without a url or invite id: $(cat "$tmp/body")"
revoke_user="$(query_param "$revoke_url" user)"
revoke_t="$(query_param "$revoke_url" t)"
[[ -n "$revoke_user" && -n "$revoke_t" ]] || die "the invite link carries no user or t: $revoke_url"
described="$(ui invite "$(jq -nc --arg u "$revoke_user" --arg t "$revoke_t" '{user: $u, t: $t}')")"
# The inviter is the demo machine user (no name, no email): named "An admin", with a null email.
[[ "$(status)" == 200 ]] && jq -e '(.tenant_name | type) == "string" and (.inviter_name | type) == "string"
    and .inviter_name != "" and ((.inviter_email | type) == "string" or .inviter_email == null)' <<<"$described" >/dev/null ||
  die "describe-invite (POST /auth/v1/invite): $(status) $described"
described="$(ui invite "$(jq -nc --arg u "$revoke_user" '{user: $u, t: "not-the-token"}')")"
[[ "$(status)" == 404 ]] || die "describe-invite with a wrong token: $(status) $described"
pass "describe-invite: the token in the link opens it, a wrong one is 404"

ok POST "/v1/invites/$revoke_invite/resend" 200 '{"delivery":"link"}'
new_url="$(jq -r '.invite_url // empty' "$tmp/body")"
new_t="$(query_param "$new_url" t)"
[[ -n "$new_url" && -n "$new_t" && "$new_t" != "$revoke_t" ]] || die "resend didn't give a new link and token: $(cat "$tmp/body")"
ui invite "$(jq -nc --arg u "$revoke_user" --arg t "$revoke_t" '{user: $u, t: $t}')" >/dev/null
[[ "$(status)" == 404 ]] || die "the old token still describes the invite after a resend: $(status)"
ui invite "$(jq -nc --arg u "$revoke_user" --arg t "$new_t" '{user: $u, t: $t}')" >/dev/null
[[ "$(status)" == 200 ]] || die "the new token doesn't describe the invite: $(status)"
pass "resend: a new link and token, the old token stops working"

s="$(api DELETE "/v1/invites/$revoke_invite")"
[[ "$s" == 204 ]] || die "revoke invite: $s $(cat "$tmp/body")"
ui invite "$(jq -nc --arg u "$revoke_user" --arg t "$new_t" '{user: $u, t: $t}')" >/dev/null
[[ "$(status)" == 404 ]] || die "a revoked invite is still described: $(status)"
ok GET /v1/members 200
jq -e --arg m "$revoke_member" 'all(.[]; .id != $m or .status == "removed")' "$tmp/body" >/dev/null ||
  die "the revoked invitee isn't removed: $(cat "$tmp/body")"
pass "revoke: the invite is gone (describe is 404) and the invitee is removed"

# --- 4. The grant -------------------------------------------------------------------------------------

s="$(uapi "$DEV_TOKEN" GET "/v1/projects/$project_id")"
[[ "$s" == 403 || "$s" == 404 ]] || die "dev read the project before a grant: $s $(cat "$tmp/body")"
ok PUT "/v1/projects/$project_id/grants/$dev_member" 200 '{"platform_role":"developer","pg_roles":["billing"]}'
jq -e '.platform_role == "developer" and (.pg_roles | index("billing") != null)' "$tmp/body" >/dev/null ||
  die "grant: $(cat "$tmp/body")"
pass "the admin granted dev Developer with billing"

# --- 5. dev in the SQL editor ---------------------------------------------------------------------------

DEV_TOKEN="$(login_token "$DEV_EMAIL" "$PASSWORD")"
# The member's role is made on first use and the grant reconciled: the first run may take a few tries.
for _ in $(seq 30); do
  s="$(run "$DEV_TOKEN" billing "INSERT INTO billing.t VALUES (1, 'from dev'); SELECT count(*) FROM billing.t")"
  [[ "$s" == 200 ]] && break
  sleep 2
done
[[ "$s" == 200 ]] || die "dev's first run as billing: $s $(cat "$tmp/events")"
jq -s -e '(.[-1].outcome == "ok") and any(.[]; .t == "complete" and .tag == "INSERT 0 1")
  and ([.[] | select(.t == "rows") | .rows[0][0]] == ["1"])' "$tmp/events" >/dev/null ||
  die "dev's INSERT as billing: $(jq -sc . "$tmp/events")"
denied "$DEV_TOKEN" billing "CREATE TABLE public.nope(id int)"
denied "$DEV_TOKEN" billing "CREATE TABLE other.nope(id int)"
denied "$DEV_TOKEN" billing "SELECT * FROM other.t"
s="$(run "$DEV_TOKEN" "" "SELECT 1")"
[[ "$s" == 403 && "$(jq -r '.error.code' "$tmp/events")" == forbidden ]] ||
  die "dev as the owner: $s $(cat "$tmp/events")"
pass "dev as billing: INSERT into billing.t ok; public and another schema permission denied; the owner is 403 forbidden"

# R31: whatever a script makes as the member role itself is handed over; nothing stays with cb_m_*.
# After RESET ROLE the member role still can't create in a schema it has no right on (billing is
# the group role's), but wherever PUBLIC may create (open) it can, and that object is handed over.
denied "$DEV_TOKEN" billing "RESET ROLE; CREATE TABLE billing.x(id int)"
run_ok "$DEV_TOKEN" billing "RESET ROLE; CREATE TABLE open.leak(id int)" '.[-1].outcome == "ok"'
# shellcheck disable=SC2016
OWNED='SELECT count(*) FROM pg_shdepend d JOIN pg_roles r ON r.oid = d.refobjid
  WHERE d.deptype = '"'o'"' AND r.rolname LIKE '"'cb\\_m\\_%'"'
  AND d.dbid = (SELECT oid FROM pg_database WHERE datname = current_database())'
atok="$(admin_token)"
n=""
for _ in $(seq 20); do # the hand-over runs right after the run's done event
  run_ok "$atok" "" "$OWNED" '.[-1].outcome == "ok"'
  n="$(jq -rs '[.[] | select(.t == "rows") | .rows[0][0]][0]' "$tmp/events")"
  [[ "$n" == 0 ]] && break
  sleep 1
done
[[ "$n" == 0 ]] || die "$n objects are still owned by a cb_m_* role after RESET ROLE; CREATE"
run_ok "$atok" "" "SELECT to_regclass('open.leak')::text" '[.[] | select(.t == "rows") | .rows[0][0]] == ["open.leak"]'
pass "RESET ROLE: CREATE in billing is denied; CREATE in open works and leaves nothing owned by a cb_m_* role (open.leak exists)"

# --- 6. Revoke -----------------------------------------------------------------------------------------------

DEV_TOKEN="$(login_token "$DEV_EMAIL" "$PASSWORD")"
stream "$DEV_TOKEN" "$tmp/long" 110 "$(run_body billing "SELECT pg_sleep(100)" 120000)" >"$tmp/long.status" &
bg_pid=$!
sleep 5
t_revoke=$SECONDS
s="$(api DELETE "/v1/projects/$project_id/grants/$dev_member")"
[[ "$s" == 200 || "$s" == 204 ]] || die "revoke: $s $(cat "$tmp/body")"
refused=""
for _ in $(seq 30); do
  s="$(uapi "$DEV_TOKEN" GET "/v1/projects/$project_id")"
  if [[ "$s" == 403 || "$s" == 404 ]]; then
    refused=1
    break
  fi
  sleep 1
done
[[ -n "$refused" ]] || die "dev still reads the project 30 s after the revoke: $s $(cat "$tmp/body")"
s="$(run "$DEV_TOKEN" billing "SELECT 1")"
[[ "$s" == 403 || "$s" == 404 ]] || die "a SQL run after the revoke: $s $(cat "$tmp/events")"
wait "$bg_pid" || true
bg_pid=""
ended=$((SECONDS - t_revoke))
((ended < 80)) || die "the open SQL session was still running $ended s after the revoke"
if grep -Eq '"outcome" *: *"ok"' "$tmp/long"; then
  die "the open SQL session finished ok after the revoke: $(tail -c 400 "$tmp/long")"
fi
pass "revoked: the next request is refused, and the open SQL session ended ($ended s after the revoke)"

# Removing the member: the token they already hold is refused at once, and a run in flight ends.
ok PUT "/v1/projects/$project_id/grants/$dev_member" 200 '{"platform_role":"developer","pg_roles":["billing"]}'
DEV_TOKEN="$(login_token "$DEV_EMAIL" "$PASSWORD")"
for _ in $(seq 30); do # the grant is reconciled again on first use
  s="$(run "$DEV_TOKEN" billing "SELECT 1")"
  [[ "$s" == 200 ]] && break
  sleep 2
done
[[ "$s" == 200 ]] || die "dev's run after the grant came back: $s $(cat "$tmp/events")"
stream "$DEV_TOKEN" "$tmp/long" 110 "$(run_body billing "SELECT pg_sleep(100)" 120000)" >"$tmp/long.status" &
bg_pid=$!
sleep 5
t_remove=$SECONDS
s="$(api DELETE "/v1/members/$dev_member")"
[[ "$s" == 200 || "$s" == 204 ]] || die "remove member: $s $(cat "$tmp/body")"
# /v1/me is for any signed-in identity: it tells them they're removed, with no projects. Every
# member route answers 404 not_found.
s="$(uapi "$DEV_TOKEN" GET /v1/me)"
[[ "$s" == 200 ]] && jq -e '.member.status == "removed" and .projects == []' "$tmp/body" >/dev/null ||
  die "the removed member's /v1/me: $s $(cat "$tmp/body")"
s="$(uapi "$DEV_TOKEN" GET /v1/members)"
[[ "$s" == 404 && "$(jq -r '.error.code' "$tmp/body")" == not_found ]] ||
  die "the removed member's old token: $s (wanted 404 not_found) $(cat "$tmp/body")"
wait "$bg_pid" || true
bg_pid=""
ended=$((SECONDS - t_remove))
((ended < 80)) || die "the open SQL session was still running $ended s after the member was removed"
if grep -Eq '"outcome" *: *"ok"' "$tmp/long"; then
  die "the open SQL session finished ok after the member was removed: $(tail -c 400 "$tmp/long")"
fi
pass "member removed: /v1/me says removed, the old token answers 404 not_found elsewhere, and the open SQL session ended ($ended s after)"

# I1: invited again, the removed member stays out until they accept: their old password signs
# nobody in (their Zitadel user stays deactivated), before and after the invite's expires_at,
# and they stay `invited`. Zitadel makes no invite code for a user with a password, so the mail
# is a password-reset one (`reset=1`); accepting sets the password with that code on the
# deactivated user (real Zitadel allows it), then reactivates them, and they are active again.
ok POST /v1/invites 201 "$(jq -nc --arg e "$DEV_EMAIL" '{email: $e, grants: [], delivery: "email"}')"
jq -e --arg m "$dev_member" '.outcome == "invited" and .member.id == $m and .member.status == "invited"' "$tmp/body" >/dev/null ||
  die "invite of the removed member: $(cat "$tmp/body")"
reinvite="$(jq -r '.member.invite.id // empty' "$tmp/body")"
[[ -n "$reinvite" ]] || die "the invite has no id: $(cat "$tmp/body")"
# old_password_refused <when>: sign-in with dev's old password doesn't finish, and dev is invited.
old_password_refused() {
  local out
  out="$(ui sign-in "{\"authRequest\":\"$(authorize)\",\"email\":\"$DEV_EMAIL\",\"password\":\"$PASSWORD\"}")"
  if [[ "$(jq -r '.error.code // empty' <<<"$out" 2>/dev/null)" == rate_limited ]]; then
    sleep 61 # a rate limit proves nothing: try again once the window is over
    out="$(ui sign-in "{\"authRequest\":\"$(authorize)\",\"email\":\"$DEV_EMAIL\",\"password\":\"$PASSWORD\"}")"
  fi
  [[ "$(jq -r '.next // empty' <<<"$out" 2>/dev/null)" != done && "$(jq -r '.error.code // empty' <<<"$out" 2>/dev/null)" != rate_limited ]] ||
    die "$1: the removed member signed in with the old password, or the check was rate limited: $(status) $out"
  ok GET /v1/members 200
  jq -e --arg m "$dev_member" 'any(.[]; .id == $m and .status == "invited")' "$tmp/body" >/dev/null ||
    die "$1: dev isn't invited: $(jq -c --arg m "$dev_member" '[.[] | select(.id == $m)]' "$tmp/body")"
}
old_password_refused "before expires_at"
metadata_sql -c "UPDATE invites SET expires_at = now() - interval '1 minute' WHERE id = '$reinvite'" >/dev/null ||
  die "couldn't expire the invite"
old_password_refused "after expires_at"
metadata_sql -c "UPDATE invites SET expires_at = now() + interval '1 day' WHERE id = '$reinvite'" >/dev/null ||
  die "couldn't restore the invite's expiry"
relink="$(mail_link "$DEV_EMAIL" accept-invite "$link")"
[[ "$relink" == *"reset=1"* ]] || die "the second invite isn't a password-reset link: $relink"
NEW_PASSWORD="E2e-again-$(openssl rand -hex 4)A!"
DEV_TOKEN="$(accept_invite "$relink" "$NEW_PASSWORD")"
s="$(uapi "$DEV_TOKEN" GET /v1/me)"
[[ "$s" == 200 ]] && jq -e --arg m "$dev_member" '.member.id == $m and .member.status == "active"' "$tmp/body" >/dev/null ||
  die "dev after accepting again: $s $(cat "$tmp/body")"
out="$(ui sign-in "{\"authRequest\":\"$(authorize)\",\"email\":\"$DEV_EMAIL\",\"password\":\"$PASSWORD\"}")"
[[ "$(jq -r '.next // empty' <<<"$out" 2>/dev/null)" != done ]] || die "the old password still signs in after accepting"
pass "invited again: the old password signs nobody in before or after expires_at (still invited); the code verifies on the deactivated user and dev is active with the new password"

# --- 7. Sign-up -------------------------------------------------------------------------------------------------

ok GET /v1/instance/settings 200
jq -e '.open_signup == false and .open_signup_switchable == true' "$tmp/body" >/dev/null ||
  die "open sign-up should be off and switchable: $(cat "$tmp/body")"
ar="$(authorize create)"
[[ "$(ui auth-request "{\"authRequest\":\"$ar\"}")" == '{"signUp":false,"signUpAllowed":false}' ]] ||
  die "closed sign-up was offered: $(status)"
ui sign-up "{\"authRequest\":\"$ar\",\"email\":\"$SIGNUP_EMAIL\",\"password\":\"$PASSWORD\",\"givenName\":\"Sign\",\"familyName\":\"Up\"}" >"$tmp/signup-body"
[[ "$(status)" == 403 && "$(jq -r '.error.code' "$tmp/signup-body")" == sign_up_closed ]] ||
  die "closed sign-up should be 403 sign_up_closed: $(status) $(cat "$tmp/signup-body")"
pass "open sign-up off: signUpAllowed false and sign-up answers sign_up_closed"

signup_opened=1
ok PATCH /v1/instance/settings 200 '{"open_signup":true}'
jq -e '.open_signup == true' "$tmp/body" >/dev/null || die "open sign-up didn't turn on: $(cat "$tmp/body")"
allowed=""
for _ in $(seq 20); do
  ar="$(authorize create)"
  if [[ "$(ui auth-request "{\"authRequest\":\"$ar\"}")" == '{"signUp":true,"signUpAllowed":true}' ]]; then
    allowed=1
    break
  fi
  sleep 1
done
[[ -n "$allowed" ]] || die "sign-up still isn't offered 20 s after turning it on: $(status)"
signed="$(ui sign-up "{\"authRequest\":\"$ar\",\"email\":\"$SIGNUP_EMAIL\",\"password\":\"$PASSWORD\",\"givenName\":\"Sign\",\"familyName\":\"Up\"}")"
[[ "$(status)" == 200 && "$(jq -r .next <<<"$signed")" == done ]] ||
  die "sign-up: $(status) $signed (is email verification off?)"
SIGNUP_TOKEN="$(exchange "$(jq -r .redirect <<<"$signed")")"
[[ -n "$SIGNUP_TOKEN" && "$SIGNUP_TOKEN" != null ]] || die "no access token after sign-up"
s="$(uapi "$SIGNUP_TOKEN" GET /v1/me)"
[[ "$s" == 200 ]] || die "signed-up /v1/me: $s $(cat "$tmp/body")"
jq -e '.member.status == "pending"' "$tmp/body" >/dev/null || die "the signed-up person isn't pending: $(cat "$tmp/body")"
signup_member="$(jq -r '.member.id' "$tmp/body")"
member_ids+=("$signup_member")
s="$(uapi "$SIGNUP_TOKEN" GET "/v1/projects/$project_id")"
[[ "$s" == 403 && "$(jq -r '.error.code' "$tmp/body")" == pending_approval ]] ||
  die "a pending member's project read: $s $(cat "$tmp/body")"
ok GET /v1/members 200
jq -e --arg m "$signup_member" --arg e "$SIGNUP_EMAIL" \
  'any(.[]; .id == $m and .email == $e and .status == "pending")' "$tmp/body" >/dev/null ||
  die "listMembers doesn't show the sign-up as pending: $(cat "$tmp/body")"
pass "open sign-up on: a third person signed up, is pending (403 pending_approval) and listed as pending"

ok POST "/v1/members/$signup_member/approve" 200 \
  "$(jq -nc --arg p "$project_id" '{grants: [{project_id: $p, platform_role: "viewer"}]}')"
jq -e '.status == "active"' "$tmp/body" >/dev/null || die "approve: $(cat "$tmp/body")"
read_ok=""
for _ in $(seq 20); do
  s="$(uapi "$SIGNUP_TOKEN" GET "/v1/projects/$project_id")"
  if [[ "$s" == 200 ]]; then
    read_ok=1
    break
  fi
  sleep 1
done
[[ -n "$read_ok" ]] || die "the approved member can't read the project: $s $(cat "$tmp/body")"
pass "approved with a viewer grant: the member reads the project"

# A second sign-up, declined: the next request with their token is refused.
ar="$(authorize create)"
signed="$(ui sign-up "{\"authRequest\":\"$ar\",\"email\":\"$DECLINE_EMAIL\",\"password\":\"$PASSWORD\",\"givenName\":\"De\",\"familyName\":\"Cline\"}")"
[[ "$(status)" == 200 && "$(jq -r .next <<<"$signed")" == done ]] || die "second sign-up: $(status) $signed"
DECLINE_TOKEN="$(exchange "$(jq -r .redirect <<<"$signed")")"
[[ -n "$DECLINE_TOKEN" && "$DECLINE_TOKEN" != null ]] || die "no access token after the second sign-up"
s="$(uapi "$DECLINE_TOKEN" GET /v1/me)"
[[ "$s" == 200 ]] && jq -e '.member.status == "pending"' "$tmp/body" >/dev/null || die "second sign-up isn't pending: $s $(cat "$tmp/body")"
decline_member="$(jq -r '.member.id' "$tmp/body")"
member_ids+=("$decline_member")
s="$(api POST "/v1/members/$decline_member/decline")"
[[ "$s" == 204 ]] || die "decline: $s $(cat "$tmp/body")"
refused=""
for _ in $(seq 20); do # the declined user's token is refused at once (`/v1/me` says removed); Zitadel's step follows
  s="$(uapi "$DECLINE_TOKEN" GET /v1/members)"
  # 403 is what a pending member already gets: a declined one is 404 not_found (or 401 once
  # Zitadel ended the session).
  if [[ "$s" == 401 || "$s" == 404 ]]; then
    refused=1
    break
  fi
  sleep 1
done
[[ -n "$refused" ]] || die "a declined sign-up's next request was answered $s: $(cat "$tmp/body")"
pass "declined a second sign-up: the next request is refused ($s)"

# The access log: every change above is in it, with the people named (the actor is the demo
# machine user, a member with no email: checked by its member id).
ok GET "/v1/access-events?limit=200" 200
jq -e '(.events | type) == "array" and has("next_before")' "$tmp/body" >/dev/null || die "access-events shape: $(head -c 400 "$tmp/body")"
jq -e --arg dev "$DEV_EMAIL" --arg su "$SIGNUP_EMAIL" '
  .events as $e
  | def has_ev($a; f): any($e[]; .action == $a and f);
  has_ev("invite_created"; .actor_member_id != null and .target_email == $dev)
  and has_ev("invite_accepted"; .target_email == $dev or .actor_email == $dev)
  and has_ev("grant_put"; .actor_member_id != null and .target_email == $dev)
  and has_ev("grant_deleted"; .actor_member_id != null and .target_email == $dev)
  and has_ev("open_signup_set"; .actor_member_id != null)
  and has_ev("member_approved"; .actor_member_id != null and .target_email == $su)
  and has_ev("member_declined"; .target_email != null)
  and has_ev("invite_resent"; true) and has_ev("invite_revoked"; true)' "$tmp/body" >/dev/null ||
  die "access-events miss an expected event: $(jq -c '[.events[] | {action, actor_email, target_email}]' "$tmp/body")"
ok GET "/v1/projects/$project_id/access-events" 200
jq -e 'any(.events[]; .action == "grant_put")' "$tmp/body" >/dev/null || die "the project's access log has no grant_put: $(head -c 400 "$tmp/body")"
pass "access-events: {events, next_before} with the invite, grant, sign-up and approval events, people named"

ok PATCH /v1/instance/settings 200 '{"open_signup":false}'
signup_opened=""
jq -e '.open_signup == false' "$tmp/body" >/dev/null || die "open sign-up didn't turn off: $(cat "$tmp/body")"
ar="$(authorize create)"
[[ "$(ui auth-request "{\"authRequest\":\"$ar\"}")" == '{"signUp":false,"signUpAllowed":false}' ]] ||
  die "sign-up is still offered after turning it off: $(status)"
pass "open sign-up off again"

echo "all passed ($((SECONDS - t0)) s)"
