#!/usr/bin/env bash
# RT1 final fix round, batch A (T1 audit hardening): no secret is ever a program argument in the
# stack. A process's arguments are world-readable (`ps`, /proc/<pid>/cmdline, on the host too),
# its environment isn't. Offline: reads the rendered Compose config (fixture stack.env).
#
# Every service's entrypoint, command and healthcheck is split into shell statements (newline,
# `;`, `&&`, `|`). A statement that reads a mounted secret (`$(cat /run/…)` or `$(cat ${KEYS}/…)`)
# must be an assignment (`NAME="$(cat …)"`, `export NAME="$(cat …)"`, a heredoc's `key='$(cat …)'`
# line, or an env prefix `NAME="$(cat …)" prog …`), and no statement may then pass `$NAME` on.
#
# KNOWN GAP (fork round 4): Neon's proxy takes its Valkey URL only as `--redis-plain` (fork
# proxy/src/binary/proxy.rs:177-178, no `env`), so the proxy's `exec proxy --redis-plain=…` is
# listed, not failed, until the fork reads it from the environment.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
fixture="$root/scripts/tests/fixtures/workbench-stack.env"
bad=0
fail() {
  echo "compose-secret-argv: $*" >&2
  bad=1
}
config="$(env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -p chelabase-feat-x \
  -f "$root/infra/compose/docker-compose.yml" --env-file "$fixture" \
  --profile sites --profile runner config --no-env-resolution --format json)" ||
  { echo "compose-secret-argv: compose config failed" >&2; exit 1; }

# One "<service>\t<statement>" line per shell statement.
statements="$(jq -r '
  .services | to_entries[] | .key as $s
  | [.value.entrypoint, .value.command,
     (.value.healthcheck.test // null | if type == "array" then .[1:] else . end)]
  | map(select(. != null) | if type == "array" then join(" ") else . end)
  | join("\n")
  | gsub("&&"; ";") | gsub("\\|\\|"; ";")
  | [splits("[\n;|]")] | .[]
  | gsub("^\\s+|\\s+$"; "") | sub("^(/bin/)?(ba)?sh -c\\s*"; "") | select(length > 0)
  | "\($s)\t\(.)"' <<<"$config")"

reads_secret='\$\$?\(cat [^)]+\)'
assignment='^(export[[:space:]]+)?[A-Za-z_][A-Za-z0-9_]*='
declare -A names=()
while IFS=$'\t' read -r svc stmt; do
  [[ "$stmt" =~ $reads_secret ]] || continue
  if [[ "$stmt" =~ $assignment ]]; then
    name="${stmt#export }"
    name="${name%%=*}"
    names["$svc:$name"]=1
    continue
  fi
  if [[ "$svc" == proxy && "$stmt" == "exec proxy --redis-plain="* ]]; then
    echo "compose-secret-argv: KNOWN GAP (fork round 4): the proxy's Valkey password is on its command line (--redis-plain)"
    continue
  fi
  fail "$svc passes a secret as a program argument: $stmt"
done <<<"$statements"

# A variable that holds a secret is never expanded into a command line afterwards.
for key in "${!names[@]}"; do
  svc="${key%%:*}" name="${key#*:}"
  while IFS=$'\t' read -r s stmt; do
    [[ "$s" == "$svc" ]] || continue
    if [[ "$stmt" == *"\$\$$name"* || "$stmt" == *"\$\${$name"* || "$stmt" == *"\$$name"* ]] &&
      ! [[ "$stmt" =~ $assignment ]]; then
      fail "$svc expands \$$name (a secret) into a command line: $stmt"
    fi
  done <<<"$statements"
done

# Guards: the fixes keep the credentials (the healthcheck still logs in as `health` with its
# password file; the storage controller still gets the hook token).
hc="$(jq -r '.services.valkey.healthcheck.test | if type == "array" then join(" ") else . end' <<<"$config")"
[[ "$hc" == *"health"* && "$hc" == *"/run/valkey-health.password"* ]] ||
  fail "valkey's healthcheck logs in as health with /run/valkey-health.password: $hc"
storcon="$(jq -r '.services["storage-controller"].entrypoint | join(" ")' <<<"$config")"
[[ "$storcon" == *"/run/storcon-hook.token"* ]] || fail "the storage controller reads /run/storcon-hook.token: $storcon"

[[ $bad -eq 0 ]] && echo "compose-secret-argv: ok"
exit $bad
