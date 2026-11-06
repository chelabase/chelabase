#!/usr/bin/env bash
# Live check of the identity service + auth + the gateway (needs `just workbench up`): closed
# sign-up, a link invite and accepting it, the session cookie and /auth/v1/token, /v1/me, sign-in,
# identical failures, password reset with the emailed code, logout ending the session. The invited
# person is removed on exit.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
LOG_PREFIX=auth-e2e
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
GATEWAY="${GATEWAY_URL:-$CHELABASE_ENTRY_API_URL}"
MAILPIT="${MAILPIT_URL:-$CHELABASE_ENTRY_MAIL_URL}"
STUDIO="$CHELABASE_ENTRY_STUDIO_URL"
tmp="$(mktemp -d)"
member_id=""
cleanup() {
  local rc=$?
  set +e
  [[ -z "$member_id" ]] || api DELETE "/v1/members/$member_id" >/dev/null
  rm -rf "$tmp"
  exit "$rc"
}
trap cleanup EXIT
[[ -x "${CHELABASE_CLI:-}" ]] || die "CHELABASE_CLI is unset or not executable (run through: just workbench run scripts/<name>.sh)"
# shellcheck source=lib/studio-auth.sh
. scripts/lib/studio-auth.sh
EMAIL="e2e-$(date +%s)-$RANDOM@example.com"
PASSWORD="E2e-pass-$(openssl rand -hex 4)A!"

# Sign-up is off by default (R34): the instance says so, and a sign-up request is refused.
instance="$(curl -sS --max-time 30 "$GATEWAY/auth/v1/instance")"
[[ "$instance" == '{"setupRequired":false,"signUpAllowed":false}' ]] || die "closed sign-up was offered: $instance"
out="$(ui sign-up "$(jq -nc --arg e "$EMAIL" --arg p "$PASSWORD" '{email: $e, password: $p, givenName: "E", familyName: "Two"}')")"
[[ "$(status)" == 403 && "$(jq -r '.error.code // empty' <<<"$out")" == sign_up_closed ]] ||
  die "closed sign-up should be 403 sign_up_closed: $(status) $out"
say "sign-up is closed"

# People join by invite. The stack's dev member (`chelabase dev token`, the tenant admin the
# bootstrap signs up) checks that email can be sent (Mailpit is the SMTP server) and invites by link.
status="$(api GET /v1/me)"
[[ "$status" == 200 ]] && jq -e '.member.tenant_admin == true' "$tmp/body" >/dev/null ||
  die "the dev member isn't a tenant admin: $status $(cat "$tmp/body") (re-run just stack-bootstrap)"
status="$(api GET /v1/invites/delivery)"
[[ "$(jq -c . "$tmp/body")" == '{"email_available":true,"reason":null}' ]] ||
  die "invites can't be emailed: is Mailpit the identity service's SMTP server? $status $(cat "$tmp/body")"
status="$(api POST /v1/invites "$(jq -nc --arg e "$EMAIL" '{email: $e, grants: [], delivery: "link"}')")"
[[ "$status" == 201 ]] || die "POST /v1/invites: $status $(cat "$tmp/body")"
member_id="$(jq -r '.member.id' "$tmp/body")"
url="$(jq -r '.invite_url // empty' "$tmp/body")"
[[ "$url" == "$STUDIO/accept-invite?user="*"code="*"t="* ]] || die "no code link: $(cat "$tmp/body")"
token="$(accept_link "$url" "$PASSWORD")"
me="$(curl -sS -H "Authorization: Bearer $token" "$GATEWAY/v1/me")"
jq -e --arg m "$member_id" '.tenant_id != null and .member.id == $m and .member.status == "active"' <<<"$me" >/dev/null ||
  die "/v1/me: $me"
say "invited by link, accepted (the session cookie), minted a token at /auth/v1/token and resolved the tenant"

# /auth/v1/token is Studio's: without X-Chelabase-Studio it is refused, also with a live session.
jar="$(new_jar)"
out="$(sign_in "$EMAIL" "$PASSWORD" "$jar")"
[[ "$(status)" == 200 && "$(jq -r .next <<<"$out")" == studio ]] || die "sign-in: $(status) $out"
not_studio="$(curl -sS -o /dev/null -w '%{http_code}' -b "$jar" -H "Origin: $STUDIO" -X POST "$GATEWAY/auth/v1/token")"
[[ "$not_studio" == 403 ]] || die "/auth/v1/token without X-Chelabase-Studio: $not_studio (wanted 403)"
session_token "$jar" >/dev/null
say "signed in with the password; /auth/v1/token needs Studio's header"

