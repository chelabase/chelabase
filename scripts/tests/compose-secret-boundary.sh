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
seaweedfs	storage-s3/access-key	ro
seaweedfs	storage-s3/secret-key	ro
seaweedfs	grpc-tls/ca.crt	ro
seaweedfs	grpc-tls/server.crt	ro
seaweedfs	grpc-tls/server.key	ro
seaweedfs	grpc-tls/client.crt	ro
seaweedfs	grpc-tls/client.key	ro
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
orchestrator	tokens/storage-internal.token	ro
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
install_part='(db/|s3/|storage-s3/|grpc-tls/|zitadel/|zitadel-config/|setup/|bootstrap|nats/auth\.conf|valkey/users\.acl)'
for svc in $(jq -r '.services | keys[]' "$tmp/base.json"); do
  jq -e --arg s "$svc" '.services[$s]' "$tmp/wb.json" >/dev/null || continue
  awk -F'\t' -v s="$svc" '$1 == s' "$tmp/have" | grep -Ev $'\t'"$install_part" >"$tmp/b" || true
  awk -F'\t' -v s="$svc" '$1 == s' "$tmp/wb.have" >"$tmp/w" || true
  if ! diff -u --label "workbench $svc" --label "base $svc" "$tmp/w" "$tmp/b" >"$tmp/diff"; then
    err "$svc mounts other secret files than under the workbench:"
    cat "$tmp/diff" >&2
  fi
done

