#!/usr/bin/env bash
# Live check of Zitadel v4 + auth + the gateway (needs `just dev`): sign-up with a code
# from Mailpit, token exchange, /v1/me, sign-in, identical failures, password reset, logout.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
set -a; source "$ROOT/.cheladb/zitadel/demo.env"; set +a
# A worktree slot's Studio app (W7; `just` exports it), else slot 0's from Studio's .env.local.
CLIENT="${VITE_ZITADEL_CLIENT_ID:-$(sed -n 's/^VITE_ZITADEL_CLIENT_ID=//p' "$ROOT/apps/studio/.env.local")}"
ISS="$CHELADB_ZITADEL_ISSUER"
GW="${GATEWAY_URL:-http://localhost:8000}"
MAILPIT="${MAILPIT_URL:-http://localhost:8025}"
STUDIO="${CHELADB_STUDIO_URL:-http://localhost:3000}"
JAR="$(mktemp)"; STATUS_FILE="$(mktemp)"; trap 'rm -f "$JAR" "$STATUS_FILE"' EXIT
EMAIL="e2e-$(date +%s)-$RANDOM@example.com"
PASSWORD="E2e-pass-$(openssl rand -hex 4)A!"

say() { echo "[auth-e2e] $*"; }
die() { echo "[auth-e2e] FAIL: $*" >&2; exit 1; }

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
VERIFIER="$(openssl rand -hex 32)"
CHALLENGE="$(printf %s "$VERIFIER" | openssl dgst -sha256 -binary | b64url)"

# authorize [prompt] → the authRequest id from Zitadel's redirect to Studio's /login
authorize() {
  local scope="openid profile email urn:zitadel:iam:user:resourceowner urn:zitadel:iam:org:project:id:${CHELADB_ZITADEL_PROJECT_ID}:aud"
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
  printf '%s' "${out##*$'\n'}" >"$STATUS_FILE"; printf '%s' "${out%$'\n'*}"
}
status() { cat "$STATUS_FILE"; }

# mail_link <to> <kind: verify|password-reset> → the link in the newest such email
mail_link() {
  local id
  for _ in $(seq 30); do
    id="$(curl -sS -G "$MAILPIT/api/v1/search" --data-urlencode "query=to:$1" | jq -r '.messages[0].ID // empty')"
    if [[ -n "$id" ]]; then
      curl -sS "$MAILPIT/api/v1/message/$id" | jq -r '.Text' | grep -oE "$STUDIO/$2\?[^ \"<>]+" | head -n1 && return 0
    fi
    sleep 1
  done
  die "no $2 email for $1"
}

# exchange <callback url> → access token
exchange() {
  local code; code="$(sed -E 's/.*[?&]code=([^&]+).*/\1/' <<<"$1")"
  curl -sS -X POST "$ISS/oauth/v2/token" -d grant_type=authorization_code -d "code=$code" \
    -d "redirect_uri=$STUDIO/callback" -d "client_id=$CLIENT" -d "code_verifier=$VERIFIER" | jq -r .access_token
}