wrong="$(ui sign-in "$(jq -nc --arg e "$EMAIL" '{email: $e, password: "nope"}')" "$(new_jar)")"
w_status="$(status)"
unknown="$(ui sign-in "$(jq -nc --arg e "nobody-$RANDOM@example.com" '{email: $e, password: "nope"}')" "$(new_jar)")"
u_status="$(status)"
[[ "$w_status" == 401 && "$u_status" == 401 && "$wrong" == "$unknown" ]] || die "failures differ: $w_status $wrong vs $u_status $unknown"
say "a wrong password and an unknown email look identical"

# timed_sign_in <email> → seconds for a failing sign-in
timed_sign_in() {
  curl -sS -o /dev/null -w '%{time_total}' -H "Origin: $STUDIO" -H 'X-Chelabase-Studio: 1' -H 'Content-Type: application/json' \
    -X POST "$GATEWAY/auth/v1/sign-in" -d "$(jq -nc --arg e "$1" '{email: $e, password: "Nope-nope-1!"}')"
}
known_t="$(timed_sign_in "$EMAIL") $(timed_sign_in "$EMAIL")"
unknown_t="$(timed_sign_in "nobody-$RANDOM@example.com") $(timed_sign_in "nobody-$RANDOM@example.com")"
awk -v k="$known_t" -v u="$unknown_t" 'BEGIN { split(k, a, " "); split(u, b, " ");
  d = (a[1] + a[2]) / 2 - (b[1] + b[2]) / 2; exit !(d < 0.15 && d > -0.15) }' ||
  die "sign-in timing tells accounts apart: existing $known_t s, unknown $unknown_t s"
say "and take the same time (existing $known_t s, unknown $unknown_t s)"

# Password reset: the code comes by mail; confirming it ends every session of the account.
ui password-reset "$(jq -nc --arg e "$EMAIL" '{email: $e}')" >/dev/null
[[ "$(status)" == 200 ]] || die "password-reset: $(status)"
mail="$(mail_id "$EMAIL")"
mail_link "$mail" password-reset >/dev/null
rcode="$(mail_code "$mail")"
NEWPW="E2e-new-$(openssl rand -hex 4)B!"
out="$(ui password-reset/confirm "$(jq -nc --arg e "$EMAIL" --arg c "$rcode" --arg p "$NEWPW" '{email: $e, code: $c, password: $p}')")"
[[ "$(status)" == 200 ]] || die "confirm: $(status) $out"
gone="$(curl -sS -o /dev/null -w '%{http_code}' -b "$jar" -H "Origin: $STUDIO" -H 'X-Chelabase-Studio: 1' -X POST "$GATEWAY/auth/v1/token")"
[[ "$gone" == 401 ]] || die "a session from before the reset still mints a token: $gone (wanted 401)"
out="$(sign_in "$EMAIL" "$PASSWORD" "$(new_jar)")"
[[ "$(status)" == 401 ]] || die "the old password after the reset: $(status) $out"
jar="$(new_jar)"
out="$(sign_in "$EMAIL" "$NEWPW" "$jar")"
[[ "$(jq -r '.next // empty' <<<"$out")" == studio ]] || die "sign-in with the new password: $(status) $out"
say "reset the password with the emailed code: earlier sessions end, the old password is refused, the new one signs in"

cp "$jar" "$tmp/before-logout"
out_headers="$(curl -sS -D - -o /dev/null -b "$jar" -c "$jar" -H "Origin: $STUDIO" -H 'X-Chelabase-Studio: 1' \
  -H 'Content-Type: application/json' -X POST "$GATEWAY/auth/v1/logout" -d '{}')"
grep -qi '^set-cookie: chelabase_session=;.*Max-Age=0' <<<"$out_headers" || die "logout didn't clear the session cookie"
kept="$(curl -sS -o /dev/null -w '%{http_code}' -b "$tmp/before-logout" -H "Origin: $STUDIO" -H 'X-Chelabase-Studio: 1' -X POST "$GATEWAY/auth/v1/token")"
[[ "$kept" == 401 ]] || die "the session cookie kept from before the logout still mints a token: $kept (wanted 401)"
say "logged out: the cookie is cleared and the session is revoked"
say "ok"
