# Neon Data Plane in Compose Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Neon's storage stack runs next to the existing dev services from pinned images, opt-in via `just neon-up`, and a smoke test proves it can create and delete a tenant with a timeline.

**Architecture:**
- **`infra/neon/`**: a trimmed port of the old repo's Neon build (pinned ref, `build.sh` for the `neon` and `compute-node` images, tag derivation, offline tests).
- **`infra/compose/docker-compose.yml`**: eight storage services under `profiles: ["neon"]`, with digest-pinned GHCR images as the `${NEON_IMAGE:-…}` defaults.
- **`scripts/neon-images.sh`**: prefers a local build and otherwise pulls the pinned refs, failing with a login-or-build message. **`scripts/neon-smoke.sh`** checks the running stack.
- **`justfile`**: `neon-build`, `neon-up`, `neon-down`, `neon-ps`, `neon-test`, `neon-smoke`; `deps-down` also stops the profile.

**Tech Stack:** bash, Docker Compose v5, just 1.45, curl, jq, Neon at `fa504217c61b`, SeaweedFS 4.47, Postgres 17.

**Spec:** `docs/superpowers/specs/2026-09-28-neon-data-plane-design.md`

## Global Constraints

- **Branch** `neon-data-plane` (already created; the spec is committed on it). Commit with `git commit …` and end every message with:
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg
  ```
- **Pinned images** (exact):
  - `ghcr.io/cheladb/neon:0.1.0@sha256:c84e9c991957184bf7aa88c70b3e1657720812ef0800226753d3ce95c8860f8a`
  - `ghcr.io/cheladb/compute-node-v17:0.1.0@sha256:7a39900c5f59ba428ecee67658fdf679706d556a53d149b237a0e9bbfa361a93`
- **`NEON_REF`:** `fa504217c61bbcaf5c512d75830564541f917f8f`; `neon_tag` is its first 12 chars; local builds are `cheladb/neon:<neon_tag>` and `cheladb/compute-node-v17:<neon_tag>`.
- **Profile:** every new service has `profiles: ["neon"]`. `just deps-up` and `just dev` must not start them.
- **Host ports:** only `127.0.0.1:1234` (storage controller) and `127.0.0.1:9898` (pageserver HTTP). Nothing new binds `0.0.0.0`.
- **Storage-controller hook:** `http://host.docker.internal:8083/storcon`, token `cheladb-dev-storcon-hook`.
- **Dev credentials (inline, commented "local dev only"):** S3 `cheladb-dev` / `cheladb-dev-secret`; storcon-db user `storcon`, password `storcon`, database `storage_controller`.
- **Volumes:** `neon_seaweedfs_data`, `neon_pageserver_data`, `neon_safekeeper1_data`, `neon_storcon_pg_data`.
- **Pull-failure message (exact):** `can't pull ghcr.io/cheladb/neon (the packages are private). Run docker login ghcr.io, or build locally with just neon-build (~40 min, ~40 GB free disk).` (with the image name that failed).
- **Never** run `just sweep-dbs`, touch the `cheladb-db-*` containers or their volumes, run `cargo fmt` across the workspace, overwrite `.cheladb/secrets/secrets.key` or the gateway key pair, or run `just down`/`just stop` (the user's stack is up; use `--dry-run` to check `down`).
- The old repo is at `<repo root>`; copy from it only what this plan names.
- Lint every new shell script with `docker run --rm -v "$PWD:/mnt" -w /mnt koalaman/shellcheck:v0.11.0 -x <files>` (no local shellcheck).

## Review Focus

1. **Not logged in to GHCR** → `just neon-up` stops before `docker compose up`, exits non-zero, and prints the login-or-build message. Pinned by the stub-docker tests in Task 2.
2. **Only one of the two images built locally** → the local one is used, the other is pulled; each image is decided on its own. Pinned by the stub-docker tests in Task 2.
3. **Plain `just deps-up` with no `NEON_IMAGE` set** → Compose still parses and starts only the five existing services. Pinned by the `config --services` tests in Task 2.
4. **Smoke fails halfway (stack down, wrong URL)** → exits 1 naming the failing call, and removes a tenant it created. Pinned by the unreachable-URL test in Task 3.
5. **Running the smoke test twice, or after `neon-down` + `neon-up`** → passes each time (random ids, volumes kept). Pinned by the live steps in Task 3.

---

## File Structure

| File | Responsibility |
|---|---|
| `infra/neon/NEON_REF` | the pinned upstream commit |
| `infra/neon/lib/tags.sh` | derives `neon_ref`, `neon_tag` |
| `infra/neon/build.sh` | builds `cheladb/neon` or `cheladb/compute-node-v17` from source |
| `infra/neon/patches/README.md` | how a patch on top of `NEON_REF` would be added |
| `infra/neon/README.md` | pull vs build, private packages, disk needs, recipes |
| `infra/neon/tests/run.sh` | offline checks for all of the above plus Compose and the two scripts |
| `scripts/neon-images.sh` | prints `NEON_IMAGE=…` / `COMPUTE_NODE_IMAGE=…` for `neon-up` |
| `scripts/neon-smoke.sh` | checks the running stack |
| `infra/compose/docker-compose.yml` | the eight storage services |
| `infra/compose/.env.example`, `infra/compose/moon.yml`, `justfile`, `.gitignore` | wiring |

`neon-images.sh` prints assignments (the recipe does `export $(scripts/neon-images.sh)`) rather than being `source`d as the spec says. It's easier to test and leaks no state into the recipe; the behaviour is the same.

---

### Task 1: Port the trimmed Neon build (`infra/neon/`)

**Files:**
- Create: `infra/neon/NEON_REF`, `infra/neon/lib/tags.sh`, `infra/neon/build.sh`, `infra/neon/patches/README.md`, `infra/neon/README.md`
- Test: `infra/neon/tests/run.sh`
- Modify: `.gitignore`, `justfile` (add `neon-build`, `neon-test`)

**Interfaces:**
- Produces: `infra/neon/lib/tags.sh`, sourced with `here=<path to infra/neon>`, sets `neon_ref` (40-hex) and `neon_tag` (12-hex). `infra/neon/build.sh neon|compute-node`; any other argument exits 2. `infra/neon/tests/run.sh` with helpers `pass`, `fail`, `check <name> <cmd…>`, `refuses <status> <cmd…>`, variables `here` (infra/neon) and `root` (repo root); Task 2 and 3 append sections before the final summary.

- [ ] **Step 1: Write the failing test**

Create `infra/neon/tests/run.sh` (then `chmod +x`):

```bash
#!/usr/bin/env bash
# Offline checks for the Neon build, the Compose services and the neon scripts: no
# network and no Neon images needed (Docker only for `docker compose config`).
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
root="$(cd "$here/../.." && pwd)"
failures=0
pass() { echo "ok   $1"; }
fail() {
  echo "FAIL $1" >&2
  failures=$((failures + 1))
}
check() { # check <name> <command...>
  local name="$1"
  shift
  if "$@"; then pass "$name"; else fail "$name"; fi
}
refuses() { # refuses <expected exit> <command...>
  local want="$1" status=0
  shift
  "$@" >/dev/null 2>&1 || status=$?
  [ "$status" -eq "$want" ]
}

# shellcheck source=../lib/tags.sh
. "$here/lib/tags.sh"

# --- tags ---------------------------------------------------------------------------------
check "NEON_REF is a full commit sha" grep -Eq '^[0-9a-f]{40}$' <<<"$neon_ref"
check "neon_tag is the first 12 chars of NEON_REF" test "$neon_tag" = "${neon_ref:0:12}"

# --- build.sh argument handling (must fail before touching docker or the disk) ------------
check "build.sh without a target exits 2" refuses 2 "$here/build.sh"
check "build.sh with an unknown target exits 2" refuses 2 "$here/build.sh" bogus
check "build.sh no longer builds vm-compute" refuses 2 "$here/build.sh" vm-compute

if [ "$failures" -gt 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "all passed"
```

- [ ] **Step 2: Run it to verify it fails**

Run: `infra/neon/tests/run.sh`
Expected: exits non-zero with `infra/neon/lib/tags.sh: No such file or directory`.

- [ ] **Step 3: Write the ref, tags and build script**

`infra/neon/NEON_REF`:
```
fa504217c61bbcaf5c512d75830564541f917f8f
```

`infra/neon/lib/tags.sh`:
```bash
# shellcheck shell=bash disable=SC2034,SC2154
# (Sourced: $here comes from the caller, and the variables are used there.)
#
# Image tags derived from the pinned ref. Sourced (with $here set to infra/neon) by build.sh,
# tests/run.sh and scripts/neon-images.sh, so they can never disagree.
#
#   neon_tag  first 12 chars of NEON_REF: cheladb/neon and cheladb/compute-node-v17

neon_ref="$(tr -d '[:space:]' <"$here/NEON_REF")"
neon_tag="${neon_ref:0:12}"
```

`infra/neon/build.sh` (then `chmod +x`):
```bash
#!/usr/bin/env bash
# Build Neon images from source at the commit pinned in NEON_REF.
#   build.sh neon          storage image (pageserver, safekeeper, storage_broker,
#                          storage_controller, proxy, ...)
#   build.sh compute-node  compute image (Postgres 17 + neon extension + compute_ctl), run as a
#                          plain container; EXTENSIONS=minimal by default
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/tags.sh
. "$here/lib/tags.sh"
ref="$neon_ref"
short="$neon_tag"
# Sources are checked out under infra/neon/.cache/ (gitignored).
src="$here/.cache/neon"
min_free_gb=40

usage() {
  echo "usage: $0 neon|compute-node" >&2
  exit 2
}

require_disk() {
  free_gb=$(df --output=avail -BG "$here" | tail -1 | tr -dc '0-9')
  if [ "$free_gb" -lt "$min_free_gb" ]; then
    echo "need at least ${min_free_gb}GB free disk, have ${free_gb}GB" >&2
    exit 1
  fi
}

checkout() {
  if [ ! -d "$src/.git" ]; then
    mkdir -p "$src"
    git -C "$src" init -q
    git -C "$src" remote add origin https://github.com/neondatabase/neon.git
  fi
  if [ "$(git -C "$src" rev-parse HEAD 2>/dev/null || true)" != "$ref" ]; then
    git -C "$src" fetch --depth 1 origin "$ref"
    git -C "$src" checkout -q --force "$ref"
  fi
  git -C "$src" submodule update --init --depth 1 \
    vendor/postgres-v14 vendor/postgres-v15 vendor/postgres-v16 vendor/postgres-v17
}

case "${1:-}" in
  neon)
    require_disk
    checkout
    DOCKER_BUILDKIT=1 docker build "$src" \
      --build-arg GIT_VERSION="$ref" \
      --build-arg BUILD_TAG="$short" \
      -t "cheladb/neon:$short"
    echo "built cheladb/neon:$short"
    ;;
  compute-node)
    require_disk
    checkout
    DOCKER_BUILDKIT=1 docker build "$src" \
      -f "$src/compute/compute-node.Dockerfile" \
      --build-arg PG_VERSION=v17 \
      --build-arg BUILD_TAG="$short" \
      --build-arg EXTENSIONS="${EXTENSIONS:-minimal}" \
      -t "cheladb/compute-node-v17:$short"
    echo "built cheladb/compute-node-v17:$short"
    ;;
  *) usage ;;
esac
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `infra/neon/tests/run.sh`
Expected: five `ok` lines, then `all passed`.

- [ ] **Step 5: Docs, gitignore, recipes**

`infra/neon/patches/README.md`:
```markdown
# Patches on upstream Neon

We run Neon at the commit pinned in `../NEON_REF` and don't fork it. If we ever need a
change (for example an extension in the compute image), it goes here as a
`git format-patch` file, and `build.sh` applies it to the pristine checkout before building.

There are no patches yet, so `build.sh` doesn't apply any. The first patch also has to:
- make `build.sh` apply `patches/*.patch` after `checkout`;
- add a patch hash suffix to `neon_tag` in `lib/tags.sh`, so patched and unpatched builds
  never share a tag;
- publish new images and update the digests in `infra/compose/docker-compose.yml`.

Write each patch as a standalone commit in upstream's style, so it could be sent upstream.
```

`infra/neon/README.md`:
```markdown
# Neon images

Cheladb runs Neon's storage stack (pageserver, safekeepers, storage broker, storage
controller) and, from roadmap step 4, its compute image. Both come from
[neondatabase/neon](https://github.com/neondatabase/neon) at the commit in `NEON_REF`.

## Pull (default)

`just neon-up` pulls `ghcr.io/cheladb/neon` and `ghcr.io/cheladb/compute-node-v17`, pinned
by digest in `infra/compose/docker-compose.yml`. **The packages are private:** run
`docker login ghcr.io` first (a token with `read:packages`).

## Build from source

`just neon-build` shallow-clones Neon at `NEON_REF` into `infra/neon/.cache/` and builds
`cheladb/neon:<tag>` and `cheladb/compute-node-v17:<tag>` (`<tag>` = first 12 chars of
`NEON_REF`). The first build needs about 40 GB of free disk and about 40 minutes. When
those local images exist, `just neon-up` uses them instead of pulling.

Reclaim the build cache afterwards with `docker builder prune -f --keep-storage 5GB`.

## Recipes

| Recipe | Does |
|---|---|
| `just neon-build` | build both images from source |
| `just neon-up` | start the storage stack (local images if present, else pull) |
| `just neon-ps` | show its containers |
| `just neon-smoke` | check it: safekeeper Active, pageserver up, create/delete a tenant |
| `just neon-down` | stop it (volumes kept) |
| `just neon-test` | offline checks of these scripts and the Compose file |

The stack is opt-in until provisioning moves to Neon (roadmap step 4). `just down` also
stops it.
```

Append to `.gitignore` after the `.cheladb/` block:
```
# Neon source checkout for `just neon-build`
infra/neon/.cache/
```

Add to `justfile`, after the `# --- Checks ---` section's `zitadel-generate` recipe:
```just
# --- Neon storage (opt-in until provisioning moves to Neon) ---

# Build the Neon images from source at infra/neon/NEON_REF (~40 min, ~40 GB free disk)
neon-build:
    infra/neon/build.sh neon
    infra/neon/build.sh compute-node

# Offline checks: Neon build scripts, Compose services, neon scripts
neon-test:
    infra/neon/tests/run.sh
```

- [ ] **Step 6: Lint and re-run**

Run: `docker run --rm -v "$PWD:/mnt" -w /mnt koalaman/shellcheck:v0.11.0 -x infra/neon/build.sh infra/neon/lib/tags.sh infra/neon/tests/run.sh && just neon-test`
Expected: no shellcheck output, then `all passed`.

- [ ] **Step 7: Commit**

```bash
git add infra/neon .gitignore justfile
git commit -m "Neon: port the trimmed image build and pin (neon + compute-node)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg"
```

---

### Task 2: Storage services in Compose, image selection, recipes

**Files:**
- Modify: `infra/compose/docker-compose.yml`, `infra/compose/.env.example`, `infra/compose/moon.yml`, `justfile`
- Create: `scripts/neon-images.sh`
- Test: `infra/neon/tests/run.sh` (append)

**Interfaces:**
- Consumes: `infra/neon/lib/tags.sh` (`neon_tag`); `tests/run.sh` helpers from Task 1.
- Produces: `scripts/neon-images.sh` → stdout exactly two lines, `NEON_IMAGE=<ref>` and `COMPUTE_NODE_IMAGE=<ref>`; messages on stderr; exit 1 on pull failure. Env `COMPOSE_FILE` overrides the Compose file it reads pins from. Compose services `seaweedfs create-bucket storage-broker safekeeper1 storcon-db storage-controller pageserver register-safekeepers`; justfile variable `neon_services` listing them; recipes `neon-up`, `neon-down`, `neon-ps`.

- [ ] **Step 1: Write the failing tests**

In `infra/neon/tests/run.sh`, insert before the final `if [ "$failures" -gt 0 ]` block:

```bash
# --- Compose ------------------------------------------------------------------------------
compose_file="$root/infra/compose/docker-compose.yml"
check "NEON_IMAGE defaults to a digest-pinned GHCR image" \
  grep -Eq '\$\{NEON_IMAGE:-ghcr\.io/cheladb/neon:[^@}]+@sha256:[0-9a-f]{64}\}' "$compose_file"
check "COMPUTE_NODE_IMAGE defaults to a digest-pinned GHCR image" \
  grep -Eq '\$\{COMPUTE_NODE_IMAGE:-ghcr\.io/cheladb/compute-node-v17:[^@}]+@sha256:[0-9a-f]{64}\}' "$compose_file"

if command -v docker >/dev/null; then
  # env -u: the check must hold with nothing set, as for a plain `just deps-up`.
  compose=(env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -f "$compose_file")
  services() { "${compose[@]}" "$@" config --services | sort | tr '\n' ' '; }
  check "without the profile, only the existing services" \
    test "$(services)" = "mailpit metadata-db nats zitadel zitadel-db "
  check "the neon profile adds the storage services" \
    test "$(services --profile neon)" = "create-bucket mailpit metadata-db nats pageserver register-safekeepers safekeeper1 seaweedfs storage-broker storage-controller storcon-db zitadel zitadel-db "
  published="$("${compose[@]}" --profile neon config --format json | jq -r '
    [.services[] | select((.profiles // []) | index("neon")) | .ports[]?
      | "\(.host_ip // "0.0.0.0"):\(.published)"] | sort | join(" ")')"
  check "storage publishes only 1234 and 9898, on 127.0.0.1" \
    test "$published" = "127.0.0.1:1234 127.0.0.1:9898"
else
  pass "no docker: skipped the compose config checks"
fi

# --- scripts/neon-images.sh (against a stub docker) ---------------------------------------
stub="$(mktemp -d)"
trap 'rm -rf "$stub"' EXIT
cat >"$stub/docker" <<'EOF'
#!/usr/bin/env bash
# Stub: the images named in $STUB_LOCAL exist locally; pulls succeed when STUB_PULL=ok.
echo "$*" >>"$STUB_LOG"
case "$1 $2" in
  "image inspect") [[ " $STUB_LOCAL " == *" $3 "* ]] ;;
  "pull -q") [[ "$STUB_PULL" == ok ]] ;;
  *) exit 99 ;;
esac
EOF
chmod +x "$stub/docker"
images() { # images <local images> <ok|fail>: runs neon-images.sh against the stub
  : >"$stub/log"
  env PATH="$stub:$PATH" STUB_LOG="$stub/log" STUB_LOCAL="$1" STUB_PULL="$2" \
    "$root/scripts/neon-images.sh" 2>"$stub/err"
}
local_neon="cheladb/neon:$neon_tag"
local_compute="cheladb/compute-node-v17:$neon_tag"
pinned_neon='NEON_IMAGE=ghcr\.io/cheladb/neon:[^@]+@sha256:[0-9a-f]{64}'
pinned_compute='COMPUTE_NODE_IMAGE=ghcr\.io/cheladb/compute-node-v17:[^@]+@sha256:[0-9a-f]{64}'

out="$(images "$local_neon $local_compute" fail || true)"
check "local builds are used for both images" \
  test "$out" = "NEON_IMAGE=$local_neon"$'\n'"COMPUTE_NODE_IMAGE=$local_compute"
check "local builds need no pull" bash -c "! grep -q '^pull' '$stub/log'"

out="$(images "" ok || true)"
check "without local builds, NEON_IMAGE is the pinned ref" grep -Eqx "$pinned_neon" <<<"$out"
check "without local builds, COMPUTE_NODE_IMAGE is the pinned ref" grep -Eqx "$pinned_compute" <<<"$out"

out="$(images "$local_neon" ok || true)"
check "a local neon build is used on its own" grep -qx "NEON_IMAGE=$local_neon" <<<"$out"
check "the compute image is still pulled then" grep -Eqx "$pinned_compute" <<<"$out"

status=0
images "" fail >/dev/null || status=$?
check "a failed pull exits 1" test "$status" -eq 1
check "a failed pull says to log in or build" \
  grep -qF "can't pull ghcr.io/cheladb/neon (the packages are private). Run docker login ghcr.io, or build locally with just neon-build (~40 min, ~40 GB free disk)." "$stub/err"
```

- [ ] **Step 2: Run them to verify they fail**

Run: `just neon-test`
Expected: exits 1; `FAIL` for both digest-pin checks, the profile check, the published-ports check and every neon-images check (the script doesn't exist).

- [ ] **Step 3: Add the services to Compose**

In `infra/compose/docker-compose.yml`, after the `name: cheladb` line add:

```yaml

# --- Neon storage (profile "neon": `just neon-up`; opt-in until provisioning moves to Neon) ---
# Pinned by digest; `just neon-up` overrides these with a local `just neon-build` when present.
# The packages are private: `docker login ghcr.io` first (infra/neon/README.md).
x-neon-images:
  neon: &neon-image ${NEON_IMAGE:-ghcr.io/cheladb/neon:0.1.0@sha256:c84e9c991957184bf7aa88c70b3e1657720812ef0800226753d3ce95c8860f8a}
  # Not run until roadmap step 4; recorded here so the pin lives in one place.
  compute: ${COMPUTE_NODE_IMAGE:-ghcr.io/cheladb/compute-node-v17:0.1.0@sha256:7a39900c5f59ba428ecee67658fdf679706d556a53d149b237a0e9bbfa361a93}

# S3 credentials for SeaweedFS. Local dev only.
x-neon-s3-env: &neon-s3-env
  AWS_ACCESS_KEY_ID: cheladb-dev
  AWS_SECRET_ACCESS_KEY: cheladb-dev-secret
  AWS_DEFAULT_REGION: us-east-1
```

Before the top-level `volumes:` key, append these services (inside `services:`):

```yaml

  # --- Neon storage (profile "neon") ------------------------------------------------------
  # Flags and health checks come from the old cheladb repo's core/docker/docker-compose.yml,
  # checked there against the pinned Neon source.

  # All-in-one SeaweedFS (master, volume, filer, S3) for pageserver layers and safekeeper WAL.
  seaweedfs:
    image: chrislusf/seaweedfs:4.47
    profiles: ["neon"]
    restart: unless-stopped
    entrypoint: ["/bin/sh", "-c"]
    command:
      - |
        cat > /tmp/s3.json <<EOT
        {"identities": [{"name": "neon",
          "credentials": [{"accessKey": "cheladb-dev", "secretKey": "cheladb-dev-secret"}],
          "actions": ["Admin", "Read", "Write", "List", "Tagging"]}]}
        EOT
        exec weed server -s3 -s3.config=/tmp/s3.json -dir=/data -volume.max=64 -master.volumeSizeLimitMB=1024
    volumes:
      - neon_seaweedfs_data:/data
    healthcheck:
      test: ["CMD", "nc", "-z", "127.0.0.1", "8333"]
      interval: 2s
      retries: 60

  create-bucket:
    image: *neon-image
    profiles: ["neon"]
    restart: "no"
    environment: *neon-s3-env
    entrypoint: ["/bin/sh", "-c"]
    command:
      - |
        for i in $$(seq 60); do
          aws --endpoint-url http://seaweedfs:8333 s3 mb s3://neon 2>/dev/null ||
            aws --endpoint-url http://seaweedfs:8333 s3 ls s3://neon >/dev/null && exit 0
          sleep 2
        done
        exit 1
    depends_on:
      seaweedfs:
        condition: service_healthy

  storage-broker:
    image: *neon-image
    profiles: ["neon"]
    restart: unless-stopped
    command: ["storage_broker", "--listen-addr=0.0.0.0:50051"]
    healthcheck:
      test: ["CMD", "bash", "-c", "</dev/tcp/127.0.0.1/50051"]
      interval: 2s
      retries: 60

  # One safekeeper keeps WAL on one disk; enough for local dev.
  safekeeper1:
    image: *neon-image
    profiles: ["neon"]
    restart: unless-stopped
    environment: *neon-s3-env
    command:
      - safekeeper
      - --id=1
      - -D
      - /data
      - --listen-pg=0.0.0.0:5454
      - --listen-http=0.0.0.0:7676
      - --advertise-pg=safekeeper1:5454
      - --availability-zone=az-1
      - --broker-endpoint=http://storage-broker:50051
      - "--remote-storage={endpoint='http://seaweedfs:8333', bucket_name='neon', bucket_region='us-east-1', prefix_in_bucket='/safekeeper/'}"
    volumes:
      - neon_safekeeper1_data:/data
    healthcheck:
      test: ["CMD", "curl", "-fsS", "http://127.0.0.1:7676/v1/status"]
      interval: 2s
      retries: 60
    depends_on:
      storage-broker:
        condition: service_healthy
      create-bucket:
        condition: service_completed_successfully

  storcon-db:
    image: postgres:17-alpine
    profiles: ["neon"]
    restart: unless-stopped
    environment:
      # Local dev only.
      POSTGRES_USER: storcon
      POSTGRES_PASSWORD: storcon
      POSTGRES_DB: storage_controller
    volumes:
      - neon_storcon_pg_data:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U storcon -d storage_controller"]
      interval: 2s
      retries: 60

  storage-controller:
    image: *neon-image
    profiles: ["neon"]
    restart: unless-stopped
    command:
      - storage_controller
      # --dev: no JWT auth between storage components. Their ports stay on the Compose
      # network; only the API below is published, on loopback.
      - --dev
      - --listen=0.0.0.0:1234
      # Placement changes are pushed to the control plane's internal listener (roadmap
      # step 3, on the host). Nothing listens there yet; it's only called when a tenant's
      # attachment changes. The token is local dev only.
      - --control-plane-url=http://host.docker.internal:8083/storcon
      - --control-plane-jwt-token=cheladb-dev-storcon-hook
      - --database-url=postgresql://storcon:storcon@storcon-db:5432/storage_controller
    extra_hosts:
      - "host.docker.internal:host-gateway"
    ports:
      # The control plane runs on the host.
      - "127.0.0.1:1234:1234"
    healthcheck:
      test: ["CMD", "curl", "-fsS", "http://127.0.0.1:1234/ready"]
      interval: 2s
      retries: 60
    depends_on:
      storcon-db:
        condition: service_healthy

  # Registers with the storage controller from metadata.json at startup.
  pageserver:
    image: *neon-image
    profiles: ["neon"]
    restart: unless-stopped
    environment: *neon-s3-env
    entrypoint: ["/bin/sh", "-c"]
    command:
      - |
        set -e
        mkdir -p /data/.neon
        echo 'id=1' > /data/.neon/identity.toml
        cat > /data/.neon/pageserver.toml <<EOT
        broker_endpoint='http://storage-broker:50051'
        pg_distrib_dir='/usr/local/'
        listen_pg_addr='0.0.0.0:6400'
        listen_http_addr='0.0.0.0:9898'
        control_plane_api='http://storage-controller:1234/upcall/v1/'
        virtual_file_io_mode='buffered'
        remote_storage={ endpoint='http://seaweedfs:8333', bucket_name='neon', bucket_region='us-east-1', prefix_in_bucket='/pageserver' }
        EOT
        echo '{"host":"pageserver","port":6400,"http_host":"pageserver","http_port":9898,"availability_zone_id":"az-1"}' > /data/.neon/metadata.json
        exec pageserver -D /data/.neon
    ports:
      # HTTP API for the host-side control plane; the pg port (6400) stays internal.
      - "127.0.0.1:9898:9898"
    volumes:
      - neon_pageserver_data:/data
    healthcheck:
      test: ["CMD", "curl", "-fsS", "http://127.0.0.1:9898/v1/status"]
      interval: 2s
      retries: 60
    depends_on:
      storage-broker:
        condition: service_healthy
      storage-controller:
        condition: service_healthy
      create-bucket:
        condition: service_completed_successfully

  # One-shot: registers the safekeepers with the storage controller and marks them Active.
  register-safekeepers:
    image: *neon-image
    profiles: ["neon"]
    restart: "no"
    entrypoint: ["/bin/sh", "-c"]
    command:
      - |
        set -e
        S=http://storage-controller:1234
        for ID in 1; do
          curl -fsS -X POST -H 'Content-Type: application/json' \
            -d "{\"id\":$$ID,\"created_at\":\"2023-10-25T09:11:25Z\",\"updated_at\":\"2024-08-28T11:32:43Z\",\"region_id\":\"local\",\"host\":\"safekeeper$$ID\",\"port\":5454,\"http_port\":7676,\"https_port\":null,\"version\":5957,\"availability_zone_id\":\"az-$$ID\"}" \
            $$S/control/v1/safekeeper/$$ID
          curl -fsS -X POST -H 'Content-Type: application/json' \
            -d '{"scheduling_policy":"Active"}' $$S/control/v1/safekeeper/$$ID/scheduling_policy
        done
    depends_on:
      storage-controller:
        condition: service_healthy
      safekeeper1:
        condition: service_healthy
```

Add to the top-level `volumes:` list:
```yaml
  neon_seaweedfs_data:
  neon_pageserver_data:
  neon_safekeeper1_data:
  neon_storcon_pg_data:
```

- [ ] **Step 4: Write `scripts/neon-images.sh`** (then `chmod +x`)

```bash
#!/usr/bin/env bash
# Chooses the Neon images for `just neon-up`. Prints NEON_IMAGE=… and COMPUTE_NODE_IMAGE=…
# on stdout for the caller to export; messages go to stderr. A local `just neon-build` wins;
# otherwise the digest-pinned GHCR default from the Compose file is pulled.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${COMPOSE_FILE:-$ROOT/infra/compose/docker-compose.yml}"
here="$ROOT/infra/neon"
# shellcheck source=../infra/neon/lib/tags.sh
. "$here/lib/tags.sh"

# pinned <VAR>: the default in the Compose file's ${VAR:-…}.
pinned() {
  sed -nE "s/.*\\\$\\{$1:-([^}]+)\\}.*/\\1/p" "$COMPOSE_FILE" | head -n1
}

choose() { # choose <VAR> <image name>
  local var="$1" local_ref="cheladb/$2:$neon_tag" ref
  if docker image inspect "$local_ref" >/dev/null 2>&1; then
    echo "[neon-images] using local build $local_ref" >&2
    echo "$var=$local_ref"
    return
  fi
  ref="$(pinned "$var")"
  if [[ -z "$ref" ]]; then
    echo "[neon-images] no \${$var:-…} default in $COMPOSE_FILE" >&2
    exit 1
  fi
  if ! docker image inspect "$ref" >/dev/null 2>&1 && ! docker pull -q "$ref" >/dev/null; then
    echo "can't pull ${ref%%[:@]*} (the packages are private). Run docker login ghcr.io, or build locally with just neon-build (~40 min, ~40 GB free disk)." >&2
    exit 1
  fi
  echo "$var=$ref"
}

choose NEON_IMAGE neon
choose COMPUTE_NODE_IMAGE compute-node-v17
```

- [ ] **Step 5: Wire the recipes, env example and moon**

In `justfile`, after `demo_project_id := …` add:
```just

# Neon storage services (Compose profile "neon").
neon_services := "seaweedfs create-bucket storage-broker safekeeper1 storcon-db storage-controller pageserver register-safekeepers"
```

Change `deps-down` to also stop the profile:
```just
# Stop control-plane infrastructure containers (and the Neon storage stack, if running)
deps-down:
    docker compose -f {{compose_file}} --env-file {{compose_env}} --profile neon down
```

Add to the Neon section (after `neon-build`, before `neon-test`):
```just
# Start the Neon storage stack (a local neon-build if present, else the pinned GHCR images)
neon-up:
    #!/usr/bin/env bash
    set -euo pipefail
    images="$(scripts/neon-images.sh)"
    # shellcheck disable=SC2086
    export $images
    docker compose -f {{compose_file}} --env-file {{compose_env}} --profile neon up -d --wait {{neon_services}}

# Stop and remove the Neon storage containers (volumes kept)
neon-down:
    docker compose -f {{compose_file}} --env-file {{compose_env}} --profile neon rm -sf {{neon_services}}

# Show the Neon storage containers
neon-ps:
    docker compose -f {{compose_file}} --env-file {{compose_env}} --profile neon ps {{neon_services}}
```

In `infra/compose/moon.yml`, the `down` task's args become:
```yaml
      - "compose"
      - "-f"
      - "docker-compose.yml"
      - "--profile"
      - "neon"
      - "down"
```

Append to `infra/compose/.env.example` after the Database credentials section:
```
# --- Neon storage (just neon-up; local dev only) ---
# The storage controller calls the control plane's internal listener (roadmap step 3,
# 127.0.0.1:8083 on the host) with this bearer token.
# CHELADB_STORCON_HOOK_TOKEN=cheladb-dev-storcon-hook
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `docker run --rm -v "$PWD:/mnt" -w /mnt koalaman/shellcheck:v0.11.0 -x scripts/neon-images.sh infra/neon/tests/run.sh && just neon-test`
Expected: no shellcheck output; every line `ok`; `all passed`.

Also run: `docker compose -f infra/compose/docker-compose.yml --env-file infra/compose/.env.example --profile neon down --dry-run 2>&1 | grep -c neon`
Expected: a non-zero count (the dry run lists the neon containers it would remove). Do **not** run it without `--dry-run`.

- [ ] **Step 7: Commit**

```bash
git add infra/compose scripts/neon-images.sh infra/neon/tests/run.sh justfile
git commit -m "Compose: Neon storage stack behind the neon profile, pinned by digest

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg"
```

---

### Task 3: Smoke test and live verification

**Files:**
- Create: `scripts/neon-smoke.sh`
- Test: `infra/neon/tests/run.sh` (append), then the live stack
- Modify: `justfile` (add `neon-smoke`)

**Interfaces:**
- Consumes: the running stack from Task 2 (`127.0.0.1:1234`, `127.0.0.1:9898`); recipes `neon-up`, `neon-down`, `neon-ps`.
- Produces: `scripts/neon-smoke.sh`, env `STORCON_URL` (default `http://127.0.0.1:1234`) and `PAGESERVER_URL` (default `http://127.0.0.1:9898`); exit 0 and last line `[neon-smoke] ok`, or exit 1 with `[neon-smoke] FAIL: <call>: <detail>` on stderr.

- [ ] **Step 1: Write the failing test**

In `infra/neon/tests/run.sh`, insert before the final summary block:

```bash
# --- scripts/neon-smoke.sh (offline: nothing listens on port 1) ----------------------------
status=0
STORCON_URL=http://127.0.0.1:1 PAGESERVER_URL=http://127.0.0.1:1 \
  "$root/scripts/neon-smoke.sh" >/dev/null 2>"$stub/smoke-err" || status=$?
check "smoke against a dead stack exits 1" test "$status" -eq 1
check "smoke names the failing call" \
  grep -qF "FAIL: GET http://127.0.0.1:1/control/v1/safekeeper" "$stub/smoke-err"
```

- [ ] **Step 2: Run it to verify it fails**

Run: `just neon-test`
Expected: exits 1; the two smoke checks `FAIL` (the script doesn't exist, so the exit status is 127).

- [ ] **Step 3: Write `scripts/neon-smoke.sh`** (then `chmod +x`)

```bash
#!/usr/bin/env bash
# Checks the running Neon storage stack (`just neon-up`): safekeeper 1 is Active, the
# pageserver answers, and a throwaway tenant with a timeline can be created, read back and
# deleted. Touches nothing else: no metadata database, no user databases.
set -euo pipefail

STORCON="${STORCON_URL:-http://127.0.0.1:1234}"
PAGESERVER="${PAGESERVER_URL:-http://127.0.0.1:9898}"

say() { echo "[neon-smoke] $*"; }
die() {
  echo "[neon-smoke] FAIL: $*" >&2
  exit 1
}
hexid() { od -An -N16 -tx1 /dev/urandom | tr -d ' \n'; }

# call <method> <url> [json body]: prints the response body; dies on no answer or non-2xx.
call() {
  local method="$1" url="$2" body="${3:-}" out status
  local args=(-sS -o /dev/stdout -w '\n%{http_code}' -X "$method")
  if [[ -n "$body" ]]; then
    args+=(-H 'content-type: application/json' -d "$body")
  fi
  out="$(curl "${args[@]}" "$url" 2>&1)" || die "$method $url: no answer (${out##*$'\n'})"
  status="${out##*$'\n'}"
  out="${out%$'\n'*}"
  [[ "$status" == 2* ]] || die "$method $url: $status $out"
  printf '%s' "$out"
}

# Deletion is asynchronous: 202 while in progress, 200 (or 404) once gone.
delete_until_gone() {
  local url="$1" status
  for _ in $(seq 120); do
    status="$(curl -sS -o /dev/null -w '%{http_code}' -X DELETE "$url")" ||
      die "DELETE $url: no answer"
    case "$status" in
      200 | 404) return 0 ;;
      202) sleep 0.5 ;;
      *) die "DELETE $url: $status" ;;
    esac
  done
  die "DELETE $url: still in progress after 60 s"
}

tenant="$(hexid)"
timeline="$(hexid)"
created=""
cleanup() {
  if [[ -n "$created" ]]; then
    curl -sS -o /dev/null -X DELETE "$STORCON/v1/tenant/$tenant" || true
  fi
}
trap cleanup EXIT

sks="$(call GET "$STORCON/control/v1/safekeeper")"
jq -e 'any(.[]; .id == 1 and .scheduling_policy == "Active")' <<<"$sks" >/dev/null ||
  die "safekeeper 1 is not Active: $sks"
say "safekeeper 1 is Active"

call GET "$PAGESERVER/v1/status" >/dev/null
say "pageserver answers"

call POST "$STORCON/v1/tenant" \
  "{\"new_tenant_id\":\"$tenant\",\"placement_policy\":{\"Attached\":0}}" >/dev/null
created=1
say "created tenant $tenant"

call POST "$STORCON/v1/tenant/$tenant/timeline" \
  "{\"new_timeline_id\":\"$timeline\",\"pg_version\":17}" >/dev/null
info="$(call GET "$STORCON/v1/tenant/$tenant/timeline/$timeline")"
jq -e --arg t "$timeline" '.timeline_id == $t' <<<"$info" >/dev/null ||
  die "GET timeline $timeline returned: $info"
say "created and read back timeline $timeline"

delete_until_gone "$STORCON/v1/tenant/$tenant/timeline/$timeline"
delete_until_gone "$STORCON/v1/tenant/$tenant"
created=""
say "deleted the timeline and the tenant"
say "ok"
```

Add to `justfile` after `neon-ps`:
```just
# Check the running Neon storage stack (creates and deletes a throwaway tenant)
neon-smoke:
    scripts/neon-smoke.sh
```

- [ ] **Step 4: Run the offline tests to verify they pass**

Run: `docker run --rm -v "$PWD:/mnt" -w /mnt koalaman/shellcheck:v0.11.0 -x scripts/neon-smoke.sh infra/neon/tests/run.sh && just neon-test`
Expected: no shellcheck output; `all passed`.

- [ ] **Step 5: Bring the stack up on fresh volumes**

Run: `docker volume ls -q | grep neon_ || echo "no neon volumes"`
Expected: `no neon volumes` (so this is a fresh start).

Run: `just neon-up` (first run pulls several GB; allow up to 10 minutes)
Expected: exits 0; `docker compose up --wait` reports the services started and healthy.

Run: `just neon-ps` and `docker ps -a --filter name=cheladb-create-bucket --filter name=cheladb-register-safekeepers --format '{{.Names}} {{.Status}}'`
Expected: six long-running services `(healthy)`; both one-shots `Exited (0)`.

If `up --wait` exits non-zero only because the one-shots exited, check they show `Exited (0)` and everything else is healthy, then report it rather than changing the recipe.

- [ ] **Step 6: Run the smoke test live, twice, and across a restart**

Run: `just neon-smoke && just neon-smoke`
Expected: both end with `[neon-smoke] ok`.

Run: `just neon-down && just neon-up && just neon-smoke`
Expected: ends with `[neon-smoke] ok`; `docker volume ls -q | grep neon_` still lists the four volumes.

- [ ] **Step 7: Check the existing stack is unaffected**

Run: `docker compose -f infra/compose/docker-compose.yml --env-file infra/compose/.env.example config --services | sort`
Expected: `mailpit metadata-db nats zitadel zitadel-db`, one per line (no neon services without the profile).

Run: `docker ps --format '{{.Names}} {{.Status}}' | grep -E 'cheladb-db-|metadata-db|zitadel|nats|mailpit'`
Expected: the same containers running as before this task (the user's three `cheladb-db-*` databases and the five deps).

- [ ] **Step 8: Commit**

```bash
git add scripts/neon-smoke.sh infra/neon/tests/run.sh justfile
git commit -m "Neon: smoke test for the storage stack (tenant + timeline round trip)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01KESSEqEbiuKm7pMwCPe2Wg"
```

Leave the Neon stack running for the user to look at; say so in the report.
