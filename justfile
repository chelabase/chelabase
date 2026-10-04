# Chelabase local lifecycle — https://github.com/casey/just
# Run `just` to list recipes.

set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
set dotenv-load := false

compose_file := "infra/compose/docker-compose.yml"
compose_env := "infra/compose/.env.example"
dev_dir := ".chelabase/dev"

# Seed IDs from crates/chelabase-metadata migrations (demo / default).
demo_tenant_id := "00000000-0000-4000-8000-000000000001"
demo_project_id := "00000000-0000-4000-8000-000000000002"

# Neon storage services (part of the base stack).
neon_services := "seaweedfs create-bucket storage-broker safekeeper1 storcon-db storage-controller pageserver register-safekeepers proxy-tls proxy"

# List available recipes
default:
    @just --list

# --- The one stack and the live queue ---
# One dev stack per machine. Worktrees are plain checkouts (each builds into its own target/);
# live tests, e2e runs and browser passes take turns on the stack through `just live`. The
# recipes below that touch the stack refuse while another worktree holds it.

# Refuse when another worktree holds the stack (it prints the holder); skipped inside just live.
# With the lock free but `just dev` running from another worktree (.chelabase/dev/worktree), a plain
# recipe would hit that worktree's apps: warn on stderr (not an error). `just live` moves dev first.
# CHELABASE_LIVE_SCRIPT replaces scripts/live.sh (tests only).
[private]
_live-check:
    #!/usr/bin/env bash
    set -uo pipefail
    [[ -z "${CHELABASE_LIVE_HOLDER:-}" ]] || exit 0
    "${CHELABASE_LIVE_SCRIPT:-scripts/live.sh}" check || { status=$?; echo "wait in line instead: just live <recipe>" >&2; exit "$status"; }
    owner="$(cat "{{dev_dir}}/worktree" 2>/dev/null || true)"
    if [[ -n "$owner" && "$owner" != "{{justfile_directory()}}" ]]; then
      echo "warning: just dev runs from $owner, not this worktree: this recipe tests that worktree's apps. Run it as: just live <recipe>" >&2
    fi

