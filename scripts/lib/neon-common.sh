# Helpers shared by scripts/neon-smoke.sh, neon-e2e.sh and sweep-dbs.sh. Sourced, not run.
# Set LOG_PREFIX (the script's name) before sourcing. Also sources entry.sh: the public entry
# (CHELABASE_ENTRY_{STUDIO,API,AUTH,MAIL}_URL), which the e2e scripts use instead of port literals.

# shellcheck source=entry.sh
. "$(dirname "${BASH_SOURCE[0]}")/entry.sh"

# secret_value <NAME>: $NAME, or the contents of the file $NAME_FILE names (the stack mounts the
# generated bearers, RT1 T1 S27); empty when neither is set.
secret_value() {
  local name="$1" file_var="${1}_FILE"
  if [[ -n "${!name:-}" ]]; then
    printf '%s' "${!name}"
  elif [[ -n "${!file_var:-}" ]]; then
    tr -d '\r\n' <"${!file_var}"
  fi
}

# STORAGE_AUTH: the curl arguments that open the storage controller: its admin NeonJWT, which the
# workbench derives from the compute-JWT key pair into <keys>/neon/storage-tokens/ (the runner
# sees it). The storage controller runs without --dev and refuses a call without it (RT1 T1).
# Empty outside a stack runner (no CHELABASE_KEYS_DIR or no token yet): the controller then
# answers 401 and the failing call is named.
STORAGE_AUTH=()
storage_admin_token="${CHELABASE_KEYS_DIR:-/nonexistent}/neon/storage-tokens/storcon-admin.jwt"
if [[ -r "$storage_admin_token" ]]; then
  STORAGE_AUTH=(-H "Authorization: Bearer $(tr -d '\r\n' <"$storage_admin_token")")
elif [[ -n "${CHELABASE_KEYS_DIR:-}" ]]; then
  echo "[${LOG_PREFIX:-neon}] no $storage_admin_token (just workbench up writes it): storage controller calls go without a token" >&2
fi
unset storage_admin_token

# delete_tenant <tenant id>: deletes it from the storage controller ($STORCON, default
# http://storage-controller:1234). Deletion is asynchronous (202): polls until the tenant is gone, 60 s at most.
delete_tenant() {
  local tenant="$1" status="" url="${STORCON:-http://storage-controller:1234}"
  for _ in $(seq 120); do
    status="$(curl -sS --max-time 60 -o /dev/null -w '%{http_code}' "${STORAGE_AUTH[@]}" -X DELETE "$url/v1/tenant/$tenant")" ||
      die "DELETE tenant $tenant: no answer"
    case "$status" in
      202) sleep 0.5 ;;
      2* | 404) break ;;
      *) die "DELETE tenant $tenant: HTTP $status" ;;
    esac
  done
  [[ "$status" != 202 ]] || die "tenant $tenant is still being deleted after 60 s"
}

say() { echo "[$LOG_PREFIX] $*"; }
die() {
  echo "[$LOG_PREFIX] FAIL: $*" >&2
  exit 1
}
# hex <bytes>: that many random bytes as lowercase hex.
hex() { od -An -N"$1" -tx1 /dev/urandom | tr -d ' \n'; }
# SQL on stdin; values are passed as psql variables (-v name=value, used as :'name').
# The database is the one in CHELABASE_METADATA_DATABASE_URL.
# Runs in the stack's runner: CHELABASE_METADATA_DATABASE_URL (stack.env) reaches the metadata database.
metadata_sql() { psql "${CHELABASE_METADATA_DATABASE_URL:?run through just workbench run}" -v ON_ERROR_STOP=1 -qtA "$@"; }

# call <method> <url> [json body]: prints the response body; dies on no answer or non-2xx.
# Each request is limited to $MAX_TIME seconds (default 60). A call to the storage controller
# ($STORCON) carries its admin token.
call() {
  local method="$1" url="$2" body="${3:-}" out status
  local args=(-sS --max-time "${MAX_TIME:-60}" -o /dev/stdout -w '\n%{http_code}' -X "$method")
  if [[ -n "$body" ]]; then
    args+=(-H 'content-type: application/json' -d "$body")
  fi
  if [[ "$url" == "${STORCON:-http://storage-controller:1234}"/* ]]; then
    args+=("${STORAGE_AUTH[@]}")
  fi
  # On failure, curl's own error (one line) comes before the status line (000).
  out="$(curl "${args[@]}" "$url" 2>&1)" || die "$method $url: no answer (${out%%$'\n'*})"
  status="${out##*$'\n'}"
  out="${out%$'\n'*}"
  [[ "$status" == 2* ]] || die "$method $url: $status $out"
  printf '%s' "$out"
}

# api <method> <path> [json body]: a gateway call as the stack's dev member, the demo tenant's admin
# (needs $GATEWAY, $tmp and $CHELABASE_CLI). Prints the HTTP status; the body goes to $tmp/body.
# The token is asked for per request (`dev token` signs the dev member in once and reuses its
# session): it is short-lived.
api() {
  local token args
  token="$("$CHELABASE_CLI" dev token)" || die "chelabase dev token"
  args=(-sS --max-time 60 -o "$tmp/body" -w '%{http_code}' -X "$1" "$GATEWAY$2" -H "Authorization: Bearer $token")
  if [[ -n "${3:-}" ]]; then args+=(-H 'content-type: application/json' -d "$3"); fi
  curl "${args[@]}" || true
}
