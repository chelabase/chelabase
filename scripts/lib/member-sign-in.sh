# Sign-in helpers for e2e scripts that act as a project member rather than the stack's dev member
# (the tenant admin `api` calls as). Sourced, not run, after neon-common.sh, with $tmp and $GATEWAY
# set. Uses studio-auth.sh (the session cookie and /auth/v1/token).
#
#   new_member <project id> <grant JSON>  invites a person by link, accepts and grants them on
#                                         the project; sets MEMBER_ID (delete it on exit:
#                                         api DELETE /v1/members/$MEMBER_ID)
#   member_token                          a fresh access token for that member (short-lived)

STUDIO="$CHELABASE_ENTRY_STUDIO_URL"
# shellcheck source=studio-auth.sh
. "$(dirname "${BASH_SOURCE[0]}")/studio-auth.sh"
MEMBER_ID=""
MEMBER_EMAIL="$LOG_PREFIX-member-$(date +%s)-$RANDOM@example.com"
MEMBER_PASSWORD="E2e-pass-$(openssl rand -hex 4)A!"

# new_member <project id> <grant JSON>: see the header.
new_member() {
  local status url
  status="$(api POST /v1/invites "$(jq -nc --arg e "$MEMBER_EMAIL" '{email: $e, grants: [], delivery: "link"}')")"
  [[ "$status" == 201 ]] || die "POST /v1/invites: $status $(cat "$tmp/body")"
  MEMBER_ID="$(jq -r '.member.id' "$tmp/body")"
  url="$(jq -r '.invite_url // empty' "$tmp/body")"
  [[ "$url" == "$STUDIO/accept-invite?user="* ]] || die "no invite link: $(cat "$tmp/body")"
  accept_link "$url" "$MEMBER_PASSWORD" >/dev/null
  status="$(api PUT "/v1/projects/$1/grants/$MEMBER_ID" "$2")"
  [[ "$status" == 200 ]] || die "PUT grant for $MEMBER_ID: $status $(cat "$tmp/body")"
}

# member_token → a fresh access token for the member made by new_member.
member_token() { login_token "$MEMBER_EMAIL" "$MEMBER_PASSWORD"; }
