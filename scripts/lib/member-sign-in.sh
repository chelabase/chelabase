# Sign-in helpers for e2e scripts that act as a project member rather than the demo tenant
# admin (as scripts/members-e2e.sh signs in). Sourced, not run, after neon-common.sh, with $tmp
# set, $GATEWAY set and demo.env sourced (CHELABASE_ZITADEL_PROJECT_ID). Uses $tmp/jar,
# $tmp/ui-status and $tmp/body.
#
#   new_member <project id> <grant JSON>  invites a person by link, accepts and grants them on
#                                         the project; sets MEMBER_ID (delete it on exit:
#                                         api DELETE /v1/members/$MEMBER_ID)
#   member_token                          a fresh access token for that member (short-lived)

STUDIO="$CHELABASE_ENTRY_STUDIO_URL"
ISS="$CHELABASE_ENTRY_AUTH_URL" # the issuer, derived from the entry
# Studio's Zitadel app: VITE_ZITADEL_CLIENT_ID when set, else the one in the stack's studio.env.
if [[ -z "${VITE_ZITADEL_CLIENT_ID:-}" && ! -f "$CHELABASE_STACK_DIR/studio.env" ]]; then
  die "${CHELABASE_STACK_DIR:-<stack dir>}/studio.env is missing: run just workbench up"
fi
CLIENT="${VITE_ZITADEL_CLIENT_ID:-$(sed -n 's/^VITE_ZITADEL_CLIENT_ID=//p' "$CHELABASE_STACK_DIR/studio.env")}"
MEMBER_ID=""
MEMBER_EMAIL="$LOG_PREFIX-member-$(date +%s)-$RANDOM@example.com"
MEMBER_PASSWORD="E2e-pass-$(openssl rand -hex 4)A!"

_b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
_VERIFIER="$(openssl rand -hex 32)"
_CHALLENGE="$(printf %s "$_VERIFIER" | openssl dgst -sha256 -binary | _b64url)"

# _authorize → the authRequest id from Zitadel's redirect to Studio's /login
_authorize() {
  local scope="openid profile email urn:zitadel:iam:user:resourceowner urn:zitadel:iam:org:project:id:${CHELABASE_ZITADEL_PROJECT_ID}:aud"
  local loc
  loc="$(curl -sS -o /dev/null -w '%{redirect_url}' -G "$ISS/oauth/v2/authorize" \
    --data-urlencode "client_id=$CLIENT" --data-urlencode "redirect_uri=$STUDIO/callback" \
    --data-urlencode "response_type=code" --data-urlencode "scope=$scope" \
    --data-urlencode "code_challenge=$_CHALLENGE" --data-urlencode "code_challenge_method=S256")"
  [[ "$loc" == "$STUDIO/login?authRequest="* ]] || die "authorize redirected to: $loc"
  echo "${loc#*authRequest=}"
}

# _ui <path> <json> → body; its status in $tmp/ui-status
_ui() {
  local out
  out="$(curl -sS -b "$tmp/jar" -c "$tmp/jar" -H "Origin: $STUDIO" -H 'Content-Type: application/json' \
    -w '\n%{http_code}' -X POST "$GATEWAY/auth/v1/$1" -d "$2")"
  printf '%s' "${out##*$'\n'}" >"$tmp/ui-status"
  printf '%s' "${out%$'\n'*}"
}

# _exchange <callback url> → access token
_exchange() {
  local code
  code="$(sed -E 's/.*[?&]code=([^&]+).*/\1/' <<<"$1")"
  curl -sS -X POST "$ISS/oauth/v2/token" -d grant_type=authorization_code -d "code=$code" \
    -d "redirect_uri=$STUDIO/callback" -d "client_id=$CLIENT" -d "code_verifier=$_VERIFIER" | jq -r .access_token
}

# new_member <project id> <grant JSON>: see the header.
new_member() {
  local status url uid code out redirect
  status="$(api POST /v1/invites "$(jq -nc --arg e "$MEMBER_EMAIL" '{email: $e, grants: [], delivery: "link"}')")"
  [[ "$status" == 201 ]] || die "POST /v1/invites: $status $(cat "$tmp/body")"
  MEMBER_ID="$(jq -r '.member.id' "$tmp/body")"
  url="$(jq -r '.invite_url // empty' "$tmp/body")"
  [[ "$url" == "$STUDIO/accept-invite?user="* ]] || die "no invite link: $(cat "$tmp/body")"
  uid="$(sed -E 's/.*[?&]user=([^&]+).*/\1/' <<<"$url")"
  code="$(sed -E 's/.*[&?]code=([^&]+).*/\1/' <<<"$url")"
  out="$(_ui accept-invite "$(jq -nc --arg ar "$(_authorize)" --arg u "$uid" --arg c "$code" --arg p "$MEMBER_PASSWORD" \
    '{authRequest: $ar, user: $u, code: $c, reset: false, password: $p}')")"
  [[ "$(cat "$tmp/ui-status")" == 200 ]] || die "accept-invite: $(cat "$tmp/ui-status") $out"
  redirect="$(jq -r .redirect <<<"$out")"
  [[ "$redirect" == "$STUDIO/callback?"* ]] || die "accept-invite didn't finish: $out"
  status="$(api PUT "/v1/projects/$1/grants/$MEMBER_ID" "$2")"
  [[ "$status" == 200 ]] || die "PUT grant for $MEMBER_ID: $status $(cat "$tmp/body")"
}

# member_token → a fresh access token for the member made by new_member.
member_token() {
  local out token
  out="$(_ui sign-in "$(jq -nc --arg ar "$(_authorize)" --arg e "$MEMBER_EMAIL" --arg p "$MEMBER_PASSWORD" \
    '{authRequest: $ar, email: $e, password: $p}')")"
  # Sign-in allows 10 attempts a minute per address: wait the window out once.
  if [[ "$(jq -r '.error.code // empty' <<<"$out" 2>/dev/null)" == rate_limited ]]; then
    sleep 61
    out="$(_ui sign-in "$(jq -nc --arg ar "$(_authorize)" --arg e "$MEMBER_EMAIL" --arg p "$MEMBER_PASSWORD" \
      '{authRequest: $ar, email: $e, password: $p}')")"
  fi
  [[ "$(jq -r .next <<<"$out")" == done ]] || die "sign-in as $MEMBER_EMAIL: $out"
  token="$(_exchange "$(jq -r .redirect <<<"$out")")"
  [[ -n "$token" && "$token" != null ]] || die "no access token for $MEMBER_EMAIL"
  echo "$token"
}
