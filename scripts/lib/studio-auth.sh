# Studio's sign-in, for e2e scripts: auth's session cookie, then POST /auth/v1/token for a member
# access token, the way Studio and `chelabase dev token` do it. Sourced, not run. Needs $GATEWAY
# (the API host's URL), $STUDIO (Studio's origin), $tmp and die(); the mail helpers need $MAILPIT.
#
#   ui <path> <json> [jar]        POST /auth/v1/<path> as Studio (Origin, X-Chelabase-Studio) with
#                                 the cookie jar (default $tmp/jar): the body on stdout, the
#                                 status in $tmp/ui-status (`$(ui …)` runs in a subshell)
#   status                        that status
#   new_jar                       a fresh, empty cookie jar's path (one per person and sign-in)
#   session_token <jar>           POST /auth/v1/token with the jar's session cookie → a token
#   sign_in <email> <pw> <jar>    POST /auth/v1/sign-in (one rate limit is waited out) → the answer
#   login_token <email> <pw>      sign_in with a fresh jar, which must answer `studio` → a token
#   accept_link <link> <pw>       a code link (`accept-invite?user=…&code=…&t=…`): described, then
#                                 accepted with the code, which sets the password → a token
#   accept_code <link> <code> <pw>  an emailed link (`?user=…&t=…`) with the code from the mail
#   accept_notice <link> <email> <pw>  a notice link (`?t=…`): an existing account accepts with its
#                                 own password → a token
#   mail_id <to> [not this id]    the newest mail to <to> (30 s at most), skipping the given one
#   mail_link <id> <page>         the Studio link to <page> in that mail
#   mail_code <id>                the six-digit code in that mail

: >"$tmp/jar"

ui() {
  local out
  out="$(curl -sS --max-time 60 -b "${3:-$tmp/jar}" -c "${3:-$tmp/jar}" -H "Origin: $STUDIO" \
    -H 'X-Chelabase-Studio: 1' -H 'Content-Type: application/json' \
    -w '\n%{http_code}' -X POST "$GATEWAY/auth/v1/$1" -d "$2")" || die "POST /auth/v1/$1: no answer"
  printf '%s' "${out##*$'\n'}" >"$tmp/ui-status"
  printf '%s' "${out%$'\n'*}"
}
status() { cat "$tmp/ui-status"; }

new_jar() { mktemp -p "$tmp" jar.XXXXXX; }

session_token() {
  local out code token
  out="$(curl -sS --max-time 60 -b "$1" -c "$1" -H "Origin: $STUDIO" -H 'X-Chelabase-Studio: 1' \
    -w '\n%{http_code}' -X POST "$GATEWAY/auth/v1/token")" || die "POST /auth/v1/token: no answer"
  code="${out##*$'\n'}"
  out="${out%$'\n'*}"
  [[ "$code" == 200 ]] || die "POST /auth/v1/token: $code $out"
  token="$(jq -r '.access_token // empty' <<<"$out")"
  [[ -n "$token" ]] || die "POST /auth/v1/token gave no access_token"
  printf '%s' "$token"
}

sign_in() {
  local body out
  body="$(jq -nc --arg e "$1" --arg p "$2" '{email: $e, password: $p}')"
  out="$(ui sign-in "$body" "$3")"
  # Sign-in allows 10 attempts a minute per address, and the e2e scripts before this one share
  # it: wait the window out once.
  if [[ "$(jq -r '.error.code // empty' <<<"$out" 2>/dev/null)" == rate_limited ]]; then
    sleep 61
    out="$(ui sign-in "$body" "$3")"
  fi
  printf '%s' "$out"
}

login_token() {
  local jar out
  jar="$(new_jar)"
  out="$(sign_in "$1" "$2" "$jar")"
  [[ "$(status)" == 200 && "$(jq -r '.next // empty' <<<"$out")" == studio ]] || die "sign-in as $1: $(status) $out"
  session_token "$jar"
}

# _param <url> <name> → that query parameter's value (empty when absent)
_param() { sed -nE "s/.*[?&]$2=([^&]+).*/\\1/p" <<<"$1"; }

# _accept <link> <json body>: describes the invite with the link's token as Studio's accept page
# does (a closed one stops here), accepts with the body in a fresh jar → a token.
_accept() {
  local jar out user t
  [[ "$1" == "$STUDIO/accept-invite?"* ]] || die "not an invite link: $1"
  user="$(_param "$1" user)"
  t="$(_param "$1" t)"
  [[ -n "$t" ]] || die "the invite link carries no token (t)"
  if [[ -n "$user" ]]; then
    ui invite "$(jq -nc --arg u "$user" --arg t "$t" '{user: $u, t: $t}')" >/dev/null
    [[ "$(status)" == 200 ]] || die "describe-invite for an open invite: $(status)"
  fi
  jar="$(new_jar)"
  out="$(ui accept-invite "$2" "$jar")"
  [[ "$(status)" == 200 && "$(jq -r '.next // empty' <<<"$out")" == studio ]] || die "accept-invite: $(status) $out"
  session_token "$jar"
}

accept_link() {
  local code
  code="$(_param "$1" code)"
  [[ -n "$code" ]] || die "the invite link carries no code"
  accept_code "$1" "$code" "$2"
}

accept_code() {
  _accept "$1" "$(jq -nc --arg t "$(_param "$1" t)" --arg u "$(_param "$1" user)" --arg c "$2" --arg p "$3" \
    '{t: $t, user: $u, code: $c, password: $p}')"
}

accept_notice() {
  _accept "$1" "$(jq -nc --arg t "$(_param "$1" t)" --arg e "$2" --arg p "$3" '{t: $t, email: $e, password: $p}')"
}

mail_id() {
  local id
  for _ in $(seq 30); do
    id="$(curl -sS --max-time 30 -G "$MAILPIT/api/v1/search" --data-urlencode "query=to:$1" | jq -r '.messages[0].ID // empty')"
    if [[ -n "$id" && "$id" != "${2:-}" ]]; then
      echo "$id"
      return 0
    fi
    sleep 1
  done
  die "no new mail to $1"
}

# _mail_text <id>: the mail's text part, with HTML-escaped ampersands undone.
_mail_text() { curl -sS --max-time 30 "$MAILPIT/api/v1/message/$1" | jq -r '.Text' | tr -d '\r' | sed 's/&amp;/\&/g'; }

mail_link() {
  local link
  link="$(_mail_text "$1" | grep -oE "$STUDIO/$2(\\?[^ \"<>]+)?" | head -n1)" || true
  [[ -n "$link" ]] || die "mail $1 has no $STUDIO/$2 link"
  echo "$link"
}

mail_code() {
  local code
  code="$(_mail_text "$1" | grep -oE '^[[:space:]]+[0-9]{6}[[:space:]]*$' | tr -d '[:space:]' | head -c 6)" || true
  [[ "$code" =~ ^[0-9]{6}$ ]] || die "mail $1 has no six-digit code"
  echo "$code"
}
