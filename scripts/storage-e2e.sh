#!/usr/bin/env bash
# Live object-storage story through the gateway, with no Studio (object-storage m16, #107). Needs
# the stack (just workbench up); runs in its runner: `just storage-e2e [--backend s3]`.
#
#  1. a database, storage policies written by its owner, and a secret key (role `authenticated`);
#  2. a bucket; an upload and its download; a resumable upload in two parts, byte for byte;
#  3. a signed URL serves without a key; the bucket made public serves the public path, and
#     private again answers 404;
#  4. a branch sees main's objects; a delete or an upload on the branch never reaches main;
#  5. a delete on main: the object and its signed URL answer 404;
#  6. the blobs are in the object store under `<database id>/`; deleting the branch and the
#     database purges the prefix (the worker's sweep), within 120 s.
#
# --backend s3 runs the same story with the storage service and the worker switched to backend
# `s3` (env only: CHELABASE_STORAGE_BACKEND, CHELABASE_STORAGE_S3_*), against a second
# S3-compatible endpoint: a throwaway SeaweedFS (`seaweedfs-s3`, data on tmpfs, bucket
# `chelabase-storage-s3`, the same `storage` key files), started for the run. Step 6 also checks
# that the stack's own SeaweedFS got nothing of the database. On exit, pass or fail, both
# services are recreated as the stack defines them and the endpoint is removed. That recreates
# the stack's storage and worker containers: don't run it while another live run uses the stack.
# `--restore` only does that restore (after a run that was killed before its own).
#
# KEEP=1 leaves the database in place (and, with --backend s3, the swap: restore with --restore).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=storage-e2e
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
GATEWAY="${GATEWAY_URL:-$CHELABASE_ENTRY_API_URL}"
PROXY_PG_PORT="${CHELABASE_PROXY_PG_PORT:-4432}"
DEMO_TENANT_ID="00000000-0000-4000-8000-000000000001"
DEMO_PROJECT_ID="00000000-0000-4000-8000-000000000002"
project="${CHELABASE_COMPOSE_PROJECT:?run through just workbench run}"
state="${CHELABASE_STACK_DIR:?run through just workbench run}"
net="${project}_default"
tmp="$(mktemp -d)"
db_id=""
dev_id=""
swapped=""
t0=$SECONDS

pass() { echo "ok   $* ($((SECONDS - t0)) s)"; }
die() {
  echo "FAIL $*" >&2
  exit 1
}

backend=seaweedfs
restore_only=""
while (($#)); do
  case "$1" in
    --backend)
      backend="${2:-}"
      shift 2 || die "--backend needs a value"
      ;;
    --backend=*)
      backend="${1#*=}"
      shift
      ;;
    --restore)
      restore_only=1
      shift
      ;;
    *) die "unknown argument $1 (wanted --backend seaweedfs|s3, or --restore)" ;;
  esac
done
[[ "$backend" == seaweedfs || "$backend" == s3 ]] || die "--backend is seaweedfs or s3, not '$backend'"

# --- the stack's compose project, and the S3 swap ------------------------------------------------

# The stack's own checkout, never the task checkout the runner works in: recreated services must
# stay on the stack's files and binaries.
stack_checkout="${CHELABASE_STACK_CHECKOUT:-${CHELABASE_CHECKOUT:?run through just workbench run}}"
SWAP_FILE="$state/storage-e2e/s3-swap.yml"
SWAP_ENDPOINT=http://seaweedfs-s3:8333
SWAP_BUCKET=chelabase-storage-s3
# compose [-f extra] <args…>: docker compose on the stack's project, base file and overlay first.
compose() {
  local files=(-f "$stack_checkout/infra/compose/docker-compose.yml" -f "$stack_checkout/infra/compose/docker-compose.workbench.yml")
  if [[ "${1:-}" == -f ]]; then
    files+=(-f "$2")
    shift 2
  fi
  CHELABASE_CHECKOUT="$stack_checkout" docker compose -p "$project" "${files[@]}" --env-file "$state/env/stack.env" "$@"
}

