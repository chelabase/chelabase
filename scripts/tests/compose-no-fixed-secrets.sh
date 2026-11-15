#!/usr/bin/env bash
# Offline check (epic selfhost-install m8, AC 4 and the offline half of AC 5): the self-hosted
# install has no fixed credential and passes no secret as a value.
#
#   1. None of the old hard-coded values is in the base file, the build override or
#      .env.example (only the workbench overlay restores them, for its existing volumes).
#   2. In the base file's rendered config, every environment variable whose name says it holds a
#      secret (ends in PASSWORD, SECRET, TOKEN, KEY, PAT, MASTERKEY) is a *_FILE, or a path under
#      /run/chelabase/ (the key paths the apps read, e.g. CHELABASE_SECRETS_KEY), never a value;
#      and no URL in the environment, command or entrypoint carries a password (user:pw@).
#   3. Every Postgres reads its password from a file (POSTGRES_PASSWORD_FILE).
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
dir="$root/infra/compose"
fail=0
err() {
  echo "compose-no-fixed-secrets: $*" >&2
  fail=1
}

# 1. The old values (and the shapes they had), in the files a self-hoster runs.
files=("$dir/docker-compose.yml" "$dir/.env.example")
[[ -f "$dir/docker-compose.build.yml" ]] && files+=("$dir/docker-compose.build.yml")
patterns=(
  'MasterkeyNeedsToHave32Characters'
  'chelabase-dev-secret'
  '"chelabase-dev"'
  'AWS_ACCESS_KEY_ID: *chelabase'
  'AWS_SECRET_ACCESS_KEY: *[^$ ]'
  'POSTGRES_PASSWORD: '
  '--masterkey[ =]'
  '://[A-Za-z0-9_.-]+:[^@/ ]+@'
  'chelabase-dev-[a-z-]+'
)
for f in "${files[@]}"; do
  for p in "${patterns[@]}"; do
    if hits="$(grep -nE -- "$p" "$f")"; then
      err "${f#"$root/"} has a fixed credential (/$p/):"$'\n'"$hits"
    fi
  done
done

# 2 and 3. The rendered base config.
json="$(env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -p chelabase-no-fixed \
  --project-directory "$dir" -f "$dir/docker-compose.yml" --profile services --env-file "$dir/.env.example" \
  config --format json)" || { echo "compose-no-fixed-secrets: rendering the base file failed" >&2; exit 1; }
bad_env="$(jq -r '.services | to_entries[] | .key as $s | (.value.environment // {}) | to_entries[]
  | select(.key | test("(^|_)(PASSWORD|SECRET|TOKEN|KEY|PAT|MASTERKEY|ACCESS_KEY_ID)$"))
  | select((.key | endswith("_FILE")) | not)
  | select((.value // "") | test("^/run/chelabase/") | not)
  | "\($s): \(.key)"' <<<"$json")"
[[ -z "$bad_env" ]] || err "secret-named variables with a value (not a file):"$'\n'"$bad_env"
# CHELABASE_PROXY_CA_FILE is no file a service reads: it is the name the connection details
# give the proxy's public CA certificate on the client (sslrootcert=), so it is exempt.
bad_file="$(jq -r '.services | to_entries[] | .key as $s | (.value.environment // {}) | to_entries[]
  | select(.key | endswith("_FILE")) | select(.key != "CHELABASE_PROXY_CA_FILE")
  | select((.value // "") | test("^/run/chelabase/|^$") | not)
  | "\($s): \(.key)=\(.value)"' <<<"$json")"
[[ -z "$bad_file" ]] || err "*_FILE variables outside /run/chelabase/:"$'\n'"$bad_file"
urls="$(jq -r '.services | to_entries[] | .key as $s
  | [(.value.environment // {} | to_entries[] | .value // ""), ((.value.command // []) | if type == "string" then . else .[] end),
     ((.value.entrypoint // []) | if type == "string" then . else .[] end)][]
  | select(test("://[^/@ ]+:[^/@ ]*@")) | "\($s): \(.)"' <<<"$json")"
[[ -z "$urls" ]] || err "a URL carries a password:"$'\n'"$urls"
pg="$(jq -r '.services | to_entries[] | select(.value.image | test("^postgres:"))
  | select((.value.environment.POSTGRES_PASSWORD_FILE // "") | test("^/run/chelabase/secrets/db/") | not)
  | .key' <<<"$json")"
[[ -z "$pg" ]] || err "a Postgres without POSTGRES_PASSWORD_FILE: $pg"

[[ "$fail" == 0 ]] || exit 1
echo "compose-no-fixed-secrets: ok"