# Run a recipe (or any command) on the stack in turn: waits in a FIFO queue, holds the stack while
# it runs, and first moves `just dev` here if it runs from another worktree.
# e.g. just live neon-e2e; just live cargo nextest run -p rest --run-ignored ignored-only
[positional-arguments]
live *args:
    #!/usr/bin/env bash
    set -euo pipefail
    [[ "${1:-}" != -- ]] || shift
    if (($# == 0)); then
      echo "usage: just live <recipe or command> [args...]   (e.g. just live neon-e2e)" >&2
      exit 2
    fi
    # A recipe of this justfile runs through just; anything else (just itself, cargo...) as given.
    if [[ " $("{{just_executable()}}" --justfile "{{justfile()}}" --summary) " == *" $1 "* ]]; then
      set -- "{{just_executable()}}" "$@"
    fi
    export LIVE_SWAP_DEV='"{{just_executable()}}" --justfile "{{justfile()}}" --working-directory "{{justfile_directory()}}" _live-swap-dev'
    exec "${CHELABASE_LIVE_SCRIPT:-scripts/live.sh}" run "$@"

# Who holds the stack, and who waits for it (and for how long)
live-status:
    "${CHELABASE_LIVE_SCRIPT:-scripts/live.sh}" status

# Offline checks for the live queue (scripts/live.sh)
live-test:
    scripts/tests/live.sh

# Offline tests for the old-name -> Chelabase rename script (scripts/rename-chelabase.sh)
rename-test:
    scripts/tests/rename-chelabase.sh

# The dev swap `just live` runs while holding the stack: when `just dev` runs from another
# worktree (.chelabase/dev/worktree), stop it (the pids file is shared) and start it here, then
# wait for the gateway. Nothing to do when it runs here, or not at all.
[private]
_live-swap-dev:
    #!/usr/bin/env bash
    set -euo pipefail
    here="{{justfile_directory()}}"
    [[ -f "{{dev_dir}}/pids" ]] || exit 0
    owner="$(cat "{{dev_dir}}/worktree" 2>/dev/null || true)"
    alive=0
    while read -r pid; do
      if [[ -n "$pid" && -d "/proc/$pid" ]]; then alive=1; fi
    done <"{{dev_dir}}/pids"
    if [[ "$owner" == "$here" ]] && ((alive)); then exit 0; fi
    if [[ "$owner" == "$here" ]]; then
      echo "[live] just dev's processes are gone (stale {{dev_dir}}/pids): restarting it"
    else
      echo "[live] just dev runs from ${owner:-another worktree}: moving it to $here"
    fi
    "{{just_executable()}}" dev-down
    CHELABASE_LIVE_HOLDER=1 "{{just_executable()}}" dev
    wait_for() { # wait_for <name> <url> <log>: up to 3 min
      for _ in $(seq 180); do
        if curl -fsS -o /dev/null "$2" 2>/dev/null; then return 0; fi
        sleep 1
      done
      echo "[live] $1 isn't answering after 3 min: see {{dev_dir}}/$3" >&2
      exit 1
    }
    wait_for "the gateway" http://127.0.0.1:8000/health gateway.log
    wait_for "Studio" http://localhost:3000/ studio.log

# Once per machine, after pulling the change that removed the worktree slots: removes the slot
# projects, the hook relay, the slot files and databases, then just deps-up. Asks first unless
# --yes; --dry-run only prints the plan.
stack-reset-slots *args: _live-check
    scripts/stack-reset-slots.sh {{args}}

# --- Dependencies (Compose: metadata Postgres, NATS, Zitadel, Valkey, the Neon storage stack) ---

# Start the infrastructure containers, including Neon (a local neon-build if present, else the pinned GHCR images)
deps-up: _live-check
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p .chelabase/zitadel
    chmod 777 .chelabase/zitadel 2>/dev/null || true
    images="$(scripts/neon-images.sh)"
    # shellcheck disable=SC2086
    export $images
    compose=(docker compose -f {{compose_file}} --env-file {{compose_env}})
    "${compose[@]}" up -d
    # Provisioning needs an active safekeeper: fail here if registering one failed. Read from
    # `ps -a`, not `compose wait`, which says "no containers" once the one-shot has exited.
    for i in $(seq 120); do
      state="$("${compose[@]}" ps -a --format '{{ "{{.State}} {{.ExitCode}}" }}' register-safekeepers 2>/dev/null | head -n1)"
      case "$state" in
        "exited 0") break ;;
        exited* | "") echo "register-safekeepers failed (${state:-no container}): ${compose[*]} logs register-safekeepers" >&2; exit 1 ;;
      esac
      [[ $i -lt 120 ]] || { echo "register-safekeepers still not finished ($state)" >&2; exit 1; }
      sleep 0.5
    done
    # The proxy's CA, for clients that verify its certificate.
    # `compose cp` writes progress to stderr: show it only if the copy fails.
    mkdir -p .chelabase/neon
    cp_err="$("${compose[@]}" cp proxy:/tls/ca.crt .chelabase/neon/proxy-ca.crt 2>&1 >/dev/null)" ||
      { echo "copying the proxy's CA certificate failed: $cp_err" >&2; exit 1; }

# Stop control-plane infrastructure containers, the Neon storage stack and any compute containers
deps-down: _live-check
    docker ps -aq --filter name=^chelabase-compute- | xargs -r docker rm -f
    docker compose -f {{compose_file}} --env-file {{compose_env}} down

# Show Compose service status
deps-ps:
    docker compose -f {{compose_file}} --env-file {{compose_env}} ps

# Alias: start deps
up: deps-up

# Alias: stop Compose only (does not stop host apps from `just dev`)
down: deps-down

# Idempotent Zitadel IAM ensure (project, API app, demo machine key, tenant bind)
zitadel-bootstrap: _live-check
    cargo run -q -p chelabase -- dev bootstrap

