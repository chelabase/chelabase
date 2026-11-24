#!/usr/bin/env bash
# Offline check (#308) that every key and token file a service reads through the strict reader
# (chelabase_types::file_value::read_private_file: no symlink, a regular file, 0600 for a
# secret, 0644 for a public key) reaches the service as that file itself, so the file
# chelabase-init made (owner, mode, regular file) is what the service opens:
#   1. the list below is chelabase-init's STRICT_READ (crates/chelabase-init/src/secrets.rs);
#   2. base file: each mount of one of them is the `secrets` volume's sub-path of that very file,
#      read-only, at /run/chelabase/secrets/<path>; never a bind, a Compose secret, or a
#      directory mount that holds it (where a symlink could stand in for it);
#   3. base file: each reader setting a service has (CHELABASE_SECRETS_KEY, the key paths,
#      CHELABASE_IDENTITY_TOKEN_FILE) names a file the service mounts that way;
#   4. workbench overlay: each is a Compose file secret (a read-only bind of the file in the keys
#      dir, mode and owner kept) of the layout file itself, with no uid, gid or mode of its own
#      (outside Swarm Compose ignores them or copies the file to apply them; either way the
#      file the reader checks must be the layout file as made), at the setting's path.
# The modes themselves are chelabase-init's: its tests pin them (strict_read_files_are_made_
# private_regular_files) and its check refuses an existing file with any other owner or mode.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
dir="$root/infra/compose"
fail=0
err() {
  echo "compose-strict-key-files: $*" >&2
  fail=1
}
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

strict='[
  "secrets/secrets.key",
  "gateway/internal-signing.pem", "gateway/internal-verify.pem",
  "auth/cookie.key",
  "auth/app-jwt.key", "auth/app-jwt.pub",
  "auth/member-jwt.key", "auth/member-jwt.pub",
  "tokens/auth-identity.token", "tokens/orchestrator-identity.token",
  "tokens/worker-identity.token", "tokens/bootstrap-identity.token"
]'
settings='[
  "CHELABASE_SECRETS_KEY", "CHELABASE_INTERNAL_SIGNING_KEY", "CHELABASE_INTERNAL_VERIFY_KEY",
  "CHELABASE_AUTH_COOKIE_KEY", "CHELABASE_APP_JWT_PRIVATE_KEY", "CHELABASE_APP_JWT_PUBLIC_KEY",
  "CHELABASE_MEMBER_JWT_PRIVATE_KEY", "CHELABASE_MEMBER_JWT_PUBLIC_KEY",
  "CHELABASE_IDENTITY_TOKEN_FILE"
]'

# 1. The same list as chelabase-init's.
init_list="$(sed -n '/^pub const STRICT_READ/,/^];/p' "$root/crates/chelabase-init/src/secrets.rs" |
  grep -o '"[^"]*"' | tr -d '"' | sort)"
here="$(jq -r '.[]' <<<"$strict" | sort)"
[[ -n "$init_list" && "$init_list" == "$here" ]] ||
  err "the list here differs from chelabase-init's STRICT_READ: init has [$(echo $init_list)], here [$(echo $here)]"

compose() { # compose <project> <env file> <-f args…>
  local project="$1" env_file="$2"
  shift 2
  env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -p "$project" --project-directory "$dir" \
    "$@" --env-file "$env_file" config --format json
}
compose chelabase-strict "$dir/.env.example" -f "$dir/docker-compose.yml" \
  --profile services --profile compute-egress --profile mcp >"$tmp/base.json" ||
  { echo "compose-strict-key-files: rendering the base file failed" >&2; exit 1; }
compose chelabase-feat-x "$root/scripts/tests/fixtures/workbench-stack.env" \
  -f "$dir/docker-compose.yml" -f "$dir/docker-compose.workbench.yml" \
  --profile sites --profile runner --profile services --profile compute-egress >"$tmp/wb.json" ||
  { echo "compose-strict-key-files: rendering base + overlay failed" >&2; exit 1; }

