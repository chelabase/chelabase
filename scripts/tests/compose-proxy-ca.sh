#!/usr/bin/env bash
# Offline check of the proxy's compute CA (epic compute-hop-tls m1, #224). The proxy reaches
# every compute over TLS and verifies its certificate against the compute CA, which it loads
# from NEON_INTERNAL_CA_FILE. The proxy itself accepts an unset variable (it then trusts only
# the system roots, and every compute login fails), so the compose entrypoint refuses to start
# without a CA.
#
# Checks, in the base file (.env.example) and base + workbench overlay (fixture stack.env):
#   1. the proxy sets NEON_INTERNAL_CA_FILE, and mounts the compute CA's certificate there,
#      read-only (base: the `secrets` volume's compute-ca/ca.crt; workbench: the keys dir's);
#   2. the rendered entrypoint, run with a stub `proxy`, refuses to exec it when the variable
#      is unset or empty, or names a missing, empty or certificate-less file, and execs it
#      (with the command's arguments) when the file holds a certificate.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
dir="$root/infra/compose"
fail=0
err() {
  echo "compose-proxy-ca: $*" >&2
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
compose chelabase-proxy-ca "$dir/.env.example" -f "$dir/docker-compose.yml" >"$tmp/base.json" ||
  { echo "compose-proxy-ca: rendering the base file failed" >&2; exit 1; }
compose chelabase-feat-x "$root/scripts/tests/fixtures/workbench-stack.env" \
  -f "$dir/docker-compose.yml" -f "$dir/docker-compose.workbench.yml" \
  | jq -f "$root/scripts/tests/fixtures/secrets-as-binds.jq" >"$tmp/wb.json" ||
  { echo "compose-proxy-ca: rendering base + overlay failed" >&2; exit 1; }
keys="$(sed -n 's/^CHELABASE_KEYS_DIR=//p' "$root/scripts/tests/fixtures/workbench-stack.env")"

# 1. The variable and its mount.
for render in base wb; do
  file="$tmp/$render.json"
  ca="$(jq -r '.services.proxy.environment.NEON_INTERNAL_CA_FILE // ""' "$file")"
  if [[ -z "$ca" ]]; then
    err "$render: the proxy doesn't set NEON_INTERNAL_CA_FILE"
    continue
  fi
  if [[ "$render" == base ]]; then
    m="$(jq -c --arg t "$ca" '[.services.proxy.volumes[]? | select(.target == $t)]' "$file")"
    jq -e 'length == 1 and .[0].type == "volume" and .[0].source == "secrets"
      and .[0].volume.subpath == "compute-ca/ca.crt" and .[0].read_only == true' <<<"$m" >/dev/null ||
      err "base: NEON_INTERNAL_CA_FILE ($ca) is not a read-only mount of secrets:compute-ca/ca.crt: $m"
  else
    m="$(jq -c --arg t "$ca" '[.services.proxy.volumes[]? | select(.target == $t)]' "$file")"
    jq -e --arg s "$keys/compute-ca/ca.crt" 'length == 1 and .[0].type == "bind"
      and .[0].source == $s and .[0].read_only == true' <<<"$m" >/dev/null ||
      err "wb: NEON_INTERNAL_CA_FILE ($ca) is not a read-only mount of $keys/compute-ca/ca.crt: $m"
  fi
done

# 2. The entrypoint, with a stub proxy. Both renders run the base's entrypoint; check each.
mkdir -p "$tmp/bin"
cat >"$tmp/bin/proxy" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >"$tmp/ran"
EOF
chmod +x "$tmp/bin/proxy"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout "$tmp/ca.key" \
  -out "$tmp/ca.crt" -days 1 -subj "/CN=compose-proxy-ca test" 2>/dev/null
: >"$tmp/empty.crt"
printf 'not a certificate\n' >"$tmp/junk.crt"

run() { # run <render> <NEON_INTERNAL_CA_FILE value, or "-" for unset>; exit status
  local file="$tmp/$1.json" script
  # `config` keeps Compose's `$$` escape; the container gets `$`.
  script="$(jq -r '.services.proxy.entrypoint | if (.[0] == "/bin/sh" and .[1] == "-c") then .[2] | gsub("\\$\\$"; "$") else error("not sh -c") end' "$file")" ||
    { err "$1: the proxy's entrypoint is not /bin/sh -c <script>"; return 99; }
  rm -f "$tmp/ran"
  local envs=(PATH="$tmp/bin:/usr/bin:/bin")
  [[ "$2" != "-" ]] && envs+=(NEON_INTERNAL_CA_FILE="$2")
  env -i "${envs[@]}" /bin/sh -c "$script" proxy --auth-backend=control-plane >/dev/null 2>"$tmp/stderr"
}
for render in base wb; do
  for case in "-" "" "$tmp/missing.crt" "$tmp/empty.crt" "$tmp/junk.crt" "$tmp"; do
    if run "$render" "$case"; then
      err "$render: the entrypoint started with NEON_INTERNAL_CA_FILE=[$case]"
    elif [[ -e "$tmp/ran" ]]; then
      err "$render: the entrypoint ran the proxy with NEON_INTERNAL_CA_FILE=[$case]"
    elif ! grep -q NEON_INTERNAL_CA_FILE "$tmp/stderr"; then
      err "$render: refusing NEON_INTERNAL_CA_FILE=[$case] doesn't name the variable: $(cat "$tmp/stderr")"
    fi
  done
  if ! run "$render" "$tmp/ca.crt"; then
    err "$render: the entrypoint refused a valid CA file: $(cat "$tmp/stderr")"
  elif [[ ! -e "$tmp/ran" ]] || ! grep -q -- '--auth-backend=control-plane' "$tmp/ran"; then
    err "$render: the entrypoint didn't exec the proxy with its arguments"
  fi
done

if [[ "$fail" == 0 ]]; then
  echo "compose-proxy-ca: ok"
fi
exit "$fail"
