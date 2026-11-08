#!/usr/bin/env bash
# Offline check of the per-service secret boundary of the self-hosted install (U10, epic
# selfhost-install m8). In the base file every secret lives in one named volume, `secrets`
# (chelabase-init's layout, plus the dir the stack adds: bootstrap/), and each service mounts
# only its own files from it with `volume.subpath`.
#
# Checks, from `docker compose config` of the base file (with .env.example) and of base +
# workbench overlay (with the fixture stack.env):
#   1. each base service's `secrets` mounts are exactly the table below (subpath and ro/rw);
#   2. no other volume or bind of the base reaches a secret (nothing else names `secrets`, and
#      only init-secrets mounts the volume's root);
#   3. per service, the machine-part files a base service mounts are the ones the same service
#      sees under the workbench (its key-dir files), with the same mode. Install-part files
#      (database, S3, setup code, NATS/Valkey auth files, the bootstrap's outputs) have no
#      keys-dir counterpart: the workbench keeps them in each stack's state dir or uses its
#      fixed dev values.
#   4. the identity service's boundary (auth-replacement m5), in both renders: only `identity`
#      mounts `identity/*` (the bootstrap also reads `identity/db.password`, self-hosted: it sets
#      the role's password); only `auth` mounts `auth/member-jwt.key`, and the member public key
#      goes to auth, the gateway, the orchestrator and realtime only; `gateway/internal-signing.pem`
#      is the gateway's alone; each `tokens/<caller>-identity.token` is mounted only by its
#      caller and `identity`. The bootstrap's own (`tokens/bootstrap-identity.token`, which
#      identity lets create a pool and nothing else) goes to the self-hosted `init-bootstrap`
#      and `identity`; under the workbench, to `identity` only (the workbench's bootstrap is a
#      `docker run` of its own, checked in tools/cli). The workbench's runner, the test harness,
#      sees the keys its live tests read and is left out of these checks, but never the
#      bootstrap's token.
#   5. the object-store keys (m2): see there. m19 (#152): the storage service's boundary, in both
#      renders: `nats/storage.password` is storage's alone, `tokens/storage-internal.token` goes
#      to storage and the orchestrator (the bearer both ways), `storage-s3/` to seaweedfs, storage
#      and the worker (the blob sweep); storage runs its binary directly (no shell that could put
#      a secret in argv or its environment), read-only, and names every secret by a `*_FILE`
#      path. NATS restarts when init-secrets is updated, so an upgrade's new NATS users (storage's)
#      reach it.
#   7. the functions service's boundary (edge functions m4b): its NATS password and each internal
#      bearer (the orchestrator's, the worker's), only the gateway mounting the signing key, no
#      Docker socket for functions or the runtime, and the runtime's networks.
#   8. the service runner's boundary (edge functions m10): profile `services`, the only new
#      Docker socket holder, each caller's bearer, and no shared network for service containers
#      (the runner gives each its own at run time, audit B3; the functions service binds none of
#      them).
# Every render takes every profile (sites, runner, services, functions), so profiled services
# are checked.
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
compose chelabase-boundary "$dir/.env.example" -f "$dir/docker-compose.yml" --profile services --profile functions >"$tmp/base.json" ||
  { echo "compose-secret-boundary: rendering the base file failed" >&2; exit 1; }
compose chelabase-feat-x "$root/scripts/tests/fixtures/workbench-stack.env" \
  -f "$dir/docker-compose.yml" -f "$dir/docker-compose.workbench.yml" --profile sites --profile runner --profile services --profile functions \
  | jq -f "$root/scripts/tests/fixtures/secrets-as-binds.jq" >"$tmp/wb.json" ||
  { echo "compose-secret-boundary: rendering base + overlay failed" >&2; exit 1; }