# write_swap_file: the second endpoint, and storage and the worker switched to backend `s3` on it.
# Only environment changes for the two services; the key files are the stack's `storage` pair.
write_swap_file() {
  local image
  image="$(docker inspect -f '{{.Config.Image}}' "$project-seaweedfs-1")" || die "the stack's seaweedfs container isn't there"
  mkdir -p "$(dirname "$SWAP_FILE")"
  cat >"$SWAP_FILE" <<EOF
# Written by scripts/storage-e2e.sh --backend s3; removed when the run restores the stack.
x-s3-backend: &s3-backend
  - CHELABASE_STORAGE_BACKEND=s3
  - CHELABASE_STORAGE_S3_ENDPOINT=$SWAP_ENDPOINT
  - CHELABASE_STORAGE_S3_REGION=us-east-1
  - CHELABASE_STORAGE_S3_PATH_STYLE=true
  - CHELABASE_STORAGE_S3_BUCKET=$SWAP_BUCKET
services:
  seaweedfs-s3:
    image: $image
    restart: "no"
    entrypoint: ["/bin/sh", "-c"]
    command:
      - |
        set -e
        umask 077
        key() { tr -d '\n' < "/run/chelabase/secrets/storage-s3/\$\$1"; }
        {
          printf '{"identities": [{"name": "storage", "credentials": [{"accessKey": "'
          key access-key; printf '", "secretKey": "'; key secret-key
          printf '"}], "actions": ["Read:$SWAP_BUCKET", "Write:$SWAP_BUCKET", "List:$SWAP_BUCKET", "Tagging:$SWAP_BUCKET"]}]}\n'
        } > /tmp/s3.json
        (
          i=0
          while [ "\$\$i" -lt 40 ]; do
            if printf 's3.bucket.create -name $SWAP_BUCKET\ns3.bucket.list\n' |
              timeout 10 weed shell -master=127.0.0.1:9333 -filer=127.0.0.1:8888 2>&1 | grep -qw $SWAP_BUCKET; then
              touch /tmp/bucket-ready; exec sleep 2147483647
            fi
            i=\$\$((i + 1)); sleep 2
          done
          kill 1
        ) &
        exec weed server -ip=127.0.0.1 -ip.bind=127.0.0.1 -s3 -s3.ip.bind=0.0.0.0 -s3.iam=false -s3.port.iceberg=0 -s3.port.lance=0 -s3.config=/tmp/s3.json -dir=/data -volume.max=8 -master.volumeSizeLimitMB=64
    tmpfs: [/data]
    networks: [default]
    secrets:
      - {source: s3-storage-access-key, target: /run/chelabase/secrets/storage-s3/access-key}
      - {source: s3-storage-secret-key, target: /run/chelabase/secrets/storage-s3/secret-key}
    healthcheck:
      test:
        - CMD-SHELL
        - "[ -e /tmp/bucket-ready ] && printf 'GET / HTTP/1.0\\\\r\\\\n\\\\r\\\\n' | nc -w 3 127.0.0.1 8333 | grep -q '<Code>AccessDenied</Code>'"
      interval: 2s
      retries: 60
      start_period: 2m
  storage:
    environment: *s3-backend
  worker:
    environment: *s3-backend
EOF
}

# restore: storage and the worker as the stack defines them, and no second endpoint.
restore() {
  echo "[$LOG_PREFIX] restoring the stack's storage and worker (backend seaweedfs)"
  compose up -d --no-deps --force-recreate --wait storage worker || echo "[$LOG_PREFIX] FAIL: recreating storage and worker failed: run just storage-e2e --restore" >&2
  if [[ -f "$SWAP_FILE" ]]; then
    compose -f "$SWAP_FILE" rm -sf seaweedfs-s3 >/dev/null 2>&1 || true
    rm -f "$SWAP_FILE"
  fi
  docker rm -f "$project-seaweedfs-s3-1" >/dev/null 2>&1 || true
}

if [[ -n "$restore_only" ]]; then
  restore
  exit 0
fi

cleanup() {
  if [[ -n "$db_id" && "${KEEP:-}" != 1 ]]; then
    api DELETE "/v1/databases/$db_id" >/dev/null || true
  fi
  if [[ -n "$swapped" && "${KEEP:-}" != 1 ]]; then
    restore
  fi
  rm -rf "$tmp"
}

[[ -x "${CHELABASE_CLI:-}" ]] || die "CHELABASE_CLI is unset or not executable (run through: just workbench run scripts/<name>.sh)"
PROXY_HOSTADDR="${CHELABASE_PROXY_HOSTADDR:?run through just workbench run}"
ca="${CHELABASE_PROXY_CA_FILE:?run through just workbench run}"
[[ -s "$ca" ]] || die "$ca is missing (run just workbench up)"
trap cleanup EXIT

