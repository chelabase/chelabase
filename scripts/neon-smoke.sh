#!/usr/bin/env bash
# Checks the running Neon storage stack (`just neon-up`): safekeeper 1 is Active, the
# pageserver answers, and a throwaway tenant with a timeline can be created, read back and
# deleted. Touches nothing else: no metadata database, no user databases.
set -euo pipefail

STORCON="${STORCON_URL:-http://127.0.0.1:1234}"
PAGESERVER="${PAGESERVER_URL:-http://127.0.0.1:9898}"
# Per-request limit, so a stack that accepts connections but never answers can't hang this.
MAX_TIME="${NEON_SMOKE_MAX_TIME:-60}"

say() { echo "[neon-smoke] $*"; }
die() {
  echo "[neon-smoke] FAIL: $*" >&2
  exit 1
}
hexid() { od -An -N16 -tx1 /dev/urandom | tr -d ' \n'; }

# call <method> <url> [json body]: prints the response body; dies on no answer or non-2xx.
call() {
  local method="$1" url="$2" body="${3:-}" out status
  local args=(-sS --max-time "$MAX_TIME" -o /dev/stdout -w '\n%{http_code}' -X "$method")
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

# Deletion is asynchronous: 202 while in progress, 200 (or 404) once gone.
delete_until_gone() {
  local url="$1" status
  for _ in $(seq 120); do
    status="$(curl -sS --max-time "$MAX_TIME" -o /dev/null -w '%{http_code}' -X DELETE "$url")" ||
      die "DELETE $url: no answer"
    case "$status" in
      200 | 404) return 0 ;;
      202) sleep 0.5 ;;
      *) die "DELETE $url: $status" ;;
    esac
  done
  die "DELETE $url: still in progress after 60 s"
}

tenant="$(hexid)"
timeline="$(hexid)"
created=""
cleanup() {
  local status
  if [[ -n "$created" ]]; then
    status="$(curl -sS --max-time "$MAX_TIME" -o /dev/null -w '%{http_code}' \
      -X DELETE "$STORCON/v1/tenant/$tenant" 2>/dev/null || true)"
    case "$status" in
      2?? | 404) ;;
      *) echo "[neon-smoke] could not delete tenant $tenant (HTTP ${status:-no answer})" >&2 ;;
    esac
  fi
}
trap cleanup EXIT

sks="$(call GET "$STORCON/control/v1/safekeeper")"
jq -e 'any(.[]; .id == 1 and .scheduling_policy == "Active")' <<<"$sks" >/dev/null ||
  die "safekeeper 1 is not Active: $sks"
say "safekeeper 1 is Active"

call GET "$PAGESERVER/v1/status" >/dev/null
say "pageserver answers"

# Marked before the call: the server may create the tenant even if the reply is lost.
created=1
call POST "$STORCON/v1/tenant" \
  "{\"new_tenant_id\":\"$tenant\",\"placement_policy\":{\"Attached\":0}}" >/dev/null
say "created tenant $tenant"

# Creating a tenant attaches it, so the storage controller notifies the control-plane hook.
# A hook that doesn't answer leaves the notification pending (and the create ~15 s slow).
shard="$(call GET "$STORCON/control/v1/tenant/$tenant" | jq -c '.shards[0]')"
jq -e '.is_pending_compute_notification == false' <<<"$shard" >/dev/null ||
  die "tenant $tenant's compute notification is still pending (is the hook answering?): $shard"
say "the control-plane hook was notified"

call POST "$STORCON/v1/tenant/$tenant/timeline" \
  "{\"new_timeline_id\":\"$timeline\",\"pg_version\":17}" >/dev/null
info="$(call GET "$STORCON/v1/tenant/$tenant/timeline/$timeline")"
jq -e --arg t "$timeline" '.timeline_id == $t' <<<"$info" >/dev/null ||
  die "GET timeline $timeline returned: $info"
say "created and read back timeline $timeline"

delete_until_gone "$STORCON/v1/tenant/$tenant/timeline/$timeline"
delete_until_gone "$STORCON/v1/tenant/$tenant"
created=""
say "deleted the timeline and the tenant"
say "ok"
