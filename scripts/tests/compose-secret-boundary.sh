#!/usr/bin/env bash
# Offline check of the per-service secret boundary of the self-hosted install (U10, epic
# selfhost-install m8). In the base file every secret lives in one named volume, `secrets`
# (chelabase-init's layout, plus the dirs the stack adds: zitadel-pats/, bootstrap/,
# zitadel-config/), and each service mounts only its own files from it with `volume.subpath`.
#
# Checks, from `docker compose config` of the base file (with .env.example) and of base +
# workbench overlay (with the fixture stack.env):
#   1. each base service's `secrets` mounts are exactly the table below (subpath and ro/rw);
#   2. no other volume or bind of the base reaches a secret (nothing else names `secrets`, and
#      only init-secrets mounts the volume's root);
#   3. per service, the machine-part files and the Zitadel PATs a base service mounts are the
#      ones the same service sees under the workbench (its key-dir files and state-dir PATs),
#      with the same mode. Install-part files (database, S3, Zitadel, setup code, NATS/Valkey
#      auth files, the bootstrap's outputs) have no keys-dir counterpart: the workbench keeps
#      them in each stack's state dir or uses its fixed dev values.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
dir="$root/infra/compose"
fail=0
err() {
  echo "compose-secret-boundary: $*" >&2
  fail=1
}
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

compose() { # compose <project> <env file> <-f args…>
  local project="$1" env_file="$2"
  shift 2
  env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -p "$project" --project-directory "$dir" \
    "$@" --env-file "$env_file" config --format json
}
compose chelabase-boundary "$dir/.env.example" -f "$dir/docker-compose.yml" >"$tmp/base.json" ||
  { echo "compose-secret-boundary: rendering the base file failed" >&2; exit 1; }
compose chelabase-feat-x "$root/scripts/tests/fixtures/workbench-stack.env" \
  -f "$dir/docker-compose.yml" -f "$dir/docker-compose.workbench.yml" --profile sites --profile runner \
  | jq -f "$root/scripts/tests/fixtures/secrets-as-binds.jq" >"$tmp/wb.json" ||
  { echo "compose-secret-boundary: rendering base + overlay failed" >&2; exit 1; }

# service <TAB> subpath ("" is the volume's root) <TAB> ro|rw, one line per `secrets` mount.
cat >"$tmp/want" <<'EOF'
metadata-db	db/metadata.password	ro
zitadel-db	db/zitadel-admin.password	ro
storcon-db	db/storcon.password	ro
nats	nats/auth.conf	ro
valkey	valkey/users.acl	ro
valkey	valkey/health.password	ro
zitadel	zitadel/masterkey	ro
zitadel	zitadel/steps.yaml	ro
zitadel	zitadel-config/database.yaml	ro
zitadel	zitadel-pats	rw
seaweedfs	s3/access-key	ro
seaweedfs	s3/secret-key	ro
create-bucket	s3/access-key	ro
create-bucket	s3/secret-key	ro
safekeeper1	s3/access-key	ro
safekeeper1	s3/secret-key	ro
safekeeper1	neon/compute-jwt.pub.pem	ro
safekeeper1	neon/storage-tokens/safekeeper-data.jwt	ro
safekeeper2	s3/access-key	ro
safekeeper2	s3/secret-key	ro
safekeeper2	neon/compute-jwt.pub.pem	ro
safekeeper2	neon/storage-tokens/safekeeper-data.jwt	ro
safekeeper3	s3/access-key	ro
safekeeper3	s3/secret-key	ro
safekeeper3	neon/compute-jwt.pub.pem	ro
safekeeper3	neon/storage-tokens/safekeeper-data.jwt	ro
storage-controller	db/storcon.password	ro
storage-controller	tokens/storcon-hook.token	ro
storage-controller	neon/compute-jwt.pub.pem	ro
storage-controller	neon/storage-tokens/pageserver-api.jwt	ro
storage-controller	neon/storage-tokens/safekeeper-data.jwt	ro
pageserver	s3/access-key	ro
pageserver	s3/secret-key	ro
pageserver	neon/compute-jwt.pub.pem	ro
pageserver	neon/storage-tokens/generations-api.jwt	ro
pageserver	neon/storage-tokens/safekeeper-data.jwt	ro
register-safekeepers	neon/storage-tokens/storcon-admin.jwt	ro
proxy	valkey/proxy.password	ro
proxy	tokens/proxy-api.token	ro
init-secrets		rw
init-bootstrap	db/metadata.password	ro
init-bootstrap	zitadel-pats/admin.pat	ro
init-bootstrap	bootstrap	rw
orchestrator	secrets/secrets.key	ro
orchestrator	gateway/internal-verify.pem	ro
orchestrator	neon/compute-jwt.pem	ro
orchestrator	neon/compute-jwt.pub	ro
orchestrator	nats/orchestrator.password	ro
orchestrator	valkey/control.password	ro
orchestrator	realtime/internal.token	ro
orchestrator	tokens/rest-internal.token	ro
orchestrator	tokens/data-keys.token	ro
orchestrator	tokens/app-auth.token	ro
orchestrator	tokens/app-sessions.token	ro
orchestrator	tokens/proxy-api.token	ro
orchestrator	tokens/storcon-hook.token	ro
orchestrator	orchestrator-tls	ro
orchestrator	db/metadata.password	ro
orchestrator	setup/setup.code	ro
orchestrator	zitadel-pats/admin.pat	ro
orchestrator	bootstrap	ro
worker	secrets/secrets.key	ro
worker	neon/compute-jwt.pem	ro
worker	neon/compute-jwt.pub	ro
worker	nats/worker.password	ro
worker	valkey/control.password	ro
worker	realtime/pg.password	ro
worker	compute-ca	rw
worker	orchestrator-tls	rw
worker	realtime-tls	rw
worker	db/metadata.password	ro
worker	zitadel-pats/admin.pat	ro
gateway	gateway/internal-signing.pem	ro
gateway	auth/app-jwt.pub	ro
gateway	tokens/data-keys.token	ro
gateway	tokens/app-sessions.token	ro
gateway	bootstrap/ids	ro
auth	auth/cookie.key	ro
auth	auth/app-jwt.key	ro
auth	auth/app-jwt.pub	ro
auth	gateway/internal-verify.pem	ro
auth	compute-ca/ca.crt	ro
auth	tokens/app-auth.token	ro
auth	zitadel-pats/admin.pat	ro
auth	zitadel-pats/login-client.pat	ro
auth	bootstrap/ids	ro
rest	gateway/internal-verify.pem	ro
rest	compute-ca/ca.crt	ro
rest	tokens/rest-internal.token	ro
realtime	gateway/internal-verify.pem	ro
realtime	compute-ca/ca.crt	ro
realtime	nats/realtime.password	ro
realtime	valkey/realtime.password	ro
realtime	realtime/pg.password	ro
realtime	realtime/ticket.key	ro
realtime	realtime/internal.token	ro
realtime	realtime-tls	ro
realtime	bootstrap/ids	ro
EOF

