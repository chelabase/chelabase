#!/usr/bin/env bash
# Offline check that the app tunables in infra/compose/.env.example reach a self-hosted install
# (epic selfhost-install, close-out S1): every `CHELABASE_*=` line, commented or not, in the
# sections "Mode", "Database credentials" and "App users" must arrive, with the value .env gives
# it, in the environment of at least one service of the base file. Compose reads .env only for
# its own substitutions, so a setting docker-compose.yml doesn't pass through is silently
# ignored. Each tunable is set to a unique marker in a scratch env file and looked up in
# `docker compose config`.
#
# Not tunables of a self-hosted install, so checked differently:
#   CHELABASE_MODE             always self-hosted: every service that has it says so, whatever
#                              .env sets (the line is the workbench's)
#   CHELABASE_OPERATOR_TENANT_ID
#                              platform mode only: never passed
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
dir="$root/infra/compose"
example="$dir/.env.example"
fail=0
err() {
  echo "compose-env-tunables: $*" >&2
  fail=1
}
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

sections='Mode|Database credentials|App users'
awk -v want="^# --- ($sections)( .*)? ---\$" '
  /^# --- .* ---$/ { on = ($0 ~ want); next }
  on && match($0, /^(# )?CHELABASE_[A-Z0-9_]+=/) {
    line = substr($0, RSTART, RLENGTH); sub(/^# /, "", line); sub(/=$/, "", line); print line
  }' "$example" | sort -u >"$tmp/names"
[[ "$(wc -l <"$tmp/names")" -ge 10 ]] || err "found only $(wc -l <"$tmp/names") tunables in $example's sections ($sections): did a heading change?"

{
  cp "$example" "$tmp/env"
  while read -r name; do echo "$name=tunable-$name"; done <"$tmp/names"
} >"$tmp/env.all"
mv "$tmp/env.all" "$tmp/env"

docker compose -p chelabase-env-tunables --project-directory "$dir" -f "$dir/docker-compose.yml" \
  --env-file "$tmp/env" config --format json >"$tmp/config.json" 2>"$tmp/config.err" ||
  { cat "$tmp/config.err" >&2; err "docker compose config failed"; exit 1; }

checked=0
while read -r name; do
  case "$name" in
    CHELABASE_MODE)
      modes="$(jq -r '[.services[] | .environment.CHELABASE_MODE // empty] | unique | join(",")' "$tmp/config.json")"
      [[ "$modes" == self-hosted ]] || err "CHELABASE_MODE in the services is '$modes' (wanted self-hosted only, whatever .env says)"
      ;;
    CHELABASE_OPERATOR_TENANT_ID)
      [[ -z "$(jq -r --arg n "$name" '[.services[] | .environment[$n] // empty] | join(",")' "$tmp/config.json")" ]] ||
        err "$name is passed to a service (platform mode only)"
      ;;
    *)
      users="$(jq -r --arg n "$name" '[.services | to_entries[] | select(.value.environment[$n] == "tunable-\($n)") | .key] | join(",")' "$tmp/config.json")"
      [[ -n "$users" ]] || err "$name (in .env.example) reaches no service: pass it as \`$name: \${$name:-}\` to the services that read it"
      ;;
  esac
  checked=$((checked + 1))
done <"$tmp/names"

[[ $fail == 0 ]] || exit 1
echo "compose-env-tunables: ok ($checked tunables)"
