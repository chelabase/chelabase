#!/usr/bin/env bash
# edge-functions m1: the S3 identity barrier around bucket `chelabase-functions` (function
# bundles). Identity `functions` reads, writes and lists `chelabase-functions` and is refused on
# `chelabase-storage` and `neon`; identities `storage` and `neon` are refused on
# `chelabase-functions`. Runs against this checkout's workbench stack (shared-state: it writes and
# removes keys under `fn-iam-probe/`); seaweedfs must run this branch's compose config.
#
#   infra/functions-runtime/tests/s3-identity.sh     # or: just workbench run <this file>
set -euo pipefail
root="$(cd "$(dirname "$0")/../../.." && pwd)"
if [[ -z "${CHELABASE_COMPOSE_PROJECT:-}" ]]; then
  stack="$(cat "$root/.workbench-stack")"
  main="$(git -C "$root" worktree list --porcelain | awk 'NR == 1 { print $2 }')"
  set -a
  # shellcheck disable=SC1090
  . "$main/.chelabase/stacks/$stack/env/stack.env"
  set +a
fi
project="$CHELABASE_COMPOSE_PROJECT"
state="$CHELABASE_STACK_DIR"
net="${project}_default"
endpoint=http://seaweedfs:8333
fail=0
err() { echo "s3-identity: FAIL: $*" >&2; fail=1; }
ok() { echo "s3-identity: ok   $*"; }

# The aws CLI is in the Neon image (the safekeepers').
sk="$(docker ps -q --filter "label=com.docker.compose.project=$project" \
  --filter "label=com.docker.compose.service=safekeeper1" | head -n1)"
[[ -n "$sk" ]] || { echo "s3-identity: FAIL: safekeeper1 isn't running" >&2; exit 1; }
image="$(docker inspect -f '{{.Config.Image}}' "$sk")"

declare -A AK SK
for id in neon:s3 storage:storage-s3 functions:functions-s3; do
  AK[${id%%:*}]="$(cat "$state/${id#*:}/access-key" 2>/dev/null || true)"
  SK[${id%%:*}]="$(cat "$state/${id#*:}/secret-key" 2>/dev/null || true)"
done
# The workbench's fixed dev pair for `functions` (docker-compose.workbench.yml's configs).
: "${AK[functions]:=chelabase-functions-dev}" "${SK[functions]:=chelabase-functions-dev-secret}"

aws_as() { # aws_as <identity> <aws args…>
  local id="$1"
  shift
  docker run --rm -i --network "$net" \
    -e "AWS_ACCESS_KEY_ID=${AK[$id]}" -e "AWS_SECRET_ACCESS_KEY=${SK[$id]}" -e AWS_DEFAULT_REGION=us-east-1 \
    -e AWS_CONFIG_FILE=/tmp/aws-config --entrypoint /bin/sh "$image" -c '
      printf "[default]\ns3 =\n    addressing_style = path\n" >/tmp/aws-config
      exec aws --endpoint-url "$0" "$@"' "$endpoint" "$@" 2>&1
}
allowed() { local what="$1"; shift; if out="$(aws_as "$@" </dev/null)"; then ok "$what"; else err "$what was refused: $out"; fi; }
denied() { local what="$1"; shift; if out="$(aws_as "$@" </dev/null)"; then err "$what was allowed: $out"; else ok "$what is refused"; fi; }

probe="fn-iam-probe/$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
cleanup() {
  aws_as functions s3 rm "s3://chelabase-functions/$probe" </dev/null >/dev/null 2>&1 || true
  aws_as storage s3 rm "s3://chelabase-storage/$probe" </dev/null >/dev/null 2>&1 || true
  aws_as neon s3 rm "s3://neon/$probe" </dev/null >/dev/null 2>&1 || true
}
trap cleanup EXIT

# functions: its own bucket works.
if echo "bundle" | aws_as functions s3 cp - "s3://chelabase-functions/$probe" >/dev/null; then
  ok "functions writes chelabase-functions"
else
  err "functions can't write chelabase-functions"
fi
allowed "functions reads chelabase-functions" functions s3 cp "s3://chelabase-functions/$probe" -
allowed "functions lists chelabase-functions" functions s3 ls "s3://chelabase-functions/fn-iam-probe/"

# Seed the other buckets with their owners so a read has something to find.
echo "storage object" | aws_as storage s3 cp - "s3://chelabase-storage/$probe" >/dev/null || err "setup: storage write"
echo "neon object" | aws_as neon s3 cp - "s3://neon/$probe" >/dev/null || err "setup: neon write"

for other in chelabase-storage neon; do
  denied "functions lists $other" functions s3 ls "s3://$other/"
  denied "functions reads $other" functions s3 cp "s3://$other/$probe" -
  if echo "intruder" | aws_as functions s3 cp - "s3://$other/$probe-fn" >/dev/null 2>&1; then
    err "functions wrote to $other"
  else
    ok "functions writing to $other is refused"
  fi
  denied "functions deletes in $other" functions s3 rm "s3://$other/$probe"
done

for id in storage neon; do
  denied "$id lists chelabase-functions" "$id" s3 ls "s3://chelabase-functions/"
  denied "$id reads chelabase-functions" "$id" s3 cp "s3://chelabase-functions/$probe" -
  if echo "intruder" | aws_as "$id" s3 cp - "s3://chelabase-functions/$probe-$id" >/dev/null 2>&1; then
    err "$id wrote to chelabase-functions"
  else
    ok "$id writing to chelabase-functions is refused"
  fi
  denied "$id deletes in chelabase-functions" "$id" s3 rm "s3://chelabase-functions/$probe"
done

names="$(aws_as functions s3api list-buckets --query 'Buckets[].Name' --output text </dev/null || true)"
[[ "$names" == chelabase-functions ]] && ok "functions' ListBuckets shows only chelabase-functions" ||
  err "functions sees [$names]"

[[ $fail -eq 0 ]] && echo "s3-identity: ok"
exit $fail