# 1. The table.
jq -r '.services | to_entries[] | .key as $s | (.value.volumes // [])[]
  | select(.type == "volume" and .source == "secrets")
  | [$s, (.volume.subpath // ""), (if .read_only then "ro" else "rw" end)] | @tsv' \
  "$tmp/base.json" | sort >"$tmp/have"
sort "$tmp/want" >"$tmp/want.sorted"
if ! diff -u --label want --label base "$tmp/want.sorted" "$tmp/have" >"$tmp/diff"; then
  err "the base file's secret mounts differ from the table:"
  cat "$tmp/diff" >&2
fi

# 2. Nothing else reaches the volume: no service lists it in `secrets:` or `volumes_from`, the
# top-level `secrets:` is gone, and no bind mount points into a Docker volume directory.
others="$(jq -r '.services | to_entries[] | .key as $s
  | ((.value.volumes // [])[] | select(.type == "bind" and (.source | test("/var/lib/docker/volumes")))
       | "\($s): bind \(.source)"),
    (if (.value.secrets // []) | length > 0 then "\($s): compose secrets \(.value.secrets | map(.source) | join(","))" else empty end),
    (if (.value.volumes_from // []) | length > 0 then "\($s): volumes_from" else empty end)' "$tmp/base.json")"
[[ -z "$others" ]] || err "secrets reach a service another way:"$'\n'"$others"
[[ "$(jq '.secrets // {} | length' "$tmp/base.json")" == 0 ]] ||
  err "the base file still has a top-level secrets: block (host files)"

# 3. The same files as under the workbench, service by service.
keys="$(sed -n 's/^CHELABASE_KEYS_DIR=//p' "$root/scripts/tests/fixtures/workbench-stack.env")"
state="$(sed -n 's/^CHELABASE_STACK_DIR=//p' "$root/scripts/tests/fixtures/workbench-stack.env")"
jq -r --arg k "$keys/" --arg st "$state" --arg z "$state/zitadel" '.services | to_entries[] | .key as $s
  | (.value.volumes // [])[] | select(.type == "bind") | (if .read_only then "ro" else "rw" end) as $m
  | if .source == $z then [$s, "zitadel-pats", $m]
    elif (.source | startswith($z + "/")) and (.source | test("/(admin|login-client)\\.pat$"))
      then [$s, "zitadel-pats/" + (.source | ltrimstr($z + "/")), $m]
    elif (.source | startswith($st + "/")) then empty
    elif (.source | startswith($k)) then [$s, (.source | ltrimstr($k)), $m]
    else empty end | @tsv' "$tmp/wb.json" | sort >"$tmp/wb.have"
install_part='(db/|s3/|zitadel/|zitadel-config/|setup/|bootstrap|nats/auth\.conf|valkey/users\.acl)'
for svc in $(jq -r '.services | keys[]' "$tmp/base.json"); do
  jq -e --arg s "$svc" '.services[$s]' "$tmp/wb.json" >/dev/null || continue
  awk -F'\t' -v s="$svc" '$1 == s' "$tmp/have" | grep -Ev $'\t'"$install_part" >"$tmp/b" || true
  awk -F'\t' -v s="$svc" '$1 == s' "$tmp/wb.have" >"$tmp/w" || true
  if ! diff -u --label "workbench $svc" --label "base $svc" "$tmp/w" "$tmp/b" >"$tmp/diff"; then
    err "$svc mounts other secret files than under the workbench:"
    cat "$tmp/diff" >&2
  fi
done

[[ "$fail" == 0 ]] || exit 1
echo "compose-secret-boundary: ok ($(wc -l <"$tmp/have") secret mounts, $(jq '.services | length' "$tmp/base.json") services)"