# 4. The object-store keys and SeaweedFS's gRPC TLS files (epic object-storage m2). The table
# pins who mounts which file: Neon's `s3/` pair by seaweedfs and the four Neon services,
# `storage-s3/` and `grpc-tls/` by seaweedfs only (until the storage service lands). Here: no
# service gets one of them from its environment or a bind (a value there shows up in `docker
# inspect`), no admin identity or key file exists, and only seaweedfs names these paths at all.
leaks="$(jq -r '.services | to_entries[] | .key as $s
  | ((.value.environment // {}) | to_entries[]
       | select(.key | test("^(AWS_(ACCESS_KEY_ID|SECRET_ACCESS_KEY)|CHELABASE_STORAGE_S3_(ACCESS|SECRET)_KEY)$"))
       | "\($s): environment \(.key) is set in the base file"),
    ((.value.volumes // [])[] | select(.type == "bind")
       | select(.source | test("storage-s3|grpc-tls|/s3/"))
       | "\($s): bind \(.source)")' "$tmp/base.json")"
[[ -z "$leaks" ]] || err "an object-store key reaches a service through its environment or a bind:"$'\n'"$leaks"
# init-secrets, which makes the files, is the one name besides seaweedfs.
for dir in storage-s3 grpc-tls; do
  users="$(jq -r --arg d "$dir/" '.services | to_entries[]
    | select(.key != "init-secrets" and (.value | tojson | contains($d))) | .key' "$tmp/base.json" | sort | tr '\n' ' ')"
  [[ "$users" == "seaweedfs " ]] || err "services naming $dir/: got [$users], want [seaweedfs ]"
done
! jq -e '[.services | to_entries[] | select(.key == "seaweedfs" or .key == "create-bucket" or (.key | startswith("safekeeper")) or .key == "pageserver")
  | select(.value | tojson | test("s3-admin|\"admin\"|Admin"))] | length > 0' "$tmp/base.json" >/dev/null ||
  err "an admin S3 identity or key file is back"

# The same wiring in the base file: two identities with bucket-scoped actions, mTLS on every
# gRPC listener, S3 directly on the network, no relay, buckets made inside, a real healthcheck.
sw_cmd="$(jq -r '.services.seaweedfs.command | join("\n")' "$tmp/base.json")"
for want in '"Read:neon", "Write:neon", "List:neon", "Tagging:neon"' \
  '"Read:chelabase-storage", "Write:chelabase-storage", "List:chelabase-storage", "Tagging:chelabase-storage"' \
  '-ip=127.0.0.1' '-ip.bind=127.0.0.1' '-s3.ip.bind=0.0.0.0' '-s3.iam=false' 'exec weed server' \
  '/etc/seaweedfs/security.toml' '[grpc.%s]' 'for c in master volume filer s3' '[grpc.client]' \
  's3.bucket.create -name neon' 's3.bucket.create -name chelabase-storage' '/tmp/buckets-ready'; do
  grep -qF -- "$want" <<<"$sw_cmd" || err "seaweedfs: the command must contain: $want"
done
! grep -qE 'nc -lk|"Admin"|"name": "admin"|-s3.port=' <<<"$sw_cmd" || err "seaweedfs: no relay, no admin identity, no moved S3 port"
! grep -qE '"(Read|Write|List|Tagging)"' <<<"$sw_cmd" || err "seaweedfs: no unscoped Read/Write/List/Tagging action"
hc="$(jq -r '.services.seaweedfs.healthcheck.test | join(" ")' "$tmp/base.json")"
for want in '/tmp/buckets-ready' '8333' '<Code>AccessDenied</Code>'; do
  grep -qF -- "$want" <<<"$hc" || err "seaweedfs: the healthcheck must be an S3-level request ($want)"
done
[[ "$(jq -c '.services["create-bucket"] | [.volumes // [], .environment // {}, .depends_on.seaweedfs.condition]' "$tmp/base.json")" == '[[],{},"service_healthy"]' ]] ||
  err "create-bucket holds no volume or credential and only waits for seaweedfs to be healthy"

# Neon's S3 endpoint is one setting (default the compose SeaweedFS), in the safekeepers' args and
# the pageserver's config, so it can be pointed at another host without editing the file. The
# bucket step may run 8 minutes, so the healthcheck's start_period covers it.
endpoint_uses() { # endpoint_uses <json>: "<service>=<endpoint>" for the three safekeepers and the pageserver
  local svc
  for svc in pageserver safekeeper1 safekeeper2 safekeeper3; do
    jq -r --arg s "$svc" '.services[$s].command | map(tostring) | join(" ")' <<<"$1" |
      grep -oP "(?<![_a-z])endpoint='[^']*'" | sed "s|^endpoint='|$svc=|; s|'\$||"
  done | sort | tr '\n' ' ' | sed 's/ $//'
}
want_default="pageserver=http://seaweedfs:8333 safekeeper1=http://seaweedfs:8333 safekeeper2=http://seaweedfs:8333 safekeeper3=http://seaweedfs:8333"
[[ "$(endpoint_uses "$(cat "$tmp/base.json")")" == "$want_default" ]] ||
  err "Neon's S3 endpoint must default to http://seaweedfs:8333 in all four services; got: $(endpoint_uses "$(cat "$tmp/base.json")")"
over="$(CHELABASE_NEON_S3_ENDPOINT=http://other.example:9000 compose chelabase-boundary "$root/infra/compose/.env.example" -f "$root/infra/compose/docker-compose.yml")"
[[ "$(endpoint_uses "$over")" == "${want_default//http:\/\/seaweedfs:8333/http://other.example:9000}" ]] ||
  err "CHELABASE_NEON_S3_ENDPOINT must change Neon's S3 endpoint in all four services"
[[ "$(jq -r '.services.seaweedfs.healthcheck.start_period' "$tmp/base.json")" == 8m0s ]] ||
  err "seaweedfs: the healthcheck start_period must cover the bucket step's 8 minutes"
grep -qF 'kill 1' <<<"$sw_cmd" && grep -qF 'last weed shell output' <<<"$sw_cmd" ||
  err "seaweedfs: the bucket step must log the last output and end the container when it gives up"

[[ "$fail" == 0 ]] || exit 1
echo "compose-secret-boundary: ok ($(wc -l <"$tmp/have") secret mounts, $(jq '.services | length' "$tmp/base.json") services)"
