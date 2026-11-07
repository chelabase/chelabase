#!/usr/bin/env bash
# Epic object-storage m2: the live check of SeaweedFS's bucket boundary. One SeaweedFS holds
# bucket `neon` (pageserver layers, safekeeper WAL) and bucket `chelabase-storage` (user files);
# identity `neon` may use only the first, `storage` only the second; there is no admin identity.
# Runs in the stack's runner (`just storage-iam-e2e`); `shared-state` for the
# live pass: it only reads and writes keys under `iam-probe/` and removes them again.
#
# SeaweedFS's bucket-scoped actions (`Read:<bucket>`) are what the design relies on. If a check
# in 2-3 fails, SeaweedFS does not enforce them for that call, and the milestone's fallback applies
# (a second `seaweedfs-storage` container with its own volume and identity; same secret paths).
#
# 1. Both buckets exist (each identity's ListBuckets shows its own); the healthcheck is healthy.
# 2. Each identity works in its own bucket and is denied in the other (list, put, get, delete;
#    path-style only: Chelabase uses it and SeaweedFS has no -s3.domainName here), and ListBuckets
#    shows only its own bucket.
# 3. CopyObject and UploadPartCopy with the other bucket's key as the source are denied.
# 4. No admin identity: the old admin dev key pair gets 403 on 8333.
# 5. Master (9333), volume (8080) and filer (8888) HTTP are refused to a container on the network;
#    only 8333 answers. Every gRPC port needs the stack's client certificate: from the network,
#    S3's (18333) fails the TLS handshake without one (curl, TLS 1.3 reports the server's
#    "certificate required" alert on first read), refuses plain-text HTTP/2 (curl
#    --http2-prior-knowledge), and answers with the client certificate (the control that the port
#    is live); the master, volume and filer gRPC ports (19333, 18080, 18888) are on loopback, so
#    they are refused from the network, and a probe sharing the container's network namespace
#    gets the same handshake failure without a certificate and an answer with one.
# 6. No storage key is in `docker inspect` of any container of the stack.
#
# Collects every failure, then exits 1 if there was one.
set -euo pipefail

LOG_PREFIX=storage-iam-e2e
project="${CHELABASE_COMPOSE_PROJECT:?set by chelabase workbench (stack.env)}"
state="${CHELABASE_STACK_DIR:?set by chelabase workbench (stack.env)}"
net="${project}_default"
endpoint=http://seaweedfs:8333
failures=0
say() { echo "[$LOG_PREFIX] $*"; }
fail() {
  echo "[$LOG_PREFIX] FAIL: $*" >&2
  failures=$((failures + 1))
}
ok() { say "ok   $*"; }

# The aws CLI is in the Neon image (the same one the safekeepers run).
neon_container="$(docker ps -q --filter "label=com.docker.compose.project=$project" \
  --filter "label=com.docker.compose.service=safekeeper1" | head -n1)"
[[ -n "$neon_container" ]] || { echo "[$LOG_PREFIX] FAIL: safekeeper1 isn't running" >&2; exit 1; }
image="$(docker inspect -f '{{.Config.Image}}' "$neon_container")"