# service <TAB> subpath ("" is the volume's root) <TAB> ro|rw, one line per `secrets` mount.
cat >"$tmp/want" <<'EOF'
metadata-db	db/metadata.password	ro
storcon-db	db/storcon.password	ro
nats	nats/auth.conf	ro
valkey	valkey/users.acl	ro
valkey	valkey/health.password	ro
seaweedfs	s3/access-key	ro
seaweedfs	s3/secret-key	ro
seaweedfs	storage-s3/access-key	ro
seaweedfs	storage-s3/secret-key	ro
seaweedfs	functions-s3/access-key	ro
seaweedfs	functions-s3/secret-key	ro
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
init-bootstrap	identity/db.password	ro
init-bootstrap	tokens/bootstrap-identity.token	ro
identity	identity/secret	ro
identity	identity/db.password	ro
identity	tokens/auth-identity.token	ro
identity	tokens/orchestrator-identity.token	ro
identity	tokens/worker-identity.token	ro
identity	tokens/bootstrap-identity.token	ro
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
orchestrator	tokens/orchestrator-functions.token	ro
orchestrator	tokens/proxy-api.token	ro
orchestrator	tokens/storcon-hook.token	ro
orchestrator	tokens/orchestrator-identity.token	ro
orchestrator	auth/member-jwt.pub	ro
orchestrator	orchestrator-tls	ro
orchestrator	db/metadata.password	ro
orchestrator	setup/setup.code	ro
worker	secrets/secrets.key	ro
worker	neon/compute-jwt.pem	ro
worker	neon/compute-jwt.pub	ro
worker	nats/worker.password	ro
worker	valkey/control.password	ro
worker	realtime/pg.password	ro
worker	tokens/worker-identity.token	ro
worker	compute-ca	rw
worker	orchestrator-tls	rw
worker	realtime-tls	rw
worker	db/metadata.password	ro
worker	storage-s3/access-key	ro
worker	storage-s3/secret-key	ro
worker	tokens/worker-functions.token	ro
worker	tokens/worker-service-runner.token	ro
worker	functions-s3/access-key	ro
worker	functions-s3/secret-key	ro
gateway	gateway/internal-signing.pem	ro
gateway	auth/app-jwt.pub	ro
gateway	auth/member-jwt.pub	ro
gateway	tokens/data-keys.token	ro
gateway	tokens/app-sessions.token	ro
auth	auth/cookie.key	ro
auth	auth/app-jwt.key	ro
auth	auth/app-jwt.pub	ro
auth	auth/member-jwt.key	ro
auth	auth/member-jwt.pub	ro
auth	tokens/auth-identity.token	ro
auth	gateway/internal-verify.pem	ro
auth	compute-ca/ca.crt	ro
auth	tokens/app-auth.token	ro
rest	gateway/internal-verify.pem	ro
rest	compute-ca/ca.crt	ro
rest	tokens/rest-internal.token	ro
storage	gateway/internal-verify.pem	ro
storage	compute-ca/ca.crt	ro
storage	tokens/storage-internal.token	ro
storage	nats/storage.password	ro
storage	storage-s3/access-key	ro
storage	storage-s3/secret-key	ro
functions	secrets/secrets.key	ro
functions	compute-ca/ca.crt	ro
functions	gateway/internal-verify.pem	ro
functions	db/metadata.password	ro
functions	nats/functions.password	ro
functions	tokens/orchestrator-functions.token	ro
functions	tokens/worker-functions.token	ro
functions	tokens/functions-service-runner.token	ro
functions	functions-s3/access-key	ro
functions	functions-s3/secret-key	ro
service-runner	db/metadata.password	ro
service-runner	tokens/functions-service-runner.token	ro
service-runner	tokens/worker-service-runner.token	ro
realtime	gateway/internal-verify.pem	ro
realtime	compute-ca/ca.crt	ro
realtime	nats/realtime.password	ro
realtime	valkey/realtime.password	ro
realtime	realtime/pg.password	ro
realtime	realtime/ticket.key	ro
realtime	realtime/internal.token	ro
realtime	realtime-tls	ro
realtime	auth/member-jwt.pub	ro
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
jq -r --arg k "$keys/" --arg st "$state" '.services | to_entries[] | .key as $s
  | (.value.volumes // [])[] | select(.type == "bind") | (if .read_only then "ro" else "rw" end) as $m
  | if (.source | startswith($st + "/")) then empty
    elif (.source | startswith($k)) then [$s, (.source | ltrimstr($k)), $m]
    else empty end | @tsv' "$tmp/wb.json" | sort >"$tmp/wb.have"
install_part='(db/|s3/|storage-s3/|functions-s3/|grpc-tls/|setup/|bootstrap|nats/auth\.conf|valkey/users\.acl)'
for svc in $(jq -r '.services | keys[]' "$tmp/base.json"); do
  jq -e --arg s "$svc" '.services[$s]' "$tmp/wb.json" >/dev/null || continue
  awk -F'\t' -v s="$svc" '$1 == s' "$tmp/have" | grep -Ev $'\t'"$install_part" >"$tmp/b" || true
  awk -F'\t' -v s="$svc" '$1 == s' "$tmp/wb.have" >"$tmp/w" || true
  if ! diff -u --label "workbench $svc" --label "base $svc" "$tmp/w" "$tmp/b" >"$tmp/diff"; then
    err "$svc mounts other secret files than under the workbench:"
    cat "$tmp/diff" >&2
  fi
done

# 4. The identity boundary, in the base file and under the workbench (the runner left out).
# who <file> <render>: the services mounting <file> (or a directory above it), sorted, one line.
who() {
  awk -F'\t' -v f="$1" '$1 != "runner" && ($2 == f || index(f, $2 "/") == 1 || $2 == "") { print $1 }' "$2" |
    LC_ALL=C sort -u | paste -sd' ' -
}
boundary() { # boundary <file> <expected services> <render label> <render file>
  local got
  got="$(who "$1" "$4")"
  [[ "$got" == "$2" ]] || err "$3: $1 is mounted by [$got], expected [$2]"
}
for render in base wb; do
  file="$tmp/have"
  [[ "$render" == wb ]] && file="$tmp/wb.have"
  # The base's init-secrets mounts the whole volume (it writes it): not a reader.
  grep -v $'^init-secrets\t' "$file" >"$tmp/readers.$render" || true
  boot=" init-bootstrap"
  [[ "$render" == wb ]] && boot=""
  boundary identity/secret "identity" "$render" "$tmp/readers.$render"
  boundary identity/db.password "identity${boot}" "$render" "$tmp/readers.$render"
  boundary auth/member-jwt.key "auth" "$render" "$tmp/readers.$render"
  boundary auth/member-jwt.pub "auth gateway orchestrator realtime" "$render" "$tmp/readers.$render"
  boundary gateway/internal-signing.pem "gateway" "$render" "$tmp/readers.$render"
  boundary tokens/auth-identity.token "auth identity" "$render" "$tmp/readers.$render"
  boundary tokens/orchestrator-identity.token "identity orchestrator" "$render" "$tmp/readers.$render"
  boundary tokens/worker-identity.token "identity worker" "$render" "$tmp/readers.$render"
  boundary tokens/bootstrap-identity.token "identity${boot}" "$render" "$tmp/readers.$render"
  # Nothing else under identity/ is mounted, by anyone but identity (and the bootstrap's file).
  extra="$(awk -F'\t' '$1 != "runner" && $1 != "identity" && ($2 ~ /^identity(\/|$)/) &&
    !($1 == "init-bootstrap" && $2 == "identity/db.password") { print $1 ": " $2 }' "$tmp/readers.$render")"
  [[ -z "$extra" ]] || err "$render: identity/ files reach other services:"$'\n'"$extra"
done
# The bootstrap's token never reaches the runner (S1: init-bootstrap, the workbench's bootstrap
# run and identity only).
! awk -F'\t' '$1 == "runner" && $2 == "tokens/bootstrap-identity.token"' "$tmp/wb.have" | grep -q . ||
  err "the workbench runner sees tokens/bootstrap-identity.token"
# The workbench runner (the test harness) reads the identity files the live tests use.
for f in identity/db.password tokens/worker-identity.token; do
  awk -F'\t' -v f="$f" '$1 == "runner" && $2 == f' "$tmp/wb.have" | grep -q . ||
    err "the workbench runner doesn't see $f (members_pool_idempotent reads it)"
done

# 5. The object-store keys and SeaweedFS's gRPC TLS files (epic object-storage m2). The table
# pins who mounts which file: Neon's `s3/` pair by seaweedfs and the four Neon services,
# `storage-s3/` and `grpc-tls/` by seaweedfs only (until the storage service lands). Here: no
# service gets one of them from its environment or a bind (a value there shows up in `docker
# inspect`), no admin identity or key file exists, and only seaweedfs names these paths at all.
leaks="$(jq -r '.services | to_entries[] | .key as $s
  | ((.value.environment // {}) | to_entries[]
       | select(.key | test("^(AWS_(ACCESS_KEY_ID|SECRET_ACCESS_KEY)|CHELABASE_STORAGE_S3_(ACCESS|SECRET)_KEY)$"))
       | "\($s): environment \(.key) is set in the base file"),
    ((.value.volumes // [])[] | select(.type == "bind")
       | select(.source | test("storage-s3|functions-s3|grpc-tls|/s3/"))
       | "\($s): bind \(.source)")' "$tmp/base.json")"
[[ -z "$leaks" ]] || err "an object-store key reaches a service through its environment or a bind:"$'\n'"$leaks"
# init-secrets, which makes the files, is the one name besides their readers: grpc-tls/ is
# seaweedfs's; storage-s3/ (the `storage` identity) is seaweedfs's, the storage service's and the
# worker's (the blob sweep, m8), in the base file (m19) as under the workbench.
# functions-s3/ (the `functions` identity, edge-functions m1) is seaweedfs's, the functions
# service's (m4) and the worker's (the bundle sweep, m7); never the runtime's.
for want in "grpc-tls:seaweedfs " "storage-s3:seaweedfs storage worker " "functions-s3:functions seaweedfs worker "; do
  dir="${want%%:*}"
  users="$(jq -r --arg d "$dir/" '.services | to_entries[]
    | select(.key != "init-secrets" and (.value | tojson | contains($d))) | .key' "$tmp/base.json" | sort | tr '\n' ' ')"
  [[ "$users" == "${want#*:}" ]] || err "services naming $dir/: got [$users], want [${want#*:}]"
done
users="$(jq -r '.services | to_entries[] | select(any(.value.volumes[]?; (.target // "") | contains("storage-s3/"))) | .key' "$tmp/wb.json" | sort | tr '\n' ' ')"
[[ "$users" == "seaweedfs storage worker " ]] || err "workbench services mounting storage-s3/: got [$users], want [seaweedfs storage worker ]"
users="$(jq -r '.services | to_entries[] | select(any(.value.volumes[]?; (.target // "") | contains("functions-s3/"))) | .key' "$tmp/wb.json" | sort | tr '\n' ' ')"
[[ "$users" == "functions seaweedfs worker " ]] || err "workbench services mounting functions-s3/: got [$users], want [functions seaweedfs worker ]"
! jq -e '[.services | to_entries[] | select(.key == "seaweedfs" or .key == "create-bucket" or (.key | startswith("safekeeper")) or .key == "pageserver")
  | select(.value | tojson | test("s3-admin|\"admin\"|Admin"))] | length > 0' "$tmp/base.json" >/dev/null ||
  err "an admin S3 identity or key file is back"

# The same wiring in the base file: two identities with bucket-scoped actions, mTLS on every
# gRPC listener, S3 directly on the network, no relay, buckets made inside, a real healthcheck.
sw_cmd="$(jq -r '.services.seaweedfs.command | join("\n")' "$tmp/base.json")"
for want in '"Read:neon", "Write:neon", "List:neon", "Tagging:neon"' \
  '"Read:chelabase-storage", "Write:chelabase-storage", "List:chelabase-storage", "Tagging:chelabase-storage"' \
  '"Read:chelabase-functions", "Write:chelabase-functions", "List:chelabase-functions"' \
  's3.bucket.create -name chelabase-functions' \
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

# 6. The storage service (m19): its own secrets only, in both renders (the runner left out).
for render in base wb; do
  boundary nats/storage.password "storage" "$render" "$tmp/readers.$render"
  boundary tokens/storage-internal.token "orchestrator storage" "$render" "$tmp/readers.$render"
done
jq -e '.services.storage' "$tmp/base.json" >/dev/null || err "the base file has no storage service"
st="$(jq -c '.services.storage // {} | {entrypoint, command, read_only, user}' "$tmp/base.json")"
[[ "$st" == '{"entrypoint":null,"command":null,"read_only":true,"user":null}' ]] ||
  err "storage runs its image's binary as its user (10001), read-only, with no entrypoint or command: got $st"
img="$(jq -r '.services.storage.image // ""' "$tmp/base.json")"
[[ "$img" == ghcr.io/chelabase/storage:* ]] || err "storage runs the ghcr.io/chelabase/storage release image: got $img"
# Every secret it mounts is named by a *_FILE setting (or the compute CA / verify key paths),
# and no environment value holds a secret's contents (a path under /run/chelabase/secrets only).
for sub in tokens/storage-internal.token nats/storage.password storage-s3/access-key storage-s3/secret-key; do
  jq -e --arg p "/run/chelabase/secrets/$sub" '.services.storage.environment // {} | to_entries
    | any(.value == $p and (.key | endswith("_FILE")))' "$tmp/base.json" >/dev/null ||
    err "storage reads $sub through a *_FILE setting"
done
bad="$(jq -r '.services.storage.environment // {} | to_entries[]
  | select(.key | test("(TOKEN|PASSWORD|SECRET|_KEY)(_FILE)?$"))
  | select((.value // "") | startswith("/run/chelabase/secrets/") | not) | .key' "$tmp/base.json")"
[[ -z "$bad" ]] || err "storage gets a secret-named setting that isn't a mounted file's path: $bad"
# The webhook allowlist defaults to nothing (public targets only, https only).
for svc in orchestrator worker; do
  got="$(jq -c --arg s "$svc" '.services[$s].environment | [.CHELABASE_WEBHOOKS_ALLOW_HOSTS, .CHELABASE_WEBHOOKS_ALLOW_HTTP]' "$tmp/base.json")"
  [[ "$got" == '["",""]' ]] || err "$svc: the webhook allowlist defaults to empty and http to off; got $got"
done
# An upgrade re-renders nats/auth.conf with the new users; NATS reads it at start only.
[[ "$(jq -r '.services.nats.depends_on["init-secrets"].restart' "$tmp/base.json")" == true ]] ||
  err "nats restarts when init-secrets is updated (depends_on init-secrets: restart: true)"

# 7. The functions service (edge functions m4b), in both renders (the runner left out): its NATS
# password is its own; each internal bearer goes to its one caller and the service.
for render in base wb; do
  boundary nats/functions.password "functions" "$render" "$tmp/readers.$render"
  boundary tokens/orchestrator-functions.token "functions orchestrator" "$render" "$tmp/readers.$render"
  boundary tokens/worker-functions.token "functions worker" "$render" "$tmp/readers.$render"
done
# Only the gateway mounts the signing key, under any name; the runtime and the functions service
# never mount the Docker socket.
for render in base wb; do
  f="$tmp/base.json"
  [[ "$render" == wb ]] && f="$tmp/wb.json"
  got="$(jq -r '.services | to_entries[] | select(.key != "runner")
    | select(any(.value.volumes[]?; ((.target // "") + " " + (.source // "")) | contains("internal-signing.pem"))) | .key' "$f" | sort | tr '\n' ' ')"
  [[ "$got" == "gateway " ]] || err "$render: internal-signing.pem is mounted by [$got], expected [gateway ]"
  for svc in functions functions-runtime; do
    jq -e --arg s "$svc" '.services[$s]' "$f" >/dev/null || { err "$render: no $svc service"; continue; }
    ! jq -e --arg s "$svc" 'any(.services[$s].volumes[]?; (.source // "") | contains("docker.sock"))' "$f" >/dev/null ||
      err "$render: $svc mounts the Docker socket"
  done
done
fs="$(jq -c '.services.functions // {} | {entrypoint, command, read_only, user}' "$tmp/base.json")"
[[ "$fs" == '{"entrypoint":null,"command":null,"read_only":true,"user":null}' ]] ||
  err "functions runs its image's binary as its user, read-only, with no entrypoint or command: got $fs"
img="$(jq -r '.services.functions.image // ""' "$tmp/base.json")"
[[ "$img" == ghcr.io/chelabase/functions:* ]] || err "functions runs the ghcr.io/chelabase/functions release image: got $img"
for sub in nats/functions.password tokens/orchestrator-functions.token tokens/worker-functions.token \
  functions-s3/access-key functions-s3/secret-key db/metadata.password; do
  jq -e --arg p "/run/chelabase/secrets/$sub" '.services.functions.environment // {} | to_entries
    | any(.value == $p and (.key | endswith("_FILE")))' "$tmp/base.json" >/dev/null ||
    err "functions reads $sub through a *_FILE setting"
done
for svc in orchestrator worker; do
  token="tokens/$svc-functions.token"
  var="CHELABASE_$(tr a-z A-Z <<<"$svc")_FUNCTIONS_TOKEN_FILE"
  [[ "$(jq -r --arg s "$svc" --arg v "$var" '.services[$s].environment[$v] // ""' "$tmp/base.json")" == "/run/chelabase/secrets/$token" ]] ||
    err "$svc reads $token through $var"
done
# Networks (the runtime's boundary): the runtime is on `functions` and `functions-egress` only;
# `functions` holds the runtime and the functions service; `functions-control` holds the service
# and its two callers (and the workbench runner, for the e2e); both are internal. The listeners
# bind named aliases, never 0.0.0.0, and the runtime's bundle store is the internal alias.
members() { # members <network> <render file>
  jq -r --arg n "$1" '.services | to_entries[] | select((.value.networks // {}) | has($n)) | .key' "$2" | sort | tr '\n' ' '
}
for render in base wb; do
  f="$tmp/base.json"
  runner=""
  [[ "$render" == wb ]] && { f="$tmp/wb.json"; runner="runner "; }
  got="$(jq -r '.services["functions-runtime"].networks // {} | keys | join(" ")' "$f")"
  [[ "$got" == "functions functions-egress" ]] || err "$render: functions-runtime is on [$got], expected [functions functions-egress]"
  # #183: the gateway joins the runtime's network as `gateway-functions` (the runtime's
  # `env.CHELABASE` binding), and nothing else does.
  got="$(members functions "$f")"
  [[ "$got" == "functions functions-runtime gateway " ]] || err "$render: the functions network holds [$got]"
  got="$(jq -c '.services.gateway.networks.functions.aliases // []' "$f")"
  [[ "$got" == '["gateway-functions"]' ]] || err "$render: the gateway's alias on the functions network: got $got"
  for svc in orchestrator worker metadata-db nats seaweedfs storage rest auth realtime identity; do
    ! jq -e --arg s "$svc" '.services[$s].networks // {} | has("functions")' "$f" >/dev/null ||
      err "$render: $svc must not be on the runtime's network"
  done
  # The binding's address is the gateway's alias and port, and its host the gateway's API host.
  rt_cmd="$(jq -r '.services["functions-runtime"].command // [] | join(" ")' "$f")"
  gw_port="$(jq -r '.services.gateway.environment.CHELABASE_GATEWAY_LISTEN // ""' "$f" | sed 's/.*://')"
  [[ -z "$gw_port" ]] && gw_port="$(sed -n 's/^CHELABASE_GATEWAY_LISTEN=.*://p' "$root/scripts/tests/fixtures/workbench-stack.env")"
  [[ "$rt_cmd" == "/usr/local/bin/workerd serve --experimental --external-addr chelabase-api=gateway-functions:$gw_port /etc/functions-runtime/config.capnp" ]] ||
    err "$render: the runtime's binding must reach gateway-functions:$gw_port; command: $rt_cmd"
  rt_host="$(jq -r '.services["functions-runtime"].environment.CHELABASE_API_HOST // ""' "$f")"
  api_host="$(jq -r '.services.gateway.environment.CHELABASE_API_HOST // ""' "$f")"
  [[ -z "$api_host" ]] && api_host="$(sed -n 's/^CHELABASE_API_HOST=//p' "$root/scripts/tests/fixtures/workbench-stack.env")"
  [[ -n "$rt_host" && "$rt_host" == "$api_host" ]] || err "$render: the runtime's CHELABASE_API_HOST ($rt_host) must be the gateway's API host ($api_host)"
  got="$(jq -r '.services["functions-runtime"].environment // {} | keys | join(" ")' "$f")"
  [[ "$got" == "CHELABASE_API_HOST" ]] || err "$render: the runtime gets CHELABASE_API_HOST and no other setting; got [$got]"
  got="$(members functions-control "$f")"
  [[ "$got" == "functions orchestrator ${runner}worker " ]] || err "$render: the functions-control network holds [$got]"
  got="$(members functions-egress "$f")"
  [[ "$got" == "functions-runtime " ]] || err "$render: the functions-egress network holds [$got]"
  for n in functions functions-control; do
    [[ "$(jq -r --arg n "$n" '.networks[$n].internal' "$f")" == true ]] || err "$render: the $n network must be internal"
  done
  for svc in orchestrator worker; do
    ! jq -e --arg s "$svc" '.services[$s].networks // {} | has("functions")' "$f" >/dev/null ||
      err "$render: $svc must not be on the runtime's network"
  done
  got="$(jq -c '.services.functions.networks // {} | to_entries | map({(.key): ((.value // {}).aliases // [])}) | add' "$f")"
  # m13b: `compute` (no alias: it dials computes and listens on nothing there).
  [[ "$got" == '{"compute":[],"default":["functions-public"],"functions":["functions-internal"],"functions-control":["functions-control"],"services-control":[]}' ]] ||
    err "$render: the functions service's network aliases: got $got"
done
# #181 (audit S1): the runtime's side serves bundle GETs only, the control side uploads and
# invocations only; each binds its own network's alias.
for render in base wb; do
  f="$tmp/base.json"
  [[ "$render" == wb ]] && f="$tmp/wb.json"
  fenv="$(jq -c '.services.functions.environment | if type == "array" then map(split("=") | {(.[0]): (.[1:] | join("="))}) | add else . end
    | {CHELABASE_FUNCTIONS_ADDR, CHELABASE_FUNCTIONS_INTERNAL_ADDR, CHELABASE_FUNCTIONS_CONTROL_ADDR}' "$f")"
  [[ "$fenv" == '{"CHELABASE_FUNCTIONS_ADDR":"functions-public:8091,127.0.0.1:8091","CHELABASE_FUNCTIONS_INTERNAL_ADDR":"functions-internal:8080","CHELABASE_FUNCTIONS_CONTROL_ADDR":"functions-control:8080"}' ]] ||
    err "$render: functions binds its aliases only, one side per network: got $fenv"
done
grep -qF '(name = "bundles", external = (address = "functions-internal:8080"' "$root/infra/functions-runtime/config.capnp" ||
  err "the runtime's bundle store must be functions-internal:8080, the internal listener's runtime-side address"
# Profile `functions` in the base file until the images' first pin (scripts/tests/
# compose-image-pins.sh), always on in the workbench, which builds both images itself.
for svc in functions functions-runtime; do
  [[ "$(jq -c --arg s "$svc" '.services[$s].profiles // []' "$tmp/base.json")" == '["functions"]' ]] ||
    err "base: $svc runs in profile functions only (its image isn't published)"
  [[ "$(jq -r --arg s "$svc" '.services[$s].profiles // [] | length' "$tmp/wb.json")" == 0 ]] ||
    err "wb: $svc runs in every workbench stack (no profile)"
done

# 8. The service runner (edge functions m10), in both renders.
for render in base wb; do
  boundary tokens/functions-service-runner.token "functions service-runner" "$render" "$tmp/readers.$render"
  boundary tokens/worker-service-runner.token "service-runner worker" "$render" "$tmp/readers.$render"
done
for render in base wb; do
  f="$tmp/base.json"
  runner=""
  [[ "$render" == wb ]] && { f="$tmp/wb.json"; runner="runner "; }
  # The Docker socket: the computes' two holders, the service runner, and the workbench's test
  # runner; never functions, the runtime or anything else, in any profile.
  got="$(jq -r '.services | to_entries[] | select(any(.value.volumes[]?; (.source // "") | contains("docker.sock"))) | .key' "$f" | sort | tr '\n' ' ')"
  [[ "$got" == "orchestrator ${runner}service-runner worker " ]] || err "$render: the Docker socket is mounted by [$got]"
  [[ "$(jq -c '.services["service-runner"].profiles // []' "$f")" == '["services"]' ]] ||
    err "$render: service-runner runs in profile services only (off by default)"
  # No shared network for service containers (audit B3): the runner makes one per service at run
  # time and attaches the functions container there; compose declares none. `services-control`:
  # the runner and its two callers, internal. The runner itself is never on `functions` or
  # `functions-control`.
  [[ "$(jq -r '.networks | has("services")' "$f")" == false ]] || err "$render: a shared services network is back"
  got="$(members services "$f")"
  [[ -z "$got" ]] || err "$render: [$got] joins a shared services network"
  got="$(members services-control "$f")"
  [[ "$got" == "functions service-runner worker " ]] || err "$render: the services-control network holds [$got]"
  [[ "$(jq -r '.networks["services-control"].internal' "$f")" == true ]] || err "$render: services-control must be internal"
  got="$(jq -r '.services["service-runner"].networks // {} | keys | join(" ")' "$f")"
  [[ "$got" == "default services-control" ]] || err "$render: service-runner is on [$got]"
  got="$(jq -c '.services["service-runner"].networks["services-control"].aliases // []' "$f")"
  [[ "$got" == '["service-runner-control"]' ]] || err "$render: service-runner's alias on services-control: got $got"
  senv="$(jq -c '.services["service-runner"].environment | if type == "array" then map(split("=") | {(.[0]): (.[1:] | join("="))}) | add else . end
    | {CHELABASE_SERVICE_RUNNER_ADDR, CHELABASE_COMPOSE_PROJECT, CHELABASE_SERVICES_NETWORK}' "$f")"
  [[ "$senv" == '{"CHELABASE_SERVICE_RUNNER_ADDR":"service-runner-control:8094,127.0.0.1:8094","CHELABASE_COMPOSE_PROJECT":"'"$(jq -r .name "$f")"'","CHELABASE_SERVICES_NETWORK":null}' ]] ||
    err "$render: service-runner binds its control alias only and attaches its project's functions container: got $senv"
done
for sub in tokens/functions-service-runner.token tokens/worker-service-runner.token db/metadata.password; do
  jq -e --arg p "/run/chelabase/secrets/$sub" '.services["service-runner"].environment // {} | to_entries
    | any(.value == $p and (.key | endswith("_FILE")))' "$tmp/base.json" >/dev/null ||
    err "service-runner reads $sub through a *_FILE setting"
done
for pair in functions:FUNCTIONS worker:WORKER; do
  svc="${pair%%:*}"
  var="CHELABASE_${pair##*:}_SERVICE_RUNNER_TOKEN_FILE"
  [[ "$(jq -r --arg s "$svc" --arg v "$var" '.services[$s].environment[$v] // ""' "$tmp/base.json")" == "/run/chelabase/secrets/tokens/$svc-service-runner.token" ]] ||
    err "$svc reads its service-runner bearer through $var"
done
sr="$(jq -c '.services["service-runner"] | {read_only, image}' "$tmp/base.json")"
[[ "$sr" == '{"read_only":true,"image":"ghcr.io/chelabase/service-runner:unpublished"}' ]] ||
  err "service-runner runs its release image read-only: got $sr"
[[ "$(jq -r '.services["service-runner"].environment.CHELABASE_SERVICE_RUNNER_URL // ""' "$tmp/base.json")" == http://service-runner-control:8094 ]] ||
  err "CHELABASE_SERVICE_RUNNER_URL is the runner's control alias"

[[ "$fail" == 0 ]] || exit 1
echo "compose-secret-boundary: ok ($(wc -l <"$tmp/have") secret mounts, $(jq '.services | length' "$tmp/base.json") services)"
