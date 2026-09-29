# Helpers shared by scripts/neon-smoke.sh, neon-compute-demo.sh and neon-e2e.sh. Sourced, not run.
# Set LOG_PREFIX (the script's name) before sourcing. The scripts that use metadata_sql or
# check_internal_addr must be in the repo root (they cd there first).

compose=(docker compose -f infra/compose/docker-compose.yml --env-file infra/compose/.env.example)

say() { echo "[$LOG_PREFIX] $*"; }
die() {
  echo "[$LOG_PREFIX] FAIL: $*" >&2
  exit 1
}
# hex <bytes>: that many random bytes as lowercase hex.
hex() { od -An -N"$1" -tx1 /dev/urandom | tr -d ' \n'; }
# SQL on stdin; values are passed as psql variables (-v name=value, used as :'name').
metadata_sql() { "${compose[@]}" exec -T metadata-db psql -U cheladb -d cheladb_metadata -v ON_ERROR_STOP=1 -qtA "$@"; }

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
    die "CHELADB_INTERNAL_ADDR='$addr' is not host:port with a numeric port (e.g. 127.0.0.1:8083 or [::1]:8083)"
  port="${BASH_REMATCH[2]}"
  ((10#$port >= 1 && 10#$port <= 65535)) ||
    die "CHELADB_INTERNAL_ADDR='$addr': port $port is out of range (1-65535)"
}
