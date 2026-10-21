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
# Fork round 4 (K8): the proxy reads its Valkey password from a file
# (`--redis-plain-password-file=/run/valkey-proxy.password`), so its `--redis-plain=` URL carries
# a user and no password. No gap is listed any more.
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

proxy_args="$(jq -r '.services.proxy | [(.entrypoint // [])[], (.command // [])[]] | join(" ")' <<<"$config")"
# No secret is ever in a service's process environment (fork round 4, F6, user 2026-10-22):
# every binary reads its secrets from their mounts itself (the proxy's
# --control-plane-token-file, the storage controller's --*-jwt-token-file flags, the
# pageserver's --auth-token-file). No service is exempt. A public key (`/run/*.pub.pem`, the
# storage controller's PUBLIC_KEY) is no secret and may be exported.
# Caught: `export NAME="$(cat /run/…)"`, and `NAME="$(cat /run/…)"` followed by `export NAME`.
env_export='^export[[:space:]]+[A-Za-z_][A-Za-z0-9_]*="\$\$\(cat /run/'
public_key='\$\$\(cat /run/[^)]*\.pub\.pem\)'
run_assign='^(export[[:space:]]+)?([A-Za-z_][A-Za-z0-9_]*)="\$\$\(cat /run/'
declare -A run_vars=()
while IFS=$'\t' read -r svc stmt; do
  [[ "$stmt" =~ $public_key ]] && continue
  if [[ "$stmt" =~ $run_assign ]]; then
    run_vars["$svc:${BASH_REMATCH[2]}"]=1
  fi
  [[ "$stmt" =~ $env_export ]] || continue
  fail "$svc exports a secret from a /run file into its environment: $stmt"
done <<<"$statements"
# `export NAME` (or `export A NAME …`) of a variable assigned from a /run file.
while IFS=$'\t' read -r svc stmt; do
  [[ "$stmt" =~ ^export[[:space:]]+([A-Za-z_][A-Za-z0-9_[:space:]]*)$ ]] || continue
  for name in ${BASH_REMATCH[1]}; do
    [[ -n "${run_vars["$svc:$name"]:-}" ]] &&
      fail "$svc exports $name (read from a /run file) into its environment: $stmt"
  done
done <<<"$statements"
[[ "$proxy_args" == *"--control-plane-token-file=/run/proxy-api.token"* ]] ||
  fail "the proxy reads its control-plane token with --control-plane-token-file=/run/proxy-api.token"
sample='export NAME="$$(cat /run/x)"'
[[ "$sample" =~ $env_export ]] || fail "self-test: the env-export pattern misses an export"
sample='NAME="$$(cat /run/x.jwt)"'
[[ "$sample" =~ $run_assign && "${BASH_REMATCH[2]}" == NAME ]] ||
  fail "self-test: the /run assignment pattern misses NAME"
sample='export PUBLIC_KEY="$$(cat /run/storage-auth.pub.pem)"'
[[ "$sample" =~ $public_key ]] || fail "self-test: the public-key pattern misses a .pub.pem"
sample='export T="$$(cat /run/pageserver-api.jwt)"'
! [[ "$sample" =~ $public_key ]] || fail "self-test: the public-key pattern takes a token"

# The storage controller and the pageserver read their tokens from their mounts themselves.
svc_args() { jq -r --arg s "$1" '.services[$s] | [(.entrypoint // [])[], (.command // [])[]] | join(" ")' <<<"$config"; }
storcon_args="$(svc_args storage-controller)"
for want in --pageserver-jwt-token-file=/run/pageserver-api.jwt \
  --safekeeper-jwt-token-file=/run/safekeeper-data.jwt \
  --control-plane-jwt-token-file=/run/storcon-hook.token; do
  [[ "$storcon_args" == *"$want"* ]] || fail "the storage controller reads its token with $want: $storcon_args"
done
for gone in PAGESERVER_JWT_TOKEN SAFEKEEPER_JWT_TOKEN CONTROL_PLANE_JWT_TOKEN; do
  [[ "$storcon_args" != *"$gone"* ]] || fail "the storage controller no longer gets $gone: $storcon_args"
done
ps_args="$(svc_args pageserver)"
[[ "$ps_args" == *"--auth-token-file=/run/safekeeper-data.jwt"* ]] ||
  fail "the pageserver reads its safekeeper token with --auth-token-file=/run/safekeeper-data.jwt"
[[ "$ps_args" != *NEON_AUTH_TOKEN* ]] || fail "the pageserver no longer gets NEON_AUTH_TOKEN"

# The proxy's Valkey URL never carries a password (`redis://user:password@host`): with a user only
# (`redis://proxy@valkey:6379`) the password comes from --redis-plain-password-file.
pw_in_url='--redis-plain=[a-z]+://[^@/ ]+:[^@]*@'
if [[ "$proxy_args" =~ $pw_in_url ]]; then
  fail "the proxy's --redis-plain URL carries a password (user:password@): $proxy_args"
fi
[[ "$proxy_args" == *"--redis-plain=redis://proxy@valkey:6379"* ]] ||
  fail "the proxy dials --redis-plain=redis://proxy@valkey:6379 (no password in the URL): $proxy_args"
[[ "$proxy_args" == *"--redis-plain-password-file=/run/valkey-proxy.password"* ]] ||
  fail "the proxy reads its Valkey password with --redis-plain-password-file=/run/valkey-proxy.password: $proxy_args"
# Checker self-test: a URL with a password in it is caught by the pattern above.
sample='exec proxy --redis-plain=redis://proxy:$(cat /run/x)@valkey:6379'
[[ "$sample" =~ $pw_in_url ]] || fail "self-test: the password-in-URL pattern misses a password"
sample='exec proxy --redis-plain=redis://proxy@valkey:6379'
! [[ "$sample" =~ $pw_in_url ]] || fail "self-test: the password-in-URL pattern flags a user-only URL"

# Guards: the fixes keep the credentials (the healthcheck still logs in as `health` with its
# password file; the storage controller still gets the hook token).
hc="$(jq -r '.services.valkey.healthcheck.test | if type == "array" then join(" ") else . end' <<<"$config")"
[[ "$hc" == *"health"* && "$hc" == *"/run/valkey-health.password"* ]] ||
  fail "valkey's healthcheck logs in as health with /run/valkey-health.password: $hc"
[[ "$storcon_args" == *"/run/storcon-hook.token"* ]] || fail "the storage controller reads /run/storcon-hook.token: $storcon_args"

[[ $bad -eq 0 ]] && echo "compose-secret-argv: ok"
exit $bad
