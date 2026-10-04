# Helpers shared by scripts/neon-smoke.sh, neon-e2e.sh and the justfile's sweep-dbs. Sourced, not run.
# Set LOG_PREFIX (the script's name) before sourcing.

# delete_tenant <tenant id>: deletes it from the storage controller ($STORCON, default
# http://127.0.0.1:1234). Deletion is asynchronous (202): polls until the tenant is gone, 60 s at most.
delete_tenant() {
  local tenant="$1" status="" url="${STORCON:-http://127.0.0.1:1234}"
  for _ in $(seq 120); do
    status="$(curl -sS --max-time 60 -o /dev/null -w '%{http_code}' -X DELETE "$url/v1/tenant/$tenant")" ||
      die "DELETE tenant $tenant: no answer"
    case "$status" in
      202) sleep 0.5 ;;
      2* | 404) break ;;
      *) die "DELETE tenant $tenant: HTTP $status" ;;
    esac
  done
  [[ "$status" != 202 ]] || die "tenant $tenant is still being deleted after 60 s"
}

neon_common_compose="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../infra/compose" && pwd)"
compose=(docker compose -f "$neon_common_compose/docker-compose.yml" --env-file "$neon_common_compose/.env.example")

say() { echo "[$LOG_PREFIX] $*"; }
die() {
  echo "[$LOG_PREFIX] FAIL: $*" >&2
  exit 1
}
# hex <bytes>: that many random bytes as lowercase hex.
hex() { od -An -N"$1" -tx1 /dev/urandom | tr -d ' \n'; }
# SQL on stdin; values are passed as psql variables (-v name=value, used as :'name').
# The database is the path of CHELABASE_METADATA_DATABASE_URL when set (a branch may run on its own
# metadata database), else chelabase_metadata.
metadata_db() {
  local db="${CHELABASE_METADATA_DATABASE_URL:-}"
  db="${db#*://}"
  [[ "$db" == */* ]] && db="${db#*/}" || db=""
  db="${db%%[?#]*}"
  echo "${db:-chelabase_metadata}"
}
metadata_sql() { "${compose[@]}" exec -T metadata-db psql -U chelabase -d "$(metadata_db)" -v ON_ERROR_STOP=1 -qtA "$@"; }

# call <method> <url> [json body]: prints the response body; dies on no answer or non-2xx.
# Each request is limited to $MAX_TIME seconds (default 60).
call() {
  local method="$1" url="$2" body="${3:-}" out status
  local args=(-sS --max-time "${MAX_TIME:-60}" -o /dev/stdout -w '\n%{http_code}' -X "$method")
  if [[ -n "$body" ]]; then
    args+=(-H 'content-type: application/json' -d "$body")
  fi
  # On failure, curl's own error (one line) comes before the status line (000).
  out="$(curl "${args[@]}" "$url" 2>&1)" || die "$method $url: no answer (${out%%$'\n'*})"
  status="${out##*$'\n'}"
  out="${out%$'\n'*}"
  [[ "$status" == 2* ]] || die "$method $url: $status $out"
  printf '%s' "$out"
}

# check_internal_addr <value>: dies unless it is host:port with a numeric port (1-65535);
# the host may be a bracketed IPv6 address like [::1].
check_internal_addr() {
  local addr="$1" port
  [[ "$addr" =~ ^(\[[0-9A-Fa-f:.]+\]|[A-Za-z0-9._-]+):([0-9]{1,5})$ ]] ||
    die "CHELABASE_INTERNAL_ADDR='$addr' is not host:port with a numeric port (e.g. 127.0.0.1:8083 or [::1]:8083)"
  port="${BASH_REMATCH[2]}"
  ((10#$port >= 1 && 10#$port <= 65535)) ||
    die "CHELABASE_INTERNAL_ADDR='$addr': port $port is out of range (1-65535)"
}

# api <method> <path> [json body]: a gateway call as the demo tenant's user (needs $GATEWAY, $tmp and
# the built chelabase). Prints the HTTP status; the body goes to $tmp/body. The token is minted per
# request: it is short-lived.
api() {
  local token args
  token="$(cargo run -q -p chelabase -- dev token)" || die "chelabase dev token"
  args=(-sS --max-time 60 -o "$tmp/body" -w '%{http_code}' -X "$1" "$GATEWAY$2" -H "Authorization: Bearer $token")
  if [[ -n "${3:-}" ]]; then args+=(-H 'content-type: application/json' -d "$3"); fi
  curl "${args[@]}" || true
}
