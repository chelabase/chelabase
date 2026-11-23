#!/usr/bin/env bash
# Offline check that computes have no route off their networks, by construction (epic
# compute-network-reach m1). From `docker compose config` with every profile, of:
#   - the self-hosted install (docker-compose.yml with .env.example),
#   - the example overlay (docker-compose.yml + docker-compose.example.yml with .env.example),
#   - the workbench overlay (docker-compose.yml + docker-compose.workbench.yml) with the fixture
#     stack.env (control slot 1), and with the slot formula's addresses for slots 0 and 63;
# it checks:
#   1. the networks `compute` and `control` are gone; `compute-internal` and `control-internal`
#      are `internal: true`, both bridge gateway modes are `isolated` (the bridge has no host
#      address), their Docker names are `<project>_compute-internal` and
#      `<project>_control-internal`, and each has a configured subnet. The two subnets don't
#      overlap each other or the legacy default 10.214.0.0/22 (Docker refuses an overlap while
#      the legacy network exists).
#   2. every service on one of those two networks is also on a network that isn't internal,
#      unless it is listed in `only_internal` below with its reason;
#   3. on each of the two networks the Docker-assigned `ip_range` and every static address are
#      inside the subnet and disjoint;
#   4. no service publishes a port while joining only internal networks, and the internal
#      networks are exactly `internal_expected`: a new network must be classified here.
# The egress service (compute-network-reach m4), in the same renders:
#   5. `compute-egress` is on exactly `compute-internal` (at a pinned address inside the subnet,
#      outside the Docker-assigned `ip_range`, and not the network, gateway or broadcast address) and
#      `compute-egress`, a network that isn't internal; no other service is on `compute-egress` or
#      `functions-egress`, and `functions-runtime` is the only member of `functions-egress`;
#   6. it is hardened and bare: a non-root numeric `user`, `read_only`, `cap_drop: [ALL]`, no
#      `cap_add`, `no-new-privileges`, a memory and a pid limit, a descriptor limit that covers
#      its default connection limit, no `ports`, `secrets`, `privileged`, host namespace, device or
#      Docker socket, no `entrypoint`, a `command` that is only its binary, exactly three sysctls (the
#      unprivileged port start at 0, and no IPv4 forwarding: `ip_forward`, `conf.all.forwarding`;
#      IPv6 is off on both networks instead, `enable_ipv6: false`), `no-new-privileges:true` as the
#      only such option, a non-root group as well as user, a health check that runs
#      `compute-egress health`, and its volumes are none (the workbench: its own binary, read-only);
#   7. it runs only behind the profile `compute-egress`, so a render without profiles has no such
#      service; its settings (allow-list, listen address, the stack's subnets, health port and dial
#      timeout) are wired, and the worker's `CHELABASE_COMPUTE_EGRESS_DNS` is its address.
# Each check also runs against a fixture service that breaks it (a capability, root, a volume, a
# second member of an egress network, …), which must fail.
# The workbench slot formula below mirrors tools/cli/src/workbench/env.rs (`control_net`,
# `compute_net`), whose unit tests pin it.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
dir="$root/infra/compose"
fixture="$root/scripts/tests/fixtures/workbench-stack.env"
fail() {
  echo "compose-compute-network-internal: $*" >&2
  exit 1
}

# The networks that are internal on purpose (no route out); every other network must not be.
# `mcp-sandbox` (mcp-core m8): `mcp` and its runtime only (compose-secret-boundary.sh check 9).
internal_expected="compute-internal control-internal functions functions-control mcp-sandbox services-control"
# Services allowed on `compute-internal` or `control-internal` with only internal networks, and
# why. Empty today: every member is also on `default` (or its own outbound network).
declare -A only_internal=()
legacy_default="10.214.0.0/22"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Every profile. The workbench's `self-hosted` profile is never activated (it holds the base's
# one-shots, switched off there), so its renders name the profiles it uses instead.
profiles=(--profile '*')
render() { # render <project> <env file> <-f args…>
  local project="$1" env_file="$2"
  shift 2
  NEON_IMAGE=neon-storage:placeholder COMPUTE_NODE_IMAGE=neon-compute:placeholder \
    docker compose -p "$project" --project-directory "$dir" "$@" --env-file "$env_file" \
    "${profiles[@]}" config --format json --no-env-resolution
}