key() { cat "$state/$1/$2"; } # key <dir> <access-key|secret-key>
declare -A AK SK
for id in neon:s3 storage:storage-s3; do
  AK[${id%%:*}]="$(key "${id#*:}" access-key)"
  SK[${id%%:*}]="$(key "${id#*:}" secret-key)"
done

# aws_as <identity> <style> <aws args…>; stdin is passed through. Path-style addressing only
# (what Chelabase uses; SeaweedFS has no -s3.domainName here, so virtual-host names are no buckets).
aws_as() {
  local id="$1" style="$2"
  shift 2
  docker run --rm -i --network "$net" \
    -e "AWS_ACCESS_KEY_ID=${AK[$id]}" -e "AWS_SECRET_ACCESS_KEY=${SK[$id]}" -e AWS_DEFAULT_REGION=us-east-1 \
    -e "AWS_CONFIG_FILE=/tmp/aws-config" --entrypoint /bin/sh "$image" -c '
      printf "[default]\ns3 =\n    addressing_style = %s\n" "$1" >/tmp/aws-config; shift
      exec aws --endpoint-url "$0" "$@"' "$endpoint" "$style" "$@" 2>&1
}
# expect_ok / expect_denied <description> <identity> <style> <aws args…>
expect_ok() {
  local what="$1"
  shift
  if out="$(aws_as "$@" </dev/null)"; then ok "$what"; else fail "$what was refused: $out"; fi
}
expect_denied() {
  local what="$1"
  shift
  if out="$(aws_as "$@" </dev/null)"; then fail "$what was allowed: $out"; else ok "$what is denied"; fi
}

# --- 1. both buckets exist, seaweedfs is healthy ------------------------------------------------
sw_container="$(docker ps -q --filter "label=com.docker.compose.project=$project" \
  --filter "label=com.docker.compose.service=seaweedfs" | head -n1)"
[[ "$(docker inspect -f '{{.State.Health.Status}}' "$sw_container")" == healthy ]] &&
  ok "seaweedfs is healthy" || fail "seaweedfs isn't healthy"
docker exec "$sw_container" test -e /tmp/buckets-ready && ok "the bucket step finished" || fail "no /tmp/buckets-ready"
unsigned="$(docker exec "$sw_container" sh -c "printf 'GET / HTTP/1.0\\r\\n\\r\\n' | nc -w 3 127.0.0.1 8333")"
grep -q '<Code>AccessDenied</Code>' <<<"$unsigned" && ok "an unsigned S3 request gets SeaweedFS's error document" ||
  fail "an unsigned S3 request got no AccessDenied document: $unsigned"
for pair in neon:neon storage:chelabase-storage; do
  names="$(aws_as "${pair%%:*}" path s3api list-buckets --query 'Buckets[].Name' --output text </dev/null || true)"
  [[ "$names" == "${pair#*:}" ]] && ok "bucket ${pair#*:} exists (seen by ${pair%%:*})" || fail "${pair%%:*} sees [$names], want [${pair#*:}]"
done

# --- 2. each identity: its bucket works, the other is denied -----------------------------------
probe="iam-probe/$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
cleanup() {
  aws_as neon path s3 rm "s3://neon/$probe" </dev/null >/dev/null 2>&1 || true
  aws_as storage path s3 rm "s3://chelabase-storage/$probe" </dev/null >/dev/null 2>&1 || true
}
trap cleanup EXIT
for pair in neon:neon:chelabase-storage storage:chelabase-storage:neon; do
  id="${pair%%:*}" rest="${pair#*:}" own="${rest%%:*}" other="${rest#*:}"
  for style in path; do
    echo "hello $id" | aws_as "$id" "$style" s3 cp - "s3://$own/$probe" >/dev/null &&
      ok "$id ($style) writes s3://$own" || fail "$id ($style) can't write its own bucket $own"
    expect_ok "$id ($style) lists s3://$own" "$id" "$style" s3 ls "s3://$own/iam-probe/"
    expect_ok "$id ($style) reads s3://$own" "$id" "$style" s3 cp "s3://$own/$probe" -
    expect_denied "$id ($style) lists s3://$other" "$id" "$style" s3 ls "s3://$other/"
    expect_denied "$id ($style) reads s3://$other" "$id" "$style" s3 cp "s3://$other/$probe" -
    if echo "intruder" | aws_as "$id" "$style" s3 cp - "s3://$other/$probe-$id" >/dev/null 2>&1; then
      fail "$id ($style) wrote to s3://$other"
      aws_as "$([[ $other == neon ]] && echo neon || echo storage)" path s3 rm "s3://$other/$probe-$id" </dev/null >/dev/null 2>&1 || true
    else
      ok "$id ($style) writing to s3://$other is denied"
    fi
    expect_denied "$id ($style) deletes in s3://$other" "$id" "$style" s3 rm "s3://$other/$probe"
  done
  names="$(aws_as "$id" path s3api list-buckets --query 'Buckets[].Name' --output text </dev/null || true)"
  [[ "$names" == "$own" ]] && ok "$id's ListBuckets shows only $own" || fail "$id's ListBuckets shows [$names], want [$own]"
  expect_denied "$id creates a bucket" "$id" path s3 mb "s3://$id-iam-probe-bucket"
done

# --- 3. copies from the other bucket -----------------------------------------------------------
for pair in neon:chelabase-storage storage:neon; do
  id="${pair%%:*}" other="${pair#*:}"
  own=neon
  [[ "$id" == storage ]] && own=chelabase-storage
  # A source object in the other bucket, put there by the identity that owns it.
  echo "secret of $other" | aws_as "$([[ $other == neon ]] && echo neon || echo storage)" path s3 cp - "s3://$other/$probe-src" >/dev/null ||
    fail "setup: could not write the copy source in $other"
  for style in path; do
    expect_denied "$id ($style) CopyObject from $other" "$id" "$style" \
      s3api copy-object --bucket "$own" --key "$probe-copy" --copy-source "$other/$probe-src"
    upload_id="$(aws_as "$id" "$style" s3api create-multipart-upload --bucket "$own" --key "$probe-mpu" \
      --query UploadId --output text </dev/null || true)"
    if [[ -n "$upload_id" && "$upload_id" != *rror* ]]; then
      expect_denied "$id ($style) UploadPartCopy from $other" "$id" "$style" \
        s3api upload-part-copy --bucket "$own" --key "$probe-mpu" --part-number 1 --upload-id "$upload_id" \
        --copy-source "$other/$probe-src"
      aws_as "$id" "$style" s3api abort-multipart-upload --bucket "$own" --key "$probe-mpu" \
        --upload-id "$upload_id" </dev/null >/dev/null 2>&1 || true
    else
      fail "$id ($style) could not start a multipart upload in its own bucket $own: $upload_id"
    fi
  done
  aws_as "$([[ $other == neon ]] && echo neon || echo storage)" path s3 rm "s3://$other/$probe-src" </dev/null >/dev/null 2>&1 || true
  aws_as "$id" path s3 rm "s3://$own/$probe-copy" </dev/null >/dev/null 2>&1 || true
done

# --- 4. no admin identity ----------------------------------------------------------------------
# The admin dev pair of an earlier build of this milestone, and any admin key file a stack may
# still have from it: neither may be an identity any more.
old_pairs=("chelabase-admin-dev:chelabase-admin-dev-secret")
if [[ -s "$state/s3-admin/access-key" ]]; then
  old_pairs+=("$(key s3-admin access-key):$(key s3-admin secret-key)")
fi
for pair in "${old_pairs[@]}"; do
  code="$(docker run --rm --network "$net" -e "AWS_ACCESS_KEY_ID=${pair%%:*}" -e "AWS_SECRET_ACCESS_KEY=${pair#*:}" \
    -e AWS_DEFAULT_REGION=us-east-1 --entrypoint /bin/sh "$image" -c \
    "aws --endpoint-url $endpoint s3 ls 2>&1 | grep -oE 'InvalidAccessKeyId|AccessDenied|SignatureDoesNotMatch' | head -1" || true)"
  [[ -n "$code" ]] && ok "an old admin key is refused ($code)" || fail "an old admin key was not refused"
done

# --- 5. ports: HTTP is refused, gRPC needs the client certificate -------------------------------
probe_c="chelabase-iam-probe-$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
docker run -d --rm --name "$probe_c" --network "$net" -v "$state/grpc-tls:/tls:ro" --entrypoint sleep "$image" 300 >/dev/null
loop_c="chelabase-iam-loop-$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
docker run -d --rm --name "$loop_c" --network "container:$sw_container" -v "$state/grpc-tls:/tls:ro" --entrypoint sleep "$image" 300 >/dev/null
trap 'docker rm -f "$probe_c" "$loop_c" >/dev/null 2>&1 || true; cleanup' EXIT
for port in 9333 8080 8888 8334 19333 18080 18888 8181 9101; do
  if docker exec "$probe_c" timeout 3 bash -c "</dev/tcp/seaweedfs/$port" >/dev/null 2>&1; then
    fail "seaweedfs:$port accepts a connection from the network"
  else
    ok "seaweedfs:$port refuses the network"
  fi
done
docker exec "$probe_c" timeout 3 bash -c "</dev/tcp/seaweedfs/8333" >/dev/null 2>&1 &&
  ok "seaweedfs:8333 (S3) answers" || fail "seaweedfs:8333 (S3) doesn't answer"
code="$(docker exec "$probe_c" curl -sS -o /dev/null -w '%{http_code}' --max-time 5 "$endpoint/neon/" || true)"
[[ "$code" == 403 || "$code" == 401 ]] && ok "an unsigned S3 request gets $code" || fail "an unsigned S3 request gets '$code', want 401/403"
code="$(docker exec "$probe_c" curl -sS -o /dev/null -w '%{http_code}' --max-time 5 -X POST \
  -d 'Action=ListUsers&Version=2010-05-08' "$endpoint/" || true)"
[[ "$code" != 200 ]] && ok "the embedded IAM API doesn't answer (got $code)" || fail "the embedded IAM API answers 200"

# Virtual-host addressing (AC 6): SeaweedFS has no -s3.domainName here, so a `Host: neon.seaweedfs`
# header must not select bucket neon for the storage key. It must neither list neon's contents or
# name the bucket, nor return its object (curl signs the request with the storage key).
for path in "/" "/?list-type=2" "/$probe"; do
  body="$(docker exec "$probe_c" curl -sS --max-time 5 --aws-sigv4 "aws:amz:us-east-1:s3" \
    --user "${AK[storage]}:${SK[storage]}" -H "x-amz-content-sha256: UNSIGNED-PAYLOAD" \
    -H "Host: neon.seaweedfs:8333" -w '\nHTTP %{http_code}' "$endpoint$path" 2>&1 || true)"
  if grep -qE "<Name>neon</Name>|<Key>iam-probe|hello neon|HTTP 200" <<<"$body" && ! grep -q "<Name>chelabase-storage</Name>" <<<"$body"; then
    fail "Host: neon.seaweedfs reached bucket neon with the storage key on $path: $body"
  elif grep -q "<Name>neon</Name>" <<<"$body"; then
    fail "Host: neon.seaweedfs shows bucket neon to the storage key on $path"
  else
    ok "Host: neon.seaweedfs does not reach bucket neon with the storage key ($path)"
  fi
done

# grpc_checks <container> <host> <port>: no cert fails the handshake, plain text fails, the cert works.
grpc_checks() {
  local c="$1" host="$2" port="$3"
  if docker exec "$c" curl -sS -o /dev/null --max-time 5 --cacert /tls/ca.crt "https://$host:$port/" >/dev/null 2>&1; then
    fail "$host:$port completed a TLS exchange without a client certificate"
  else ok "$host:$port fails the TLS handshake without a client certificate"; fi
  if docker exec "$c" curl -sS -o /dev/null --max-time 5 --http2-prior-knowledge "http://$host:$port/" >/dev/null 2>&1; then
    fail "$host:$port answers plain-text gRPC/HTTP2"
  else ok "$host:$port refuses plain-text gRPC/HTTP2"; fi
  code="$(docker exec "$c" curl -sS -o /dev/null -w '%{http_code}' --max-time 5 --cacert /tls/ca.crt \
    --cert /tls/client.crt --key /tls/client.key "https://$host:$port/" 2>/dev/null || true)"
  [[ "$code" =~ ^[1-5][0-9][0-9]$ ]] && ok "$host:$port answers with the client certificate ($code)" ||
    fail "$host:$port doesn't answer with the client certificate (control)"
}
grpc_checks "$probe_c" seaweedfs 18333
for port in 19333 18080 18888 18333; do grpc_checks "$loop_c" 127.0.0.1 "$port"; done

# --- 6. no storage key in docker inspect ----------------------------------------------
all="$(docker ps -aq --filter "label=com.docker.compose.project=$project")"
# shellcheck disable=SC2086
inspect="$(docker inspect $all)"
for id in storage; do
  for v in "${AK[$id]}" "${SK[$id]}"; do
    if grep -qF -- "$v" <<<"$inspect"; then fail "an $id key appears in docker inspect"; else ok "an $id key is not in docker inspect"; fi
  done
done

[[ "$failures" == 0 ]] || { echo "[$LOG_PREFIX] $failures check(s) failed" >&2; exit 1; }
say "all checks passed"