if [[ "$backend" == s3 ]]; then
  write_swap_file
  swapped=1
  compose -f "$SWAP_FILE" up -d --no-deps --wait seaweedfs-s3 >/dev/null 2>"$tmp/compose.err" ||
    die "starting the second S3 endpoint: $(tail -5 "$tmp/compose.err")"
  compose -f "$SWAP_FILE" up -d --no-deps --force-recreate --wait storage worker >/dev/null 2>"$tmp/compose.err" ||
    die "recreating storage and the worker on backend s3: $(tail -5 "$tmp/compose.err")"
  for svc in storage worker; do
    got="$(docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$project-$svc-1" | grep -E '^CHELABASE_STORAGE_(BACKEND|S3_ENDPOINT|S3_BUCKET)=' | sort | tr '\n' ' ')"
    [[ "$got" == "CHELABASE_STORAGE_BACKEND=s3 CHELABASE_STORAGE_S3_BUCKET=$SWAP_BUCKET CHELABASE_STORAGE_S3_ENDPOINT=$SWAP_ENDPOINT " ]] ||
      die "$svc isn't on the second endpoint: $got"
  done
  pass "storage and the worker run backend s3 against $SWAP_ENDPOINT (bucket $SWAP_BUCKET)"
  STORE_ENDPOINT="$SWAP_ENDPOINT" STORE_BUCKET="$SWAP_BUCKET"
else
  STORE_ENDPOINT=http://seaweedfs:8333 STORE_BUCKET=chelabase-storage
fi

# --- helpers ---------------------------------------------------------------------------------------

# ok <method> <path> <expected status> [body]: a management call, or die.
ok() {
  local status
  status="$(api "$1" "$2" "${4:-}")"
  [[ "$status" == "$3" ]] || die "$1 $2: $status (wanted $3) $(cat "$tmp/body")"
}
wait_db() {
  local s=""
  for _ in $(seq 120); do
    ok GET "/v1/databases/$db_id" 200
    s="$(jq -r '.status' "$tmp/body")"
    case "$s" in
      ready) return ;;
      failed) die "database $db_id failed: $(jq -r '.error' "$tmp/body")" ;;
    esac
    sleep 1
  done
  die "database $db_id is still $s after 120 s"
}
branch_field() {
  ok GET "/v1/databases/$db_id/branches" 200
  jq -r --arg id "$1" ".[] | select(.id == \$id) | .$2 // empty" "$tmp/body"
}
wait_branch() {
  local s=""
  for _ in $(seq 180); do
    s="$(branch_field "$1" status)"
    [[ "$s" != ready ]] || return 0
    [[ "$s" != failed ]] || die "branch $1 failed: $(branch_field "$1" error)"
    sleep 1
  done
  die "branch $1 is '$s' after 180 s (wanted ready)"
}
# psql_main <psql args…>: as the owner on main, through the proxy; retries while the compute starts.
psql_main() {
  local host pw name out
  ok GET "/v1/databases/$db_id/branches/$db_id/connection" 200
  host="$(jq -r '.host' "$tmp/body")"
  pw="$(jq -r '.password' "$tmp/body")"
  name="$(jq -r '.database' "$tmp/body")"
  for _ in $(seq 60); do
    if out="$(PGPASSWORD="$pw" PGCONNECT_TIMEOUT=10 psql \
      "host=$host hostaddr=$PROXY_HOSTADDR port=$PROXY_PG_PORT user=postgres dbname=$name sslmode=verify-full sslrootcert=$ca" \
      -v ON_ERROR_STOP=1 -qtA "$@" 2>"$tmp/err")"; then
      printf '%s' "$out"
      return
    fi
    sleep 2
  done
  die "psql: $(cat "$tmp/err")"
}
# new_key <branch id> <name>: a secret key (role authenticated) on that branch.
new_key() {
  ok POST "/v1/databases/$db_id/branches/$1/keys" 201 "$(jq -nc --arg n "$2" '{name: $n}')"
  jq -r '.key' "$tmp/body"
}
# st <key> <method> <path> [curl args…]: the storage API with that key. Prints the status; the
# body goes to $tmp/sbody. The key goes through stdin, never argv.
st() {
  local key="$1" m="$2" p="$3"
  shift 3
  printf 'Authorization: Bearer %s\n' "$key" |
    curl -sS --max-time 120 -o "$tmp/sbody" -w '%{http_code}' -X "$m" "$GATEWAY$p" -H @- "$@" || echo 000
}
# expect <what> <wanted status> <got status>
expect() { [[ "$3" == "$2" ]] || die "$1: $3 (wanted $2) $(head -c 400 "$tmp/sbody")"; }
# nokey <path>: a GET with no credential. Prints the status; the body goes to $tmp/sbody.
nokey() { curl -sS --max-time 60 -o "$tmp/sbody" -w '%{http_code}' "$GATEWAY$1" || echo 000; }
sha() { sha256sum "$1" | cut -d' ' -f1; }