ar="$(authorize create)"
[[ "$(ui auth-request "{\"authRequest\":\"$ar\"}")" == '{"signUp":true}' ]] || die "prompt=create not reported"
signed_up="$(ui sign-up "{\"authRequest\":\"$ar\",\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\",\"givenName\":\"E\",\"familyName\":\"Two\"}")"
[[ "$(status)" == 200 ]] || die "sign-up: $(status)"
if [[ "$(jq -r .next <<<"$signed_up")" == verify ]]; then
  # CHELADB_REQUIRE_EMAIL_VERIFICATION=true: finish with the code from Mailpit.
  code="$(mail_link "$EMAIL" verify | sed -E 's/.*code=([^&]+).*/\1/')"
  signed_up="$(ui verify-email "{\"code\":\"$code\"}")"
  say "verified the email with the code from Mailpit"
fi
redirect="$(jq -r .redirect <<<"$signed_up")"
[[ "$redirect" == "$STUDIO/callback?"* ]] || die "sign-up didn't finish: $signed_up"
token="$(exchange "$redirect")"
[[ -n "$token" && "$token" != null ]] || die "no access token"
me="$(curl -sS -H "Authorization: Bearer $token" "$GW/v1/me")"
jq -e '.tenant != null' <<<"$me" >/dev/null || die "/v1/me: $me"
say "signed up, exchanged the code and resolved the tenant"

ar="$(authorize)"
signed="$(ui sign-in "{\"authRequest\":\"$ar\",\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}")"
[[ "$(jq -r .next <<<"$signed")" == done ]] || die "sign-in: $signed"
say "signed in with the password"

wrong="$(ui sign-in "{\"authRequest\":\"$(authorize)\",\"email\":\"$EMAIL\",\"password\":\"nope\"}")"; w_status="$(status)"
unknown="$(ui sign-in "{\"authRequest\":\"$(authorize)\",\"email\":\"nobody-$RANDOM@example.com\",\"password\":\"nope\"}")"
u_status="$(status)"
[[ "$w_status" == 401 && "$u_status" == 401 && "$wrong" == "$unknown" ]] || die "failures differ: $w_status $wrong vs $u_status $unknown"
say "a wrong password and an unknown email look identical"

# timed_sign_in <email> → seconds for a failing sign-in (only the POST is timed)
timed_sign_in() {
  local ar; ar="$(authorize)"
  curl -sS -o /dev/null -w '%{time_total}' -H "Origin: $STUDIO" -H 'Content-Type: application/json' \
    -X POST "$GW/auth/v1/sign-in" -d "{\"authRequest\":\"$ar\",\"email\":\"$1\",\"password\":\"Nope-nope-1!\"}"
}
known_t="$(timed_sign_in "$EMAIL") $(timed_sign_in "$EMAIL")"
unknown_t="$(timed_sign_in "nobody-$RANDOM@example.com") $(timed_sign_in "nobody-$RANDOM@example.com")"
awk -v k="$known_t" -v u="$unknown_t" 'BEGIN { split(k, a, " "); split(u, b, " ");
  d = (a[1] + a[2]) / 2 - (b[1] + b[2]) / 2; exit !(d < 0.15 && d > -0.15) }' ||
  die "sign-in timing tells accounts apart: existing $known_t s, unknown $unknown_t s"
say "and take the same time (existing $known_t s, unknown $unknown_t s)"

ui password-reset "{\"email\":\"$EMAIL\"}" >/dev/null
link="$(mail_link "$EMAIL" password-reset)"
uid="$(sed -E 's/.*userId=([^&]+).*/\1/' <<<"$link")"; rcode="$(sed -E 's/.*code=([^&]+).*/\1/' <<<"$link")"
NEWPW="E2e-new-$(openssl rand -hex 4)B!"
ui password-reset/confirm "{\"userId\":\"$uid\",\"code\":\"$rcode\",\"password\":\"$NEWPW\"}" >/dev/null
[[ "$(status)" == 200 ]] || die "confirm: $(status)"
again="$(ui sign-in "{\"authRequest\":\"$(authorize)\",\"email\":\"$EMAIL\",\"password\":\"$NEWPW\"}")"
[[ "$(jq -r .next <<<"$again")" == done ]] || die "sign-in with the new password: $again"
say "reset the password and signed in with the new one"

out_headers="$(curl -sS -D - -o /dev/null -b "$JAR" -c "$JAR" -H "Origin: $STUDIO" -H 'Content-Type: application/json' -X POST "$GW/auth/v1/logout" -d '{}')"
grep -qi '^set-cookie: cheladb_session=;.*Max-Age=0' <<<"$out_headers" || die "logout didn't clear the session cookie"
say "logged out"
say "ok"