# --- IPv4 arithmetic ---------------------------------------------------------------------------
ip2int() {
  local a b c d
  IFS=. read -r a b c d <<<"$1"
  echo $(((a << 24) | (b << 16) | (c << 8) | d))
}
# range <cidr>: "<first> <last>" as integers
range() {
  local addr="${1%/*}" prefix="${1#*/}" n mask
  [[ "$1" == */* && "$prefix" =~ ^[0-9]+$ && "$prefix" -le 32 ]] || fail "not an IPv4 CIDR: $1"
  n="$(ip2int "$addr")"
  mask=$(((0xFFFFFFFF << (32 - prefix)) & 0xFFFFFFFF))
  echo "$((n & mask)) $(((n & mask) | (~mask & 0xFFFFFFFF)))"
}
overlaps() { # overlaps <cidr> <cidr>
  local a1 a2 b1 b2
  read -r a1 a2 <<<"$(range "$1")"
  read -r b1 b2 <<<"$(range "$2")"
  ((a1 <= b2 && b1 <= a2))
}
inside() { # inside <inner cidr> <outer cidr>
  local a1 a2 b1 b2
  read -r a1 a2 <<<"$(range "$1")"
  read -r b1 b2 <<<"$(range "$2")"
  ((a1 >= b1 && a2 <= b2))
}

check() { # check <label> <project> <rendered json file>
  local label="$1" project="$2" json="$3" q net subnet ip_range addr s svc
  q() { jq -r "$1" "$json"; }

  # 1. the two networks
  for net in compute control; do
    [[ "$(q ".networks | has(\"$net\")")" == false ]] ||
      fail "$label: the network \`$net\` still exists (it is \`$net-internal\` now)"
  done
  declare -A subnets=()
  for net in compute-internal control-internal; do
    [[ "$(q ".networks | has(\"$net\")")" == true ]] || fail "$label: no network \`$net\`"
    [[ "$(q ".networks[\"$net\"].internal")" == true ]] || fail "$label: \`$net\` isn't internal"
    for family in ipv4 ipv6; do
      [[ "$(q ".networks[\"$net\"].driver_opts[\"com.docker.network.bridge.gateway_mode_$family\"] // empty")" == isolated ]] ||
        fail "$label: \`$net\` lacks com.docker.network.bridge.gateway_mode_$family=isolated"
    done
    [[ "$(q ".networks[\"$net\"].name")" == "${project}_$net" ]] ||
      fail "$label: \`$net\` is named $(q ".networks[\"$net\"].name"), not ${project}_$net"
    [[ "$(q ".networks[\"$net\"].ipam.config | length")" == 1 ]] ||
      fail "$label: \`$net\` needs exactly one configured subnet"
    subnet="$(q ".networks[\"$net\"].ipam.config[0].subnet // empty")"
    [[ -n "$subnet" ]] || fail "$label: \`$net\` has no configured subnet"
    ! overlaps "$subnet" "$legacy_default" ||
      fail "$label: \`$net\` ($subnet) overlaps the legacy control default $legacy_default"
    subnets[$net]="$subnet"
  done
  ! overlaps "${subnets[compute-internal]}" "${subnets[control-internal]}" ||
    fail "$label: the compute (${subnets[compute-internal]}) and control (${subnets[control-internal]}) subnets overlap"

  # 4. the set of internal networks, and no published port on only internal networks
  local internal
  internal="$(q '[.networks | to_entries[] | select(.value.internal == true) | .key] | sort | join(" ")')"
  [[ "$internal" == "$internal_expected" ]] ||
    fail "$label: the internal networks are \"$internal\", expected \"$internal_expected\" (classify a new network here)"
  local is_internal='(.key as $n | $internal | index($n)) != null'
  local svc_line
  while IFS=$'\t' read -r svc svc_line; do
    [[ -n "$svc" ]] || continue
    fail "$label: $svc publishes a port but joins only internal networks ($svc_line)"
  done < <(jq -r --arg internal "$internal_expected" '
    ($internal | split(" ")) as $internal
    | .services | to_entries[]
    | select((.value.ports // []) | length > 0)
    | select((.value.networks // {}) | length > 0)
    | select([(.value.networks // {}) | to_entries[] | '"$is_internal"'] | all)
    | "\(.key)\t\(.value.networks | keys | join(","))"' "$json")

  # 2. every member of the two networks also has a network that isn't internal
  while IFS=$'\t' read -r svc svc_line; do
    [[ -n "$svc" ]] || continue
    [[ -n "${only_internal[$svc]:-}" ]] && continue
    fail "$label: $svc is on $svc_line, all internal; add a non-internal network or list it in only_internal with its reason"
  done < <(jq -r --arg internal "$internal_expected" '
    ($internal | split(" ")) as $internal
    | .services | to_entries[]
    | select((.value.networks // {}) | has("compute-internal") or has("control-internal"))
    | select([.value.networks | to_entries[] | '"$is_internal"'] | all)
    | "\(.key)\t\(.value.networks | keys | join(","))"' "$json")

  # 3. dynamic range and static addresses
  for net in compute-internal control-internal; do
    subnet="${subnets[$net]}"
    ip_range="$(q ".networks[\"$net\"].ipam.config[0].ip_range // empty")"
    [[ -n "$ip_range" ]] || fail "$label: \`$net\` has no ip_range for Docker-assigned addresses"
    inside "$ip_range" "$subnet" || fail "$label: \`$net\` ip_range $ip_range isn't inside $subnet"
    while IFS=$'\t' read -r s addr; do
      [[ -n "$s" ]] || continue
      inside "$addr/32" "$subnet" || fail "$label: $s's static address $addr on \`$net\` isn't inside $subnet"
      ! overlaps "$addr/32" "$ip_range" ||
        fail "$label: $s's static address $addr on \`$net\` is inside the Docker-assigned range $ip_range"
    done < <(q ".services | to_entries[] | select(.value.networks[\"$net\"].ipv4_address? // empty | length > 0)
      | \"\(.key)\t\(.value.networks[\"$net\"].ipv4_address)\"")
  done
  echo "compose-compute-network-internal: ok: $label (compute ${subnets[compute-internal]}, control ${subnets[control-internal]})"
}

# --- compute-egress (m4) -----------------------------------------------------------------------
# egress_check <label> <mode: base|workbench> <rendered json> <compute subnet> <compute ip_range>
# Exits 1 (through `fail`) on the first violation; the fixtures below run it in a subshell.
egress_check() {
  local label="$1" mode="$2" json="$3" subnet="$4" ip_range="$5" q addr first_host net_addr svc got
  q() { jq -r "$1" "$json"; }
  local e='.services["compute-egress"]'
  [[ "$(q "$e | type")" == object ]] || fail "$label: no compute-egress service"

  # 5. no service shares another's network, pid or ipc namespace, and no two networks are one
  # Docker network (a service in the namespace of compute-egress gets both its networks, its
  # route out and its listeners without being named on either network)
  got="$(q '[.services | to_entries[] | select([.value.network_mode, .value.pid, .value.ipc] | map(select(type == "string" and test("^(service:(compute-egress|functions-runtime)$|container:)"))) | length > 0) | .key] | sort | join(" ")')"
  [[ -z "$got" ]] || fail "$label: [$got] share the namespace (network_mode, pid or ipc) of compute-egress, functions-runtime or a container"
  got="$(q '[.services | to_entries[] | select((.value.network_mode // "none") != "none") | .key] | sort | join(" ")')"
  [[ -z "$got" ]] || fail "$label: [$got] set a network_mode other than none"
  [[ "$(q '[.networks | to_entries[] | (.value.name // .key)] | (length == (unique | length))')" == true ]] ||
    fail "$label: two networks render to the same Docker name ($(q '[.networks | to_entries[] | (.value.name // .key)] | group_by(.) | map(select(length > 1) | .[0]) | join(" ")'))"
  for net in compute-egress compute-internal; do
    [[ "$(q ".networks[\"$net\"].enable_ipv6 | tostring")" == false ]] ||
      fail "$label: the \`$net\` network doesn't set enable_ipv6: false (it is $(q ".networks[\"$net\"].enable_ipv6 | tostring"))"
  done

  # 5. networks and address
  [[ "$(q "($e.networks // {}) | keys | join(\" \")")" == "compute-egress compute-internal" ]] ||
    fail "$label: compute-egress is on [$(q "($e.networks // {}) | keys | join(\" \")")], not exactly compute-egress and compute-internal"
  addr="$(q "$e.networks[\"compute-internal\"].ipv4_address // empty")"
  [[ -n "$addr" ]] || fail "$label: compute-egress has no pinned address on compute-internal"
  inside "$addr/32" "$subnet" || fail "$label: compute-egress's address $addr isn't inside $subnet"
  ! overlaps "$addr/32" "$ip_range" ||
    fail "$label: compute-egress's address $addr is inside the Docker-assigned range $ip_range (Docker hands out the range's first address)"
  read -r net_addr _ <<<"$(range "$subnet")"
  [[ "$(ip2int "$addr")" -ne "$net_addr" && "$(ip2int "$addr")" -ne $((net_addr + 1)) ]] ||
    fail "$label: compute-egress's address $addr is the network or gateway address of $subnet"
  [[ "$(ip2int "$addr")" -ne "$(range "$subnet" | cut -d' ' -f2)" ]] || fail "$label: compute-egress's address $addr is the broadcast address of $subnet"
  [[ "$(q '.networks["compute-egress"].internal // false')" == false ]] || fail "$label: the compute-egress network is internal"
  [[ "$(q '.networks["compute-egress"].name')" == "$(q .name)_compute-egress" ]] ||
    fail "$label: the compute-egress network is named $(q '.networks["compute-egress"].name')"
  got="$(q '[.services | to_entries[] | select(.value.networks // {} | has("compute-egress")) | .key] | sort | join(" ")')"
  [[ "$got" == "compute-egress" ]] || fail "$label: the compute-egress network holds [$got], only compute-egress may be on it"
  got="$(q '[.services | to_entries[] | select(.value.networks // {} | has("functions-egress")) | .key] | sort | join(" ")')"
  [[ "$got" == "functions-runtime" ]] || fail "$label: the functions-egress network holds [$got], only functions-runtime may be on it"
  [[ "$(q "$e.network_mode // empty")" == "" ]] || fail "$label: compute-egress sets network_mode"
  got="$(q "[.services | to_entries[] | select(.value.networks[\"compute-internal\"].ipv4_address? // empty | length > 0)
    | .value.networks[\"compute-internal\"].ipv4_address] | group_by(.) | map(select(length > 1) | .[0]) | join(\" \")")"
  [[ -z "$got" ]] || fail "$label: more than one service has the static address [$got] on compute-internal"

  # 6. hardening
  local user
  user="$(q "$e.user // empty")"
  [[ "$user" =~ ^([0-9]+)(:([0-9]+))?$ && "${BASH_REMATCH[1]}" -ne 0 ]] || fail "$label: compute-egress's user is \"$user\", not a numeric non-root user"
  [[ "$user" =~ ^[0-9]+:([0-9]+)$ && "${BASH_REMATCH[1]}" -ne 0 ]] || fail "$label: compute-egress's user \"$user\" has no non-root group (a bare uid gets group 0 when the image has no such user)"
  [[ "$(q "$e.read_only")" == true ]] || fail "$label: compute-egress's root filesystem isn't read-only"
  [[ "$(q "($e.cap_drop // []) | join(\",\")")" == ALL ]] || fail "$label: compute-egress doesn't drop exactly ALL capabilities"
  [[ "$(q "$e.cap_add // [] | length")" == 0 ]] || fail "$label: compute-egress adds a capability"
  [[ "$(q "$e.security_opt // [] | index(\"no-new-privileges:true\") != null")" == true ]] || fail "$label: compute-egress lacks no-new-privileges"
  [[ "$(q "$e.security_opt // [] | map(select(startswith(\"no-new-privileges\"))) | join(\",\")")" == "no-new-privileges:true" ]] ||
    fail "$label: compute-egress's security_opt has a no-new-privileges entry besides one no-new-privileges:true (Docker applies the last)"
  [[ "$(q "$e.security_opt // [] | map(select(test(\"unconfined|seccomp|apparmor|label\"))) | length")" == 0 ]] ||
    fail "$label: compute-egress loosens its security profile"
  [[ "$(q "$e.mem_limit // 0 | tonumber > 0")" == true ]] || fail "$label: compute-egress has no memory limit"
  [[ "$(q "(($e.pids_limit // 0) > 0)")" == true ]] || fail "$label: compute-egress has no pid limit"
  [[ "$(q "[$e.ulimits.nofile.soft, $e.ulimits.nofile.hard] | all(. != null and . >= 4096)")" == true ]] ||
    fail "$label: compute-egress's descriptor limit (ulimits.nofile) is missing or below 4096 (two per forwarded connection, 1024 at once)"
  for key in ports secrets privileged pid ipc uts userns_mode cgroup devices device_cgroup_rules volumes_from group_add cap_add \
    tmpfs links external_links; do
    [[ "$(q "$e | (.$key // empty) | if type == \"array\" or type == \"object\" then length else 1 end // 0")" =~ ^(0|)$ ]] ||
      fail "$label: compute-egress sets $key"
  done
  [[ "$(q "$e.privileged // false")" == false ]] || fail "$label: compute-egress is privileged"
  # No entrypoint (a program that takes a program, `env sh -c`, would pass a list of shell names), and
  # a command that is the binary alone.
  [[ "$(q "$e.entrypoint // [] | length")" == 0 ]] || fail "$label: compute-egress sets an entrypoint"
  [[ "$(q "$e.command // [] | if type == \"array\" then (length == 0 or (length == 1 and (.[0] | split(\"/\") | last) == \"compute-egress\")) else false end")" == true ]] ||
    fail "$label: compute-egress's command is [$(q "$e.command // [] | if type == \"array\" then join(\" \") else . end")], not only the compute-egress binary"
  [[ "$(q "($e.sysctls // {}) | to_entries | map(\"\\(.key)=\\(.value)\") | sort | join(\" \")")" == \
    "net.ipv4.conf.all.forwarding=0 net.ipv4.ip_forward=0 net.ipv4.ip_unprivileged_port_start=0" ]] ||
    fail "$label: compute-egress's sysctls are [$(q "$e.sysctls // {} | to_entries | map(\"\\(.key)=\\(.value)\") | sort | join(\" \")")], not exactly the unprivileged port start at 0 and no IPv4 forwarding"
  got="$(q "($e.healthcheck.test // []) | map(split(\"/\") | last) | join(\" \")")"
  [[ "$got" == "CMD compute-egress health" ]] || fail "$label: compute-egress's health check runs [$got], not \`compute-egress health\`"
  # Volumes: none, except the workbench's own binary, read-only, at its own path.
  if [[ "$mode" == workbench ]]; then
    [[ "$(q "[$e.volumes // [] | .[] | select(.type == \"bind\" and (.read_only // false) and (.source | test(\"/target/debug/compute-egress\$\")) and .source == .target)] | length")" == 1 &&
      "$(q "$e.volumes // [] | length")" == 1 ]] ||
      fail "$label: compute-egress's one volume must be its own binary, read-only (got $(q "$e.volumes // [] | map(.source // .target) | join(\",\")"))"
    [[ "$(q "$e.extra_hosts // [] | if type == \"array\" then . else (to_entries | map(\"\\(.key)=\\(.value)\")) end | map(sub(\":\"; \"=\")) | join(\",\")")" == "host.docker.internal=host-gateway" ]] ||
      fail "$label: compute-egress's extra_hosts must be host.docker.internal:host-gateway only (the test allow-list's non-public name)"
  else
    [[ "$(q "$e.volumes // [] | length")" == 0 ]] || fail "$label: compute-egress mounts a volume"
    [[ "$(q "$e.extra_hosts // [] | length")" == 0 ]] || fail "$label: compute-egress sets extra_hosts"
  fi
  [[ "$(q "[$e.volumes // [] | .[] | select((.source // \"\") | contains(\"docker.sock\"))] | length")" == 0 ]] ||
    fail "$label: compute-egress mounts the Docker socket"

  # 7. profile, settings, the worker's resolver
  [[ "$(q "($e.profiles // []) | join(\",\")")" == compute-egress ]] || fail "$label: compute-egress must be in the profile compute-egress only"
  local env='.services["compute-egress"].environment'
  [[ "$(q "$env.CHELABASE_COMPUTE_EGRESS_LISTEN")" == "$addr" ]] || fail "$label: the egress service listens on $(q "$env.CHELABASE_COMPUTE_EGRESS_LISTEN"), not its address $addr"
  [[ "$(q "$env | has(\"CHELABASE_COMPUTE_EGRESS_ALLOW\") and has(\"CHELABASE_COMPUTE_EGRESS_DENY\") and has(\"CHELABASE_COMPUTE_EGRESS_HEALTH_PORT\") and has(\"CHELABASE_COMPUTE_EGRESS_DIAL_TIMEOUT_SECS\")")" == true ]] ||
    fail "$label: the egress service doesn't pass its allow-list, deny list, health port and dial timeout"
  [[ "$(q "$env.CHELABASE_INTERNAL_SUBNETS | split(\",\") | sort | join(\" \")")" == \
    "$(q '[.networks["compute-internal"].ipam.config[0].subnet, .networks["control-internal"].ipam.config[0].subnet] | sort | join(" ")')" ]] ||
    fail "$label: CHELABASE_INTERNAL_SUBNETS must be the compute and control subnets"
  [[ "$(q "$env | keys | map(select(test(\"(_FILE|_KEY|_PASSWORD|_TOKEN|_SECRET|_URL|_URI)\$\"))) | length")" == 0 ]] ||
    fail "$label: compute-egress's environment names a secret"
  [[ "$(q '.services.worker.environment.CHELABASE_COMPUTE_EGRESS_DNS // empty')" == "$addr" ]] ||
    fail "$label: the worker's CHELABASE_COMPUTE_EGRESS_DNS is $(q '.services.worker.environment.CHELABASE_COMPUTE_EGRESS_DNS // "unset"'), not compute-egress's address $addr"
}

# A fixture that breaks one rule must fail the check: egress_breaks <label> <mode> <json> <subnet> <range> <jq filter>
egress_breaks() {
  local what="$1" mode="$2" json="$3" subnet="$4" ip_range="$5" filter="$6" expect="${7:-}"
  local out
  jq "$filter" "$json" >"$tmp/mutant.json" || fail "the fixture filter for $what is wrong"
  if out="$( (egress_check "mutant ($what)" "$mode" "$tmp/mutant.json" "$subnet" "$ip_range") 2>&1 )"; then
    fail "the egress check accepts a service with $what"
  fi
  # It must be the check that refused it, not a broken filter or a jq error.
  [[ "$out" == "compose-compute-network-internal: mutant ($what): "* ]] ||
    fail "the egress check failed on the fixture with $what for another reason: $out"
  [[ -z "$expect" || "$out" == *"$expect"* ]] ||
    fail "the egress check refused the fixture with $what, but not with \"$expect\": $out"
}
egress_checks() { # egress_checks <label> <mode> <rendered json> (also runs the failure fixtures)
  local label="$1" mode="$2" json="$3" subnet ip_range
  subnet="$(jq -r '.networks["compute-internal"].ipam.config[0].subnet' "$json")"
  ip_range="$(jq -r '.networks["compute-internal"].ipam.config[0].ip_range' "$json")"
  egress_check "$label" "$mode" "$json" "$subnet" "$ip_range"
  local f='.services["compute-egress"]'
  egress_breaks "a capability added" "$mode" "$json" "$subnet" "$ip_range" "$f.cap_add = [\"NET_ADMIN\"]"
  egress_breaks "no cap_drop" "$mode" "$json" "$subnet" "$ip_range" "del($f.cap_drop)"
  egress_breaks "user root" "$mode" "$json" "$subnet" "$ip_range" "$f.user = \"root\""
  egress_breaks "user 0:0" "$mode" "$json" "$subnet" "$ip_range" "$f.user = \"0:0\""
  egress_breaks "no user" "$mode" "$json" "$subnet" "$ip_range" "del($f.user)"
  egress_breaks "a writable root" "$mode" "$json" "$subnet" "$ip_range" "$f.read_only = false"
  egress_breaks "privilege escalation allowed" "$mode" "$json" "$subnet" "$ip_range" "del($f.security_opt)"
  egress_breaks "privileged" "$mode" "$json" "$subnet" "$ip_range" "$f.privileged = true"
  egress_breaks "a published port" "$mode" "$json" "$subnet" "$ip_range" "$f.ports = [{target: 53, published: \"53\", protocol: \"udp\"}]"
  egress_breaks "a secret" "$mode" "$json" "$subnet" "$ip_range" "$f.secrets = [{source: \"x\"}]"
  egress_breaks "the Docker socket" "$mode" "$json" "$subnet" "$ip_range" "$f.volumes += [{type: \"bind\", source: \"/var/run/docker.sock\", target: \"/var/run/docker.sock\"}]"
  egress_breaks "a volume" "$mode" "$json" "$subnet" "$ip_range" "$f.volumes += [{type: \"volume\", source: \"secrets\", target: \"/s\"}]"
  egress_breaks "a shell entrypoint" "$mode" "$json" "$subnet" "$ip_range" "$f.entrypoint = [\"/bin/sh\", \"-c\", \"exec compute-egress\"]"
  egress_breaks "a sysctl dropped" "$mode" "$json" "$subnet" "$ip_range" "del($f.sysctls[\"net.ipv4.ip_unprivileged_port_start\"])"
  egress_breaks "forwarding on" "$mode" "$json" "$subnet" "$ip_range" "$f.sysctls[\"net.ipv4.ip_forward\"] = \"1\""
  egress_breaks "a fifth sysctl" "$mode" "$json" "$subnet" "$ip_range" "$f.sysctls[\"net.core.somaxconn\"] = \"4096\""
  egress_breaks "an IPv6 forwarding sysctl" "$mode" "$json" "$subnet" "$ip_range" "$f.sysctls[\"net.ipv6.conf.all.forwarding\"] = \"0\"" "sysctls are"
  egress_breaks "IPv6 on compute-egress" "$mode" "$json" "$subnet" "$ip_range" ".networks[\"compute-egress\"].enable_ipv6 = true" "enable_ipv6"
  egress_breaks "IPv6 on compute-internal" "$mode" "$json" "$subnet" "$ip_range" ".networks[\"compute-internal\"].enable_ipv6 = true" "enable_ipv6"
  egress_breaks "IPv6 unset on compute-internal" "$mode" "$json" "$subnet" "$ip_range" "del(.networks[\"compute-internal\"].enable_ipv6)" "enable_ipv6"
  egress_breaks "another service in the egress network namespace" "$mode" "$json" "$subnet" "$ip_range" \
    ".services.worker.network_mode = \"service:compute-egress\" | del(.services.worker.networks)" "share the namespace"
  egress_breaks "another service in the egress pid namespace" "$mode" "$json" "$subnet" "$ip_range" \
    ".services.proxy.pid = \"service:compute-egress\"" "share the namespace"
  egress_breaks "another service in the functions runtime's ipc namespace" "$mode" "$json" "$subnet" "$ip_range" \
    ".services.proxy.ipc = \"service:functions-runtime\"" "share the namespace"
  egress_breaks "another service in a container's network namespace" "$mode" "$json" "$subnet" "$ip_range" \
    ".services.worker.network_mode = \"container:x\" | del(.services.worker.networks)" "share the namespace"
  egress_breaks "another service on the host network" "$mode" "$json" "$subnet" "$ip_range" \
    ".services.worker.network_mode = \"host\" | del(.services.worker.networks)" "network_mode other than none"
  egress_breaks "a second network key with the egress network's name" "$mode" "$json" "$subnet" "$ip_range" \
    ".networks.egress2 = {name: (.name + \"_compute-egress\")} | .services.worker.networks.egress2 = {}" "the same Docker name"
  egress_breaks "no deny list passed" "$mode" "$json" "$subnet" "$ip_range" "del(.services[\"compute-egress\"].environment.CHELABASE_COMPUTE_EGRESS_DENY)" "deny list"
  egress_breaks "a user whose group is root" "$mode" "$json" "$subnet" "$ip_range" "$f.user = \"65532:0\"" "non-root group"
  egress_breaks "a user without a group" "$mode" "$json" "$subnet" "$ip_range" "$f.user = \"65532\"" "non-root group"
  egress_breaks "an entrypoint that runs a shell through env" "$mode" "$json" "$subnet" "$ip_range" \
    "$f.entrypoint = [\"/usr/bin/env\", \"sh\", \"-c\", \"exec compute-egress\"]" "sets an entrypoint"
  egress_breaks "a command that runs a shell through env" "$mode" "$json" "$subnet" "$ip_range" \
    "$f.command = [\"/usr/bin/env\", \"sh\", \"-c\", \"exec compute-egress\"]" "not only the compute-egress binary"
  egress_breaks "no-new-privileges:false after true" "$mode" "$json" "$subnet" "$ip_range" \
    "$f.security_opt = [\"no-new-privileges:true\", \"no-new-privileges:false\"]" "no-new-privileges"
  egress_breaks "another service at compute-egress's static address" "$mode" "$json" "$subnet" "$ip_range" \
    ".services.worker.networks[\"compute-internal\"].ipv4_address = $f.networks[\"compute-internal\"].ipv4_address" "static address"
  egress_breaks "no descriptor limit" "$mode" "$json" "$subnet" "$ip_range" "del($f.ulimits)"
  egress_breaks "a low descriptor limit" "$mode" "$json" "$subnet" "$ip_range" "$f.ulimits.nofile = {soft: 1024, hard: 1024}"
  egress_breaks "no memory limit" "$mode" "$json" "$subnet" "$ip_range" "del($f.mem_limit)"
  egress_breaks "no pid limit" "$mode" "$json" "$subnet" "$ip_range" "del($f.pids_limit)"
  egress_breaks "a curl health check" "$mode" "$json" "$subnet" "$ip_range" "$f.healthcheck.test = [\"CMD\", \"curl\", \"-f\", \"http://127.0.0.1:8080\"]"
  egress_breaks "no profile" "$mode" "$json" "$subnet" "$ip_range" "del($f.profiles)"
  egress_breaks "the default network" "$mode" "$json" "$subnet" "$ip_range" "$f.networks.default = {}"
  egress_breaks "no compute-egress network" "$mode" "$json" "$subnet" "$ip_range" "del($f.networks[\"compute-egress\"])"
  egress_breaks "the control network" "$mode" "$json" "$subnet" "$ip_range" "$f.networks[\"control-internal\"] = {}"
  egress_breaks "a second member of compute-egress" "$mode" "$json" "$subnet" "$ip_range" ".services.worker.networks[\"compute-egress\"] = {}"
  egress_breaks "the gateway on compute-egress" "$mode" "$json" "$subnet" "$ip_range" ".services.gateway.networks[\"compute-egress\"] = {}"
  egress_breaks "a second member of functions-egress" "$mode" "$json" "$subnet" "$ip_range" ".services.worker.networks[\"functions-egress\"] = {}"
  egress_breaks "compute-egress on functions-egress" "$mode" "$json" "$subnet" "$ip_range" "$f.networks[\"functions-egress\"] = {}"
  egress_breaks "an internal compute-egress network" "$mode" "$json" "$subnet" "$ip_range" ".networks[\"compute-egress\"].internal = true"
  egress_breaks "an address inside the Docker-assigned range" "$mode" "$json" "$subnet" "$ip_range" \
    "$f.networks[\"compute-internal\"].ipv4_address = (.networks[\"compute-internal\"].ipam.config[0].ip_range | sub(\"/.*\"; \"\"))"
  egress_breaks "the subnet's gateway address" "$mode" "$json" "$subnet" "$ip_range" \
    "$f.networks[\"compute-internal\"].ipv4_address = (.networks[\"compute-internal\"].ipam.config[0].subnet | sub(\"[.]0/.*\"; \".1\"))"
  egress_breaks "a resolver that isn't its address" "$mode" "$json" "$subnet" "$ip_range" ".services.worker.environment.CHELABASE_COMPUTE_EGRESS_DNS = \"10.9.9.9\""
  egress_breaks "no resolver for the worker" "$mode" "$json" "$subnet" "$ip_range" "del(.services.worker.environment.CHELABASE_COMPUTE_EGRESS_DNS)"
  egress_breaks "a secret-named setting" "$mode" "$json" "$subnet" "$ip_range" "$f.environment.CHELABASE_SECRETS_KEY_FILE = \"/k\""
}

base="$dir/docker-compose.yml"
render chelabase "$dir/.env.example" -f "$base" >"$tmp/base.json" || fail "rendering the self-hosted install failed"
check "self-hosted install" chelabase "$tmp/base.json"
egress_checks "self-hosted install" base "$tmp/base.json"
# Without the profile there is no egress container: nothing but compute-egress's profile starts it.
no_profile="$(NEON_IMAGE=neon-storage:placeholder COMPUTE_NODE_IMAGE=neon-compute:placeholder \
  docker compose -p chelabase --project-directory "$dir" -f "$base" --env-file "$dir/.env.example" config --services)" ||
  fail "rendering the self-hosted install without profiles failed"
! grep -qx 'compute-egress' <<<"$no_profile" || fail "self-hosted install: compute-egress starts without its profile"
render chelabase "$dir/.env.example" -f "$base" -f "$dir/docker-compose.example.yml" >"$tmp/example.json" ||
  fail "rendering the example overlay failed"
check "example overlay" chelabase "$tmp/example.json"
egress_checks "example overlay" base "$tmp/example.json"

workbench() { # workbench <label> <env file>
  # `mcp`: every workbench compose call turns it on (tools/cli/src/workbench/argv.rs).
  local profiles=(--profile sites --profile runner --profile services --profile compute-egress --profile mcp)
  render chelabase-feat-x "$2" -f "$base" -f "$dir/docker-compose.workbench.yml" >"$tmp/wb.json" ||
    fail "rendering the workbench overlay ($1) failed"
  check "$1" chelabase-feat-x "$tmp/wb.json"
  egress_checks "$1" workbench "$tmp/wb.json"
}
workbench "workbench (fixture, slot 1)" "$fixture"
[[ "$(jq -r '.networks["control-internal"].ipam.config[0].subnet' "$tmp/wb.json")" == "$(sed -n 's/^CHELABASE_CONTROL_INTERNAL_SUBNET=//p' "$fixture")" ]] ||
  fail "the workbench's control network takes the stack's subnet"
[[ "$(jq -r '.networks["compute-internal"].ipam.config[0].subnet' "$tmp/wb.json")" == "$(sed -n 's/^CHELABASE_COMPUTE_SUBNET=//p' "$fixture")" ]] ||
  fail "the workbench's compute network takes the stack's subnet"
for slot in 0 63; do
  b=$((slot * 4))
  sed -E \
    -e "s|^CHELABASE_CONTROL_INTERNAL_SUBNET=.*|CHELABASE_CONTROL_INTERNAL_SUBNET=10.213.$b.0/22|" \
    -e "s|^CHELABASE_CONTROL_INTERNAL_COMPUTE_RANGE=.*|CHELABASE_CONTROL_INTERNAL_COMPUTE_RANGE=10.213.$((b + 2)).0/23|" \
    -e "s|^CHELABASE_ORCHESTRATOR_CONTROL_INTERNAL_IP=.*|CHELABASE_ORCHESTRATOR_CONTROL_INTERNAL_IP=10.213.$b.10|" \
    -e "s|^CHELABASE_RUNNER_CONTROL_INTERNAL_IP=.*|CHELABASE_RUNNER_CONTROL_INTERNAL_IP=10.213.$b.11|" \
    -e "s|^CHELABASE_REALTIME_CONTROL_INTERNAL_IP=.*|CHELABASE_REALTIME_CONTROL_INTERNAL_IP=10.213.$b.12|" \
    -e "s|^CHELABASE_COMPUTE_SUBNET=.*|CHELABASE_COMPUTE_SUBNET=10.212.$b.0/22|" \
    -e "s|^CHELABASE_COMPUTE_RANGE=.*|CHELABASE_COMPUTE_RANGE=10.212.$((b + 2)).0/23|" \
    -e "s|^CHELABASE_COMPUTE_EGRESS_IP=.*|CHELABASE_COMPUTE_EGRESS_IP=10.212.$b.2|" \
    -e "s|^CHELABASE_COMPUTE_EGRESS_DNS=.*|CHELABASE_COMPUTE_EGRESS_DNS=10.212.$b.2|" \
    "$fixture" >"$tmp/slot$slot.env"
  workbench "workbench (slot $slot)" "$tmp/slot$slot.env"
  [[ "$(jq -r '.networks["compute-internal"].ipam.config[0].subnet' "$tmp/wb.json")" == "10.212.$b.0/22" ]] ||
    fail "slot $slot: the compute subnet follows the slot"
  [[ "$(jq -r '.services["compute-egress"].networks["compute-internal"].ipv4_address' "$tmp/wb.json")" == "10.212.$b.2" ]] ||
    fail "slot $slot: the egress address follows the slot (10.212.{4k}.2)"
done
echo "compose-compute-network-internal: ok"