# The AWS CLI is in the Neon image; it lists a bucket as the `storage` identity. Path-style
# addressing, as Chelabase uses.
neon_image="$(docker inspect -f '{{.Config.Image}}' "$project-safekeeper1-1")" || die "safekeeper1 isn't running (its image has the AWS CLI)"
# s3_keys <endpoint> <bucket> <prefix>: the keys under the prefix, one per line (none: empty).
s3_keys() {
  docker run --rm --network "$net" \
    -e "AWS_ACCESS_KEY_ID=$(cat "$state/storage-s3/access-key")" -e "AWS_SECRET_ACCESS_KEY=$(cat "$state/storage-s3/secret-key")" \
    -e AWS_DEFAULT_REGION=us-east-1 -e AWS_CONFIG_FILE=/tmp/aws-config --entrypoint /bin/sh "$neon_image" -c '
      printf "[default]\ns3 =\n    addressing_style = path\n" >/tmp/aws-config
      exec aws --endpoint-url "$0" s3api list-objects-v2 --bucket "$1" --prefix "$2" --output json' "$1" "$2" "$3" \
    >"$tmp/s3.json" 2>"$tmp/s3.err" || die "listing s3://$2/$3 at $1: $(cat "$tmp/s3.err")"
  if [[ -s "$tmp/s3.json" ]]; then jq -r '.Contents[]?.Key' "$tmp/s3.json"; fi
}

# --- 1. a database, its storage policies and a key ------------------------------------------------

name="e2e_$(hex 2)"
status="$(api POST /v1/databases "$(jq -nc --arg t "$DEMO_TENANT_ID" --arg p "$DEMO_PROJECT_ID" --arg n "$name" \
  '{tenant_id: $t, project_id: $p, name: $n}')")"
[[ "$status" == 202 || "$status" == 201 ]] || die "POST /v1/databases: $status $(cat "$tmp/body")"
db_id="$(jq -r '.database_id' "$tmp/body")"
[[ -n "$db_id" && "$db_id" != null ]] || die "POST /v1/databases returned no database_id: $(cat "$tmp/body")"
wait_db
pass "database $name ($db_id) is ready"
# The owner's policies: `authenticated` may see, create and change buckets, and do anything with
# their objects (the guide's "private bucket for the app's server" pattern).
psql_main -c "
  select storage.create_policy('e2e_buckets', 'buckets', 'ALL', array['authenticated']::name[], 'true', 'true');
  select storage.create_policy('e2e_objects', 'objects', 'ALL', array['authenticated']::name[], 'true', 'true');" >/dev/null
MAIN_KEY="$(new_key "$db_id" storage-e2e)"
[[ "$MAIN_KEY" == chk_sec_* ]] || die "no secret key: $MAIN_KEY"
pass "storage policies created by the owner; a secret key on main"

# --- 2. a bucket, uploads and downloads -------------------------------------------------------------

expect "create bucket files" 201 "$(st "$MAIN_KEY" POST /storage/v1/bucket -H 'content-type: application/json' -d '{"name":"files"}')"
jq -e '.name == "files" and .public == false and .write_once == false' "$tmp/sbody" >/dev/null || die "the bucket: $(cat "$tmp/sbody")"
expect "upload hello.txt" 200 "$(st "$MAIN_KEY" POST /storage/v1/object/files/docs/hello.txt -H 'content-type: text/plain' --data-binary 'hello storage')"
jq -e '.size == 13 and (.version_id | type) == "string"' "$tmp/sbody" >/dev/null || die "the upload's answer: $(cat "$tmp/sbody")"
expect "download hello.txt" 200 "$(st "$MAIN_KEY" GET /storage/v1/object/files/docs/hello.txt)"
[[ "$(cat "$tmp/sbody")" == 'hello storage' ]] || die "hello.txt came back as $(head -c 100 "$tmp/sbody")"
expect "upload hello.txt again without upsert" 409 "$(st "$MAIN_KEY" POST /storage/v1/object/files/docs/hello.txt -H 'content-type: text/plain' --data-binary 'other')"
pass "bucket files; docs/hello.txt uploaded and downloaded; a second upload without upsert is 409"