# 2. Base: every mount that reaches a strict file.
jq -r --argjson strict "$strict" '
  .services | to_entries[] | .key as $svc | (.value.volumes // [])[]
  | . as $m
  | ($m.volume.subpath // "") as $sub
  | $strict[] as $p
  | ("/run/chelabase/secrets/" + $p) as $want
  | select($sub == $p or ($m.target // "") == $want
      or (($m.type == "bind" or $m.type == "volume") and ($want | startswith(($m.target // "") + "/"))
          and ($m.target // "") != ""))
  | if $m.type == "volume" and $m.source == "secrets" and $sub == $p and $m.target == $want
       and $m.read_only == true then empty
    elif $m.type == "volume" and $m.source == "secrets" and $sub == "" and $svc == "init-secrets" then empty
    else "\($svc): \($p) reaches it through \($m.type) \($m.source // "") \($sub) at \($m.target) (read_only \($m.read_only // false)); want the secrets volume sub-path \($p), read-only, at \($want)"
    end' "$tmp/base.json" >"$tmp/bad-mounts"
while IFS= read -r line; do err "base: $line"; done <"$tmp/bad-mounts"
# No Compose secret in the base file at all (U10: every secret is in the volume).
jq -r '.services | to_entries[] | select((.value.secrets // []) | length > 0) | .key' "$tmp/base.json" |
  while IFS= read -r svc; do err "base: $svc has Compose secrets; the base mounts the secrets volume only"; done

# 3. Base: each reader setting names a file the service mounts as in 2.
jq -r --argjson strict "$strict" --argjson settings "$settings" '
  .services | to_entries[] | .key as $svc | .value as $s
  | ($s.environment // {}) | to_entries[] | select(.key as $k | $settings | index($k))
  | select((.value // "") != "")
  | .key as $k | .value as $path
  | [ ($s.volumes // [])[] | select(.target == $path) ] as $at
  | if ($at | length) == 0 then
      # A setting the service gets from the shared env block but mounts no file for: it never
      # reads it (the reader is not built in that service), unless it is a reader of it.
      empty
    elif ($at | length) == 1 and $at[0].type == "volume" and $at[0].source == "secrets"
         and $at[0].read_only == true
         and ($strict | index($at[0].volume.subpath)) and ("/run/chelabase/secrets/" + $at[0].volume.subpath) == $path
    then empty
    else "\($svc): \($k)=\($path) is not the secrets volume sub-path of a strict file, read-only"
    end' "$tmp/base.json" >"$tmp/bad-settings"
while IFS= read -r line; do err "base: $line"; done <"$tmp/bad-settings"
# The services that read each file mount it (a reader that loses its mount fails at start, as it
# should, but the stack would not come up: catch it here).
while IFS=$'\t' read -r svc var; do
  path="$(jq -r --arg s "$svc" --arg v "$var" '.services[$s].environment[$v] // ""' "$tmp/base.json")"
  [[ -n "$path" ]] || { err "base: $svc doesn't set $var"; continue; }
  n="$(jq --arg s "$svc" --arg p "$path" '[.services[$s].volumes[]? | select(.target == $p)] | length' "$tmp/base.json")"
  [[ "$n" == 1 ]] || err "base: $svc reads $var=$path but mounts it $n times"
done <<'EOF'
orchestrator	CHELABASE_SECRETS_KEY
orchestrator	CHELABASE_INTERNAL_VERIFY_KEY
orchestrator	CHELABASE_MEMBER_JWT_PUBLIC_KEY
orchestrator	CHELABASE_IDENTITY_TOKEN_FILE
worker	CHELABASE_SECRETS_KEY
worker	CHELABASE_IDENTITY_TOKEN_FILE
functions	CHELABASE_SECRETS_KEY
functions	CHELABASE_INTERNAL_VERIFY_KEY
gateway	CHELABASE_INTERNAL_SIGNING_KEY
gateway	CHELABASE_APP_JWT_PUBLIC_KEY
gateway	CHELABASE_MEMBER_JWT_PUBLIC_KEY
auth	CHELABASE_AUTH_COOKIE_KEY
auth	CHELABASE_APP_JWT_PRIVATE_KEY
auth	CHELABASE_APP_JWT_PUBLIC_KEY
auth	CHELABASE_MEMBER_JWT_PRIVATE_KEY
auth	CHELABASE_MEMBER_JWT_PUBLIC_KEY
auth	CHELABASE_INTERNAL_VERIFY_KEY
auth	CHELABASE_IDENTITY_TOKEN_FILE
rest	CHELABASE_INTERNAL_VERIFY_KEY
realtime	CHELABASE_INTERNAL_VERIFY_KEY
realtime	CHELABASE_MEMBER_JWT_PUBLIC_KEY
storage	CHELABASE_INTERNAL_VERIFY_KEY
mcp	CHELABASE_MEMBER_JWT_PUBLIC_KEY
init-bootstrap	CHELABASE_IDENTITY_TOKEN_FILE
EOF

# 4. Workbench: each strict file is a file secret of the layout file itself, mounted with no
# uid, gid or mode of its own, at the path its reader setting names.
keys="$(sed -n 's/^CHELABASE_KEYS_DIR=//p' "$root/scripts/tests/fixtures/workbench-stack.env")"
[[ -n "$keys" ]] || err "workbench: the fixture sets no CHELABASE_KEYS_DIR"
jq -r --argjson strict "$strict" --arg keys "$keys" '
  . as $r
  | ($r.secrets // {}) | to_entries[]
  | .key as $name | .value.file as $file
  | ($strict[] | select(($keys + "/" + .) == $file)) as $p
  | $r.services | to_entries[] | .key as $svc
  | (.value.secrets // [])[]
  | (if type == "string" then {source: .} else . end)
  | select(.source == $name)
  | if (has("uid") or has("gid") or has("mode")) then
      "\($svc): secret \($name) (\($p)) sets uid, gid or mode; mount the layout file as it is"
    elif (.target // "") != $file then
      "\($svc): secret \($name) (\($p)) lands at \(.target // ("/run/secrets/" + $name)), not at \($file)"
    else empty end' "$tmp/wb.json" >"$tmp/bad-wb"
while IFS= read -r line; do err "workbench: $line"; done <"$tmp/bad-wb"
# Every strict file the workbench uses is one of those secrets, of the layout file itself.
jq -r --argjson strict "$strict" --arg keys "$keys" '
  [(.secrets // {})[] | .file] as $files
  | $strict[] | select(($keys + "/" + .) as $f | $files | index($f) | not)
  | "\(.) is not a file secret of \($keys)/\(.)"' "$tmp/wb.json" >"$tmp/missing-wb"
while IFS= read -r line; do err "workbench: $line"; done <"$tmp/missing-wb"

[[ "$fail" == 0 ]] || exit 1
echo "compose-strict-key-files: ok"