# Wipe Zitadel volume only so FirstInstance rewrites admin.pat
zitadel-reset: _live-check
    COMPOSE_FILE={{compose_file}} COMPOSE_ENV={{compose_env}} ./scripts/zitadel-reset.sh

# Mint a short-lived demo access token (stdout)
auth-token:
    @cargo run -q -p chelabase -- dev token

# --- Individual processes (foreground) ---

orchestrator:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -f .chelabase/zitadel/demo.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .chelabase/zitadel/demo.env
      set +a
    fi
    if [[ -f .chelabase/zitadel/actions.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .chelabase/zitadel/actions.env
      set +a
    fi
    cargo run -p orchestrator

worker:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -f .chelabase/zitadel/demo.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .chelabase/zitadel/demo.env
      set +a
    fi
    eval "$(scripts/neon-images.sh)"
    export CHELABASE_COMPUTE_IMAGE="$COMPUTE_NODE_IMAGE"
    cargo run -p worker

gateway:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -f .chelabase/zitadel/demo.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .chelabase/zitadel/demo.env
      set +a
    fi
    cargo run -p gateway

rest:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -f .chelabase/zitadel/demo.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .chelabase/zitadel/demo.env
      set +a
    fi
    cargo run -p rest

auth:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -f .chelabase/zitadel/demo.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .chelabase/zitadel/demo.env
      set +a
    fi
    cargo run -p auth

studio:
    pnpm --filter @chelabase/studio dev

# --- Detached full stack ---

# Start deps + control-plane apps + Studio in background; print seed info
dev: _live-check deps-up
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p "{{dev_dir}}"
    if [[ -f "{{dev_dir}}/pids" ]]; then
      echo "dev already running from $(cat "{{dev_dir}}/worktree" 2>/dev/null || echo 'an unknown worktree') ({{dev_dir}}/pids exists). Run: just dev-down"
      exit 1
    fi
    # Which worktree runs the apps (the dev dir is shared through the .chelabase symlink): just live
    # moves them when another worktree takes the stack.
    echo "{{justfile_directory()}}" >"{{dev_dir}}/worktree"

    echo "waiting for metadata postgres…"
    for i in $(seq 1 30); do
      if docker compose -f "{{compose_file}}" exec -T metadata-db pg_isready -U chelabase -d chelabase_metadata >/dev/null 2>&1; then
        break
      fi
      sleep 1
    done

    # A metadata database other than chelabase_metadata (CHELABASE_METADATA_DATABASE_URL) is created
    # when missing; the orchestrator migrates it below.
    metadata_db="${CHELABASE_METADATA_DATABASE_URL:-}"
    metadata_db="${metadata_db##*/}"
    metadata_db="${metadata_db%%\?*}"
    if [[ -n "$metadata_db" && "$metadata_db" != chelabase_metadata ]]; then
      psql_meta=(docker compose -f "{{compose_file}}" --env-file "{{compose_env}}" exec -T metadata-db psql -U chelabase -d chelabase_metadata -qtA)
      if [[ "$("${psql_meta[@]}" -c "SELECT 1 FROM pg_database WHERE datname = '$metadata_db'")" != 1 ]]; then
        echo "creating metadata database $metadata_db"
        "${psql_meta[@]}" -c "CREATE DATABASE \"$metadata_db\" OWNER chelabase"
      fi
    fi

    echo "bootstrapping Zitadel IAM…"
    if ! cargo run -q -p chelabase -- dev bootstrap; then
      echo ""
      echo "Zitadel bootstrap failed (often missing admin.pat on an old volume)."
      echo "Run: just zitadel-reset && just zitadel-bootstrap"
      exit 1
    fi

    export CHELABASE_METADATA_DATABASE_URL="${CHELABASE_METADATA_DATABASE_URL:-postgres://chelabase:chelabase@127.0.0.1:5433/chelabase_metadata}"
    export CHELABASE_NATS_URL="${CHELABASE_NATS_URL:-nats://127.0.0.1:4222}"
    if [[ -f .chelabase/zitadel/demo.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .chelabase/zitadel/demo.env
      set +a
    fi
    if [[ -f .chelabase/zitadel/actions.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .chelabase/zitadel/actions.env
      set +a
    fi

    # The worker starts computes from this image (a local neon-build if present).
    eval "$(scripts/neon-images.sh)"
    export CHELABASE_COMPUTE_IMAGE="$COMPUTE_NODE_IMAGE"

    cargo build -p orchestrator -p worker -p gateway -p rest -p auth -p chelabase

    # Start orchestrator first so migrations add tenants.zitadel_org_id, then re-bind.
    nohup setsid cargo run -p orchestrator >"{{dev_dir}}/orchestrator.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"
    sleep 3
    cargo run -q -p chelabase -- dev bootstrap >/dev/null || true

    nohup setsid cargo run -p worker >"{{dev_dir}}/worker.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"
    nohup setsid cargo run -p gateway >"{{dev_dir}}/gateway.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"
    nohup setsid cargo run -p rest >"{{dev_dir}}/rest.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"
    nohup setsid cargo run -p auth >"{{dev_dir}}/auth.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"
    nohup setsid pnpm --filter @chelabase/studio dev >"{{dev_dir}}/studio.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"

    sleep 2
    echo ""
    echo "=== Chelabase dev stack (detached) ==="
    echo "Seed tenant_id:  {{demo_tenant_id}}"
    echo "Seed project_id: {{demo_project_id}}"
    echo "Gateway:         http://127.0.0.1:8000  (front door — call the API here)"
    echo "Orchestrator:    http://127.0.0.1:8081  (internal; requires gateway identity)"
    echo "Internal:        http://127.0.0.1:8083  (internal; proxy / compute_ctl / storage controller)"
    echo "Worker health:   http://127.0.0.1:8082/health"
    echo "rest:            http://127.0.0.1:8085  (data API; via the gateway at /data/v1; internal on :8086)"
    echo "auth:            http://127.0.0.1:8084  (sign-in backend; via the gateway at /auth/v1)"
    echo "Studio:          http://localhost:3000  (sign in / create account)"
    echo "Mailpit (email): http://localhost:8025  (signup verification codes)"
    echo "Zitadel console: http://localhost:8080/ui/console"
    echo "Demo IAM:        .chelabase/zitadel/ (cargo run -p chelabase -- dev token)"
    echo "Logs:            {{dev_dir}}/*.log"
    echo ""
    echo "Create a DB:"
    echo "  just create-db-demo"
    echo ""
    echo "Stop host apps:  just dev-down"
    echo "Stop Compose:    just down"
    echo "Stop everything: just stop"

# Stop host processes started by `just dev` (Compose stays up)
dev-down:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ ! -f "{{dev_dir}}/pids" ]]; then
      echo "no {{dev_dir}}/pids — nothing to stop"
      exit 0
    fi
    while read -r pid; do
      [[ -n "$pid" ]] || continue
      # A stale pids file (reboot, crash) can name a pid the OS has since
      # reused: only touch processes that are still our cargo/pnpm apps.
      cmd="$(tr '\0' ' ' 2>/dev/null <"/proc/$pid/cmdline" || true)"
      if [[ ! "$cmd" =~ (cargo|pnpm|target/debug/|vite) ]]; then
        echo "skipping pid $pid (${cmd:-not running})"
        continue
      fi
      # `just dev` starts each app under setsid, so its pid is also its process
      # group id: killing the group also stops grandchildren (cargo → binary,
      # pnpm → vite) that a plain `kill` would orphan.
      kill -- -"$pid" 2>/dev/null || kill "$pid" 2>/dev/null || true
    done <"{{dev_dir}}/pids"
    rm -f "{{dev_dir}}/pids" "{{dev_dir}}/worktree"
    echo "dev host processes stopped"

# Stop host apps then Compose
stop: dev-down down

# Fresh start: stop dev apps; delete tenant DBs, all Zitadel accounts and Mailpit; re-bootstrap
# (The lock is checked first, then the confirmation: _cleanup does the work.)
cleanup: _live-check _cleanup

[private]
[confirm("Delete all tenant databases, every Zitadel account and all Mailpit email? [y/N]")]
_cleanup:
    #!/usr/bin/env bash
    set -euo pipefail
    # Keeps secrets.key, the gateway and cookie keys, the seeded demo tenant/project and the
    # Neon storage stack.
    just="{{just_executable()}}"
    "$just" dev-down
    # Anything still on the dev ports (e.g. an app started by hand) would keep old code running.
    for port in 8000 8081 8082 8084 8085 8086 3000; do
      for pid in $(ss -ltnpH "sport = :$port" 2>/dev/null | grep -oE 'pid=[0-9]+' | cut -d= -f2 | sort -u); do
        cmd="$(tr '\0' ' ' 2>/dev/null <"/proc/$pid/cmdline" || true)"
        if [[ "$cmd" =~ (gateway|orchestrator|worker|auth|rest|vite|node) ]]; then
          echo "stopping pid $pid on :$port ($cmd)"
          kill "$pid" 2>/dev/null || true
        fi
      done
    done
    "$just" deps-up
    "$just" sweep-dbs
    "$just" zitadel-reset
    curl -fsS -X DELETE http://localhost:8025/api/v1/messages >/dev/null && echo "emptied Mailpit"
    for _ in $(seq 60); do [[ -s .chelabase/zitadel/login-client.pat ]] && break; sleep 1; done
    "$just" zitadel-bootstrap
    echo ""
    echo "Sweeping old build artifacts..."
    "$just" sweep-builds
    echo ""
    echo "clean. Run: just dev"

# --- Demo helpers ---

# POST a demo database using seeded tenant/project IDs + minted JWT access token
create-db-demo name="demo1":
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -f .chelabase/zitadel/demo.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .chelabase/zitadel/demo.env
      set +a
    fi
    TOKEN="$(cargo run -q -p chelabase -- dev token)"
    curl -sS -X POST "http://127.0.0.1:8000/v1/databases" \
      -H "content-type: application/json" \
      -H "Authorization: Bearer ${TOKEN}" \
      -d "{\"tenant_id\":\"{{demo_tenant_id}}\",\"project_id\":\"{{demo_project_id}}\",\"name\":\"{{name}}\"}"
    echo

# Remove worker-provisioned containers (label chelabase.managed=true) and Neon computes (label
# chelabase.dev/endpoint-id), delete their storage-controller tenants, and clear the `databases`,
# `branches` and `neon_endpoints` rows. Does not touch Compose deps.
sweep-dbs: _live-check
    #!/usr/bin/env bash
    set -euo pipefail
    ids="$(docker ps -aq --filter 'label=chelabase.managed=true' 2>/dev/null || true)"
    if [[ -z "${ids}" ]]; then
      echo "no managed tenant containers to remove"
    else
      echo "removing managed tenant containers:"
      docker ps -a --filter 'label=chelabase.managed=true' --format '  {{ "{{.Names}}" }}\t{{ "{{.Status}}" }}'
      docker stop ${ids} >/dev/null
      # -v also drops anonymous volumes of databases created before named volumes.
      docker rm -f -v ${ids} >/dev/null
      echo "removed $(echo "${ids}" | wc -w | tr -d ' ') container(s)"
    fi

    volumes="$(docker volume ls -q --filter 'label=chelabase.managed=true' 2>/dev/null || true)"
    if [[ -z "${volumes}" ]]; then
      echo "no managed data volumes to remove"
    else
      docker volume rm -f ${volumes} >/dev/null
      echo "removed $(echo "${volumes}" | wc -w | tr -d ' ') data volume(s)"
    fi

    ids="$(docker ps -aq --filter 'label=chelabase.dev/endpoint-id' 2>/dev/null || true)"
    if [[ -z "${ids}" ]]; then
      echo "no Neon compute containers to remove"
    else
      echo "removing Neon compute containers:"
      docker ps -a --filter 'label=chelabase.dev/endpoint-id' --format '  {{ "{{.Names}}" }}\t{{ "{{.Status}}" }}'
      docker rm -f ${ids} >/dev/null
      echo "removed $(echo "${ids}" | wc -w | tr -d ' ') compute container(s)"
    fi

    if docker compose -f "{{compose_file}}" --env-file "{{compose_env}}" exec -T metadata-db \
         pg_isready -U chelabase -d chelabase_metadata >/dev/null 2>&1; then
      LOG_PREFIX=sweep-dbs
      # shellcheck disable=SC1091
      . scripts/lib/neon-common.sh
      tenants="$(metadata_sql <<<"SELECT DISTINCT tenant_id FROM neon_endpoints;")"
      for tenant in ${tenants}; do
        delete_tenant "${tenant}"
      done
      echo "deleted $(echo "${tenants}" | wc -w | tr -d ' ') storage-controller tenant(s)"
      counts="$(metadata_sql <<<"WITH e AS (DELETE FROM neon_endpoints RETURNING 1), b AS (DELETE FROM branches RETURNING 1), d AS (DELETE FROM databases RETURNING 1) SELECT (SELECT count(*) FROM e) || ' ' || (SELECT count(*) FROM d);")"
      echo "cleared ${counts% *} Neon endpoint row(s) and ${counts#* } metadata database row(s)"
    else
      echo "metadata Postgres not running — skipped clearing databases table (just up && just sweep-dbs)"
    fi

# --- Checks ---

# Regenerate the committed OpenAPI documents (openapi/*.json) from the Rust handlers, then both orval clients
api-gen:
    CHELABASE_BLESS=1 cargo test -p orchestrator -p auth document_is_current
    pnpm --filter @chelabase/management-client generate
    pnpm --filter @chelabase/studio generate:login
    just scalar-sync

# Copy the pnpm-installed Scalar bundle to the orchestrator's assets (pnpm is the single source of the version)
scalar-sync:
    scripts/scalar.sh sync

check:
    cargo check --workspace
    cargo test -p orchestrator -p auth openapi::tests
    pnpm --filter @chelabase/management-client generate
    pnpm --filter @chelabase/studio generate:login
    test -z "$(git status --porcelain -- packages/management-client/src/generated apps/studio/src/lib/login-client/generated openapi)" || { git status --porcelain -- packages/management-client/src/generated apps/studio/src/lib/login-client/generated openapi; echo "generated code is stale: run just api-gen and commit"; exit 1; }
    scripts/scalar.sh check
    pnpm --filter @chelabase/management-client typecheck
    pnpm --filter @chelabase/management-client test
    pnpm --filter @chelabase/studio typecheck
    pnpm --filter @chelabase/studio test
    pnpm --filter @chelabase/client typecheck
    pnpm --filter @chelabase/client test

# Rust tests (nextest, one process per test) plus doctests, which nextest does not run.
# The ignored live tests stay on `cargo test ... -- --ignored` (see neon-test-live).
test:
    cargo nextest run --workspace
    cargo test --workspace --doc

# Delete build artifacts older than `days` from the main checkout's target/ and every worktree's
# (needs cargo-sweep: cargo install cargo-sweep --locked)
sweep-builds days="14":
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v cargo-sweep >/dev/null 2>&1; then
      echo "cargo-sweep isn't installed: skipping (install it: cargo install cargo-sweep --locked)"
      exit 0
    fi
    main="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
    git -C "$main" worktree list --porcelain | sed -n 's/^worktree //p' | while IFS= read -r wt; do
      dir="$wt/target"
      if [[ -d "$dir" ]]; then
        echo "sweeping $dir (older than {{days}} days)"
        # cargo-sweep takes a project path; CARGO_TARGET_DIR says which target dir to clean.
        CARGO_TARGET_DIR="$dir" cargo sweep --time {{days}} "$wt"
      else
        echo "skipping $dir (does not exist)"
      fi
    done

# New worktree at .worktrees/<branch with / as ->; creates the branch from base if it is new.
# It builds into its own target/ and runs nothing: live runs go through just live.
wt-add branch base="main":
    #!/usr/bin/env bash
    set -euo pipefail
    main="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
    branch="{{branch}}"
    dir="$main/.worktrees/${branch//\//-}"
    if git -C "$main" show-ref --verify --quiet "refs/heads/$branch"; then
      git -C "$main" worktree add "$dir" "$branch"
    else
      git -C "$main" worktree add "$dir" -b "$branch" "{{base}}"
    fi
    ln -s "$main/.chelabase" "$dir/.chelabase"
    if [[ -e "$main/apps/studio/.env.local" ]]; then
      ln -s "$main/apps/studio/.env.local" "$dir/apps/studio/.env.local"
    fi
    (cd "$dir" && pnpm install --frozen-lockfile)
    echo "$dir"

# Remove the worktree (refuses if it has changes) and its target/; the branch is kept
wt-remove branch:
    #!/usr/bin/env bash
    set -euo pipefail
    main="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
    branch="{{branch}}"
    git -C "$main" worktree remove "$main/.worktrees/${branch//\//-}"
    echo "removed worktree; branch $branch kept (git branch -d $branch to delete it)"

# --- Neon storage (part of the base stack; see deps-up) ---

# Build the Neon images from source at infra/neon/NEON_REF (~40 min, ~40 GB free disk)
neon-build:
    infra/neon/build.sh neon
    infra/neon/build.sh compute-node

# Show the Neon storage containers
neon-ps:
    docker compose -f {{compose_file}} --env-file {{compose_env}} ps -a {{neon_services}}

# Check the running Neon storage stack (creates and deletes a throwaway tenant)
neon-smoke: _live-check
    scripts/neon-smoke.sh

# Live check through the API: create a database, then psql, SQL over HTTP and the serverless
# driver through the proxy, query cancel, a password reset that works at once, and delete
# (needs just deps-up and just dev)
neon-e2e: _live-check
    scripts/neon-e2e.sh

# Live branching story through the API and the proxy: branch at a time, reset to parent, restore
# with a backup, a point outside the history window, delete (needs just deps-up and just dev)
branch-e2e: _live-check
    scripts/branch-e2e.sh

# Live check that deleting a database, a branch or a superseded timeline reclaims the safekeeper's
# local WAL and S3 backup (needs just deps-up and just dev)
storage-leak-e2e: _live-check
    scripts/storage-leak-e2e.sh

# Live projects and app users story: a project and its database, enable app users, SDK sign-up
# with data, own-row RLS, a branch gets the rows, a Zitadel email change through the webhook,
# refresh rotation and reuse, a user deleted, the project deleted with its org (needs just dev)
app-users-e2e: _live-check
    scripts/app-users-e2e.sh

# Live data API story: keys, queries through the gateway, RLS, generated types, revoke (needs just deps-up and just dev)
data-e2e: _live-check
    scripts/data-e2e.sh

# Live data API across databases and Studio's console: a second database, a key allowed only on it, console CRUD as owner, anon under RLS and other roles, disable (needs just dev)
console-e2e: _live-check
    scripts/console-e2e.sh

# Live SQL editor API: a script as owner and anon under RLS, notice, error position, cancel, a 75 s run through the gateway, the guard (confirm, protected branch), saved queries, the schema document (needs just dev)
sql-editor-e2e: _live-check
    scripts/sql-editor-e2e.sh

# Live login roles story: create a plain role, connect, back a secret key, refused delete, reset,
# revoke, delete (needs just deps-up and just dev)
role-e2e: _live-check
    scripts/role-e2e.sh

# Offline checks: Neon build scripts, Compose services, neon scripts
neon-test:
    infra/neon/tests/run.sh

# Round trip through the Rust storage-controller client (needs just deps-up and just dev: attach notifies the internal listener)
neon-test-live: _live-check
    cargo test -p chelabase-neon --test storcon_live -- --ignored

# Live sign-up/sign-in/reset/logout through the gateway and auth (needs just dev)
auth-e2e: _live-check
    scripts/auth-e2e.sh