# A resumable upload: a 5 MiB part and a 1 MiB last part, sent last part first.
head -c $((5 * 1024 * 1024)) /dev/urandom >"$tmp/p1"
head -c $((1024 * 1024 + 17)) /dev/urandom >"$tmp/p2"
cat "$tmp/p1" "$tmp/p2" >"$tmp/big.bin"
expect "start a resumable upload" 200 "$(st "$MAIN_KEY" POST /storage/v1/upload/files/big.bin -H 'content-type: application/octet-stream')"
session="$(jq -r '.id' "$tmp/sbody")"
[[ "$session" =~ ^[0-9a-f-]{36}$ ]] || die "no session id: $(cat "$tmp/sbody")"
expect "part 2" 200 "$(st "$MAIN_KEY" PUT "/storage/v1/upload/session/$session/2" -H 'content-type: application/octet-stream' --data-binary @"$tmp/p2")"
expect "part 1" 200 "$(st "$MAIN_KEY" PUT "/storage/v1/upload/session/$session/1" -H 'content-type: application/octet-stream' --data-binary @"$tmp/p1")"
expect "list the parts" 200 "$(st "$MAIN_KEY" GET "/storage/v1/upload/session/$session")"
expect "complete the upload" 200 "$(st "$MAIN_KEY" POST "/storage/v1/upload/session/$session/complete")"
jq -e --argjson n "$(stat -c %s "$tmp/big.bin")" '.size == $n' "$tmp/sbody" >/dev/null || die "the completed object: $(cat "$tmp/sbody")"
expect "download big.bin" 200 "$(st "$MAIN_KEY" GET /storage/v1/object/files/big.bin)"
[[ "$(sha "$tmp/sbody")" == "$(sha "$tmp/big.bin")" ]] || die "big.bin came back different ($(stat -c %s "$tmp/sbody") bytes)"
pass "a resumable upload in two parts came back byte for byte ($(stat -c %s "$tmp/big.bin") bytes)"

# --- 3. a signed URL, and a public bucket ---------------------------------------------------------

expect "sign hello.txt" 200 "$(st "$MAIN_KEY" POST /storage/v1/object/sign/files/docs/hello.txt -H 'content-type: application/json' -d '{"expires_in":600}')"
signed="$(jq -r '.signed_url // empty' "$tmp/sbody")"
[[ "$signed" == "/storage/v1/object/sign/files/docs/hello.txt?token="* ]] || die "the signed URL: $(cat "$tmp/sbody")"
expect "the signed URL, without a key" 200 "$(nokey "$signed")"
[[ "$(cat "$tmp/sbody")" == 'hello storage' ]] || die "the signed URL served $(head -c 100 "$tmp/sbody")"
expect "the signed URL with a tampered token" 401 "$(nokey "${signed%?}x")"
expect "the object path without a key" 401 "$(nokey /storage/v1/object/files/docs/hello.txt)"
public="/storage/v1/object/public/$db_id/files/docs/hello.txt"
expect "the public path of a private bucket" 404 "$(nokey "$public")"
expect "make files public" 204 "$(st "$MAIN_KEY" PATCH /storage/v1/bucket/files -H 'content-type: application/json' -d '{"public":true}')"
expect "the public path" 200 "$(nokey "$public")"
[[ "$(cat "$tmp/sbody")" == 'hello storage' ]] || die "the public path served $(head -c 100 "$tmp/sbody")"
expect "make files private" 204 "$(st "$MAIN_KEY" PATCH /storage/v1/bucket/files -H 'content-type: application/json' -d '{"public":false}')"
expect "the public path once private again" 404 "$(nokey "$public")"
pass "a signed URL serves without a key (a tampered one is 401); the public path serves only while the bucket is public"

# --- 4. a branch --------------------------------------------------------------------------------------

ok POST "/v1/databases/$db_id/branches" 202 '{"name":"dev"}'
dev_id="$(jq -r '.id' "$tmp/body")"
wait_branch "$dev_id"
DEV_KEY="$(new_key "$dev_id" storage-e2e-dev)"
expect "hello.txt on dev" 200 "$(st "$DEV_KEY" GET /storage/v1/object/files/docs/hello.txt)"
[[ "$(cat "$tmp/sbody")" == 'hello storage' ]] || die "dev served $(head -c 100 "$tmp/sbody")"
expect "big.bin on dev" 200 "$(st "$DEV_KEY" GET /storage/v1/object/files/big.bin)"
[[ "$(sha "$tmp/sbody")" == "$(sha "$tmp/big.bin")" ]] || die "big.bin on dev is different"
expect "the main key on dev's path" 200 "$(st "$MAIN_KEY" GET /storage/v1/object/files/docs/hello.txt)"
expect "upload dev-only.txt on dev" 200 "$(st "$DEV_KEY" POST /storage/v1/object/files/dev-only.txt -H 'content-type: text/plain' --data-binary 'only on dev')"
expect "dev-only.txt on main" 404 "$(st "$MAIN_KEY" GET /storage/v1/object/files/dev-only.txt)"
expect "delete big.bin on dev" 204 "$(st "$DEV_KEY" DELETE /storage/v1/object/files/big.bin)"
expect "big.bin on dev after its delete" 404 "$(st "$DEV_KEY" GET /storage/v1/object/files/big.bin)"
expect "big.bin on main after dev's delete" 200 "$(st "$MAIN_KEY" GET /storage/v1/object/files/big.bin)"
[[ "$(sha "$tmp/sbody")" == "$(sha "$tmp/big.bin")" ]] || die "big.bin on main changed after dev's delete"
pass "branch dev sees main's objects; its upload and its delete never reach main"

# --- 5. a delete on main --------------------------------------------------------------------------------

expect "delete hello.txt on main" 204 "$(st "$MAIN_KEY" DELETE /storage/v1/object/files/docs/hello.txt)"
expect "hello.txt after its delete" 404 "$(st "$MAIN_KEY" GET /storage/v1/object/files/docs/hello.txt)"
expect "its signed URL after the delete" 404 "$(nokey "$signed")"
expect "hello.txt on dev after main's delete" 200 "$(st "$DEV_KEY" GET /storage/v1/object/files/docs/hello.txt)"
pass "a delete on main: the object and its signed URL answer 404; dev keeps its copy"

# --- 6. the blobs in the object store, and the sweep's purge -------------------------------------------

blobs="$(s3_keys "$STORE_ENDPOINT" "$STORE_BUCKET" "$db_id/" | grep -c . || true)"
# hello.txt, big.bin and dev-only.txt: three versions, each kept while a branch references it.
((blobs >= 3)) || die "s3://$STORE_BUCKET/$db_id/ at $STORE_ENDPOINT holds $blobs blob(s), wanted at least 3"
if [[ "$backend" == s3 ]]; then
  stray="$(s3_keys http://seaweedfs:8333 chelabase-storage "$db_id/" | grep -c . || true)"
  [[ "$stray" == 0 ]] || die "the stack's own SeaweedFS holds $stray blob(s) of $db_id: the swap didn't take"
  pass "$blobs blobs under $db_id/ on the second endpoint, none on the stack's SeaweedFS"
else
  pass "$blobs blobs under s3://$STORE_BUCKET/$db_id/"
fi
ok DELETE "/v1/databases/$db_id/branches/$dev_id" 202
for i in $(seq 120); do
  s="$(branch_field "$dev_id" status)"
  [[ -z "$s" || "$s" == deleted ]] && break
  ((i < 120)) || die "branch dev is still '$s' 120 s after its delete"
  sleep 1
done
ok DELETE "/v1/databases/$db_id" 202
for i in $(seq 120); do
  [[ "$(api GET "/v1/databases/$db_id")" != 404 ]] || break
  ((i < 120)) || die "the database still answers 120 s after its delete"
  sleep 1
done
deleted="$db_id"
db_id=""
left=""
for _ in $(seq 120); do
  left="$( (s3_keys "$STORE_ENDPOINT" "$STORE_BUCKET" "$deleted/"; s3_keys "$STORE_ENDPOINT" "$STORE_BUCKET" "_sweep/$deleted/"; s3_keys "$STORE_ENDPOINT" "$STORE_BUCKET" "_sweep/purge/$deleted") | grep -c . || true)"
  [[ "$left" != 0 ]] || break
  sleep 1
done
[[ "$left" == 0 ]] || die "the sweep left $left object(s) of $deleted in s3://$STORE_BUCKET 120 s after the database's delete"
pass "the database's delete purged its blobs, marks and purge marker from s3://$STORE_BUCKET"
echo "all passed, backend $backend ($((SECONDS - t0)) s)"
