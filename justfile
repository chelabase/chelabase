# Cheladb local lifecycle — https://github.com/casey/just
# Run `just` to list recipes.

set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
set dotenv-load := false

# The shared tier (project cheladb) and each worktree slot's app tier (project cheladb-s<N>).
# Every recipe that touches either loads the slot first (scripts/lib/slots.sh): the main
# checkout, and any worktree without a slot, is slot 0 with today's ports and names.
shared_file := "infra/compose/shared.yml"
slot_file := "infra/compose/slot.yml"
compose_env := "infra/compose/.env.example"

# Seed IDs from crates/cheladb-metadata migrations (demo / default).
demo_tenant_id := "00000000-0000-4000-8000-000000000001"
demo_project_id := "00000000-0000-4000-8000-000000000002"

# Neon storage services in the shared tier (each slot runs its own proxy).
neon_services := "seaweedfs create-bucket storage-broker safekeeper1 storcon-db storage-controller pageserver register-safekeepers proxy-tls hook-relay"

# List available recipes
default:
    @just --list

# --- Dependencies (Compose: the shared tier, then this worktree slot's app tier) ---

# Start the shared tier (metadata Postgres, Zitadel, Mailpit, the Neon storage stack, the hook
# relay; left as is when it's already up) and this slot's app tier (NATS, Valkey, the proxy).
# Neon: a local neon-build if present, else the pinned GHCR images.
deps-up:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load "$PWD"
    if [[ -n "$(slot_old_layout)" ]]; then
      echo "NATS, Valkey and the proxy still run in the single Compose project of before slots: run just stack-migrate once" >&2
      exit 1
    fi
    mkdir -p "$CHELADB_SHARED_DIR/zitadel" "$CHELADB_SHARED_DIR/slots"
    chmod 777 "$CHELADB_SHARED_DIR/zitadel" 2>/dev/null || true
    images="$(scripts/neon-images.sh)"
    # shellcheck disable=SC2086
    export $images
    if ! docker image inspect "${CHELADB_HOOK_RELAY_IMAGE:-cheladb/hook-relay:dev}" >/dev/null 2>&1; then
      "{{just_executable()}}" hook-relay-build
    fi
    # Only starts what's missing or stopped: other slots may be using the shared tier, so a
    # changed configuration is reported, not applied (just stack-migrate applies it).
    compose_shared up -d --no-recreate
    drift="$(slot_shared_drift)"
    if [[ -n "$drift" ]]; then
      echo "warning: the shared tier runs an older configuration of: $(echo $drift)." >&2
      echo "  When no other slot uses it (just stack), apply it with: just stack-migrate" >&2
    fi
    # Provisioning needs an active safekeeper: fail here if registering one failed.
    slot_shared_oneshot register-safekeepers ||
      { echo "register-safekeepers failed: docker compose -p cheladb logs register-safekeepers" >&2; exit 1; }
    # The slot's proxy mounts the certificate proxy-tls writes.
    slot_shared_oneshot proxy-tls ||
      { echo "proxy-tls failed: docker compose -p cheladb logs proxy-tls" >&2; exit 1; }
    slot_wait_metadata
    slot_ensure_db
    compose_slot up -d
    # The proxy's CA, for clients that verify its certificate (the same for every slot).
    # `compose cp` writes progress to stderr: show it only if the copy fails.
    mkdir -p .cheladb/neon
    cp_err="$(compose_slot cp proxy:/tls/ca.crt .cheladb/neon/proxy-ca.crt 2>&1 >/dev/null)" ||
      { echo "copying the proxy's CA certificate failed: $cp_err" >&2; exit 1; }
    if ((CHELADB_SLOT > 0)); then
      echo "slot $CHELADB_SLOT ($CHELADB_COMPOSE_PROJECT): NATS :$CHELADB_NATS_PORT, Valkey :$CHELADB_VALKEY_PORT, proxy :$CHELADB_PROXY_PG_PORT/:$CHELADB_PROXY_HTTP_PORT, metadata database $CHELADB_METADATA_DB"
    fi

# Stop this slot's app tier and its computes; also the shared tier when no other slot's app tier runs
deps-down:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load "$PWD"
    slot_compute_ids | xargs -r docker rm -f
    compose_slot down
    others="$(slot_running_projects | grep -vx "$CHELADB_COMPOSE_PROJECT" || true)"
    if [[ -n "$others" ]]; then
      echo "shared tier left running: $(echo $others) still use it (just stack)"
    else
      # No slot uses the shared tier: every compute goes with it.
      docker ps -aq --filter name=^cheladb-compute- | xargs -r docker rm -f
      compose_shared down
    fi

# Show Compose service status (the shared tier, then this slot's app tier)
deps-ps:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load "$PWD"
    compose_shared ps
    compose_slot ps

# Alias: start deps
up: deps-up

# Alias: stop Compose only (does not stop host apps from `just dev`)
down: deps-down

# Idempotent Zitadel IAM ensure (project, API app with every slot's Studio redirect, demo machine
# key, tenant bind in this slot's metadata database)
zitadel-bootstrap:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load "$PWD"
    cargo run -q -p cheladb -- dev bootstrap

# Wipe Zitadel volume only so FirstInstance rewrites admin.pat (Zitadel is shared by every slot)
zitadel-reset:
    COMPOSE_FILE={{shared_file}} COMPOSE_ENV={{compose_env}} ./scripts/zitadel-reset.sh

# Mint a short-lived demo access token (stdout)
auth-token:
    @cargo run -q -p cheladb -- dev token

# --- Individual processes (foreground) ---

orchestrator:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load_all "$PWD"
    cargo run -p orchestrator

worker:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load_all "$PWD"
    eval "$(scripts/neon-images.sh)"
    export CHELADB_COMPUTE_IMAGE="$COMPUTE_NODE_IMAGE"
    cargo run -p worker

gateway:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load_all "$PWD"
    cargo run -p gateway

rest:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load_all "$PWD"
    cargo run -p rest

auth:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load_all "$PWD"
    cargo run -p auth

studio:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load_all "$PWD"
    pnpm --filter @cheladb/studio dev

# --- Detached full stack ---

# Start deps + control-plane apps + Studio in background (this worktree slot's ports); print seed info
dev: deps-up
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load "$PWD"
    dev_dir="$CHELADB_DEV_DIR"
    mkdir -p "$dev_dir"
    if [[ -f "$CHELADB_DEV_DIR/pids" ]]; then
      echo "dev already running ($CHELADB_DEV_DIR/pids exists). Run: just dev-down"
      exit 1
    fi

    echo "waiting for metadata postgres…"
    for i in $(seq 1 30); do
      if compose_shared exec -T metadata-db pg_isready -U cheladb -d "$CHELADB_METADATA_DB" >/dev/null 2>&1; then
        break
      fi
      sleep 1
    done

    echo "bootstrapping Zitadel IAM…"
    if ! cargo run -q -p cheladb -- dev bootstrap; then
      echo ""
      echo "Zitadel bootstrap failed (often missing admin.pat on an old volume)."
      echo "Run: just zitadel-reset && just zitadel-bootstrap"
      exit 1
    fi

    # The slot's ports and metadata database, plus demo.env (identity and key paths).
    slot_load_all "$PWD"

    # The worker starts computes from this image (a local neon-build if present).
    eval "$(scripts/neon-images.sh)"
    export CHELADB_COMPUTE_IMAGE="$COMPUTE_NODE_IMAGE"

    cargo build -p orchestrator -p worker -p gateway -p rest -p auth -p cheladb

    # Start orchestrator first so migrations add tenants.zitadel_org_id, then re-bind.
    nohup setsid cargo run -p orchestrator >"$dev_dir/orchestrator.log" 2>&1 & echo $! >>"$CHELADB_DEV_DIR/pids"
    sleep 3
    cargo run -q -p cheladb -- dev bootstrap >/dev/null || true

    nohup setsid cargo run -p worker >"$dev_dir/worker.log" 2>&1 & echo $! >>"$CHELADB_DEV_DIR/pids"
    nohup setsid cargo run -p gateway >"$dev_dir/gateway.log" 2>&1 & echo $! >>"$CHELADB_DEV_DIR/pids"
    nohup setsid cargo run -p rest >"$dev_dir/rest.log" 2>&1 & echo $! >>"$CHELADB_DEV_DIR/pids"
    nohup setsid cargo run -p auth >"$dev_dir/auth.log" 2>&1 & echo $! >>"$CHELADB_DEV_DIR/pids"
    nohup setsid pnpm --filter @cheladb/studio dev >"$dev_dir/studio.log" 2>&1 & echo $! >>"$CHELADB_DEV_DIR/pids"

    sleep 2
    echo ""
    if ((CHELADB_SLOT > 0)); then
      echo "=== Cheladb dev stack (detached), slot $CHELADB_SLOT ==="
    else
      echo "=== Cheladb dev stack (detached) ==="
    fi
    echo "Seed tenant_id:  {{demo_tenant_id}}"
    echo "Seed project_id: {{demo_project_id}}"
    echo "Gateway:         http://$CHELADB_GATEWAY_ADDR  (front door — call the API here)"
    echo "Orchestrator:    http://$CHELADB_ORCHESTRATOR_ADDR  (internal; requires gateway identity)"
    echo "Internal:        http://$CHELADB_INTERNAL_ADDR  (internal; proxy / compute_ctl / storage controller)"
    echo "Worker health:   http://$CHELADB_WORKER_HEALTH_ADDR/health"
    echo "rest:            http://$CHELADB_REST_ADDR  (data API; via the gateway at /data/v1; internal on :${CHELADB_REST_INTERNAL_ADDR##*:})"
    echo "auth:            http://$CHELADB_AUTH_ADDR  (sign-in backend; via the gateway at /auth/v1)"
    echo "Studio:          $CHELADB_STUDIO_URL  (sign in / create account)"
    echo "Mailpit (email): http://localhost:8025  (signup verification codes)"
    echo "Zitadel console: http://localhost:8080/ui/console"
    echo "Demo IAM:        .cheladb/zitadel/ (cargo run -p cheladb -- dev token)"
    echo "Logs:            $dev_dir/*.log"
    echo ""
    echo "Create a DB:"
    echo "  just create-db-demo"
    echo ""
    echo "Stop host apps:  just dev-down"
    echo "Stop Compose:    just down"
    echo "Stop everything: just stop"

# Stop host processes started by `just dev` in this slot (Compose stays up)
dev-down:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load "$PWD"
    # Reads "$CHELADB_DEV_DIR/pids" (this slot's).
    slot_dev_down

# Stop host apps then Compose
stop: dev-down down

# Fresh start: stop dev apps; delete tenant DBs, all Zitadel accounts and Mailpit; re-bootstrap
[confirm("Delete all tenant databases, every Zitadel account and all Mailpit email? [y/N]")]
cleanup:
    #!/usr/bin/env bash
    set -euo pipefail
    # Keeps secrets.key, the gateway and cookie keys, the seeded demo tenant/project and the
    # Neon storage stack.
    just="{{just_executable()}}"
    . scripts/lib/slots.sh
    slot_load "$PWD"
    # Zitadel and Mailpit are shared: resetting them under another slot breaks its sign-ins.
    others="$(slot_running_projects | grep -vx "$CHELADB_COMPOSE_PROJECT" || true)"
    if [[ -n "$others" ]]; then
      echo "other slots run ($(echo $others)): cleanup resets the shared Zitadel and Mailpit; stop them first (just stack)" >&2
      exit 1
    fi
    "$just" dev-down
    # Anything still on the slot's dev ports (e.g. an app started by hand) would keep old code running.
    for port in "$CHELADB_GATEWAY_PORT" "$(slot_port 8081 "$CHELADB_SLOT")" "$(slot_port 8082 "$CHELADB_SLOT")" \
        "$(slot_port 8084 "$CHELADB_SLOT")" "$(slot_port 8085 "$CHELADB_SLOT")" "$(slot_port 8086 "$CHELADB_SLOT")" \
        "$CHELADB_STUDIO_PORT"; do
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
    for _ in $(seq 60); do [[ -s .cheladb/zitadel/login-client.pat ]] && break; sleep 1; done
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
    . scripts/lib/slots.sh
    slot_load_all "$PWD"
    TOKEN="$(cargo run -q -p cheladb -- dev token)"
    curl -sS -X POST "http://$CHELADB_GATEWAY_ADDR/v1/databases" \
      -H "content-type: application/json" \
      -H "Authorization: Bearer ${TOKEN}" \
      -d "{\"tenant_id\":\"{{demo_tenant_id}}\",\"project_id\":\"{{demo_project_id}}\",\"name\":\"{{name}}\"}"
    echo

# This slot only: remove its Neon computes (label cheladb.dev/slot; slot 0 also the old
# worker-provisioned containers, label cheladb.managed=true), delete the storage-controller
# tenants its metadata database names, and clear that database's `databases`, `branches` and
# `neon_endpoints` rows. Other slots and the Compose deps are left alone.
sweep-dbs:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load "$PWD"
    slot_sweep

# --- Checks ---

# Regenerate the committed OpenAPI documents (openapi/*.json) from the Rust handlers, then both orval clients
api-gen:
    CHELADB_BLESS=1 cargo test -p orchestrator -p auth document_is_current
    pnpm --filter @cheladb/management-client generate
    pnpm --filter @cheladb/studio generate:login
    just scalar-sync

# Copy the pnpm-installed Scalar bundle to the orchestrator's assets (pnpm is the single source of the version)
scalar-sync:
    scripts/scalar.sh sync

check:
    cargo check --workspace
    cargo test -p orchestrator -p auth openapi::tests
    pnpm --filter @cheladb/management-client generate
    pnpm --filter @cheladb/studio generate:login
    test -z "$(git status --porcelain -- packages/management-client/src/generated apps/studio/src/lib/login-client/generated openapi)" || { git status --porcelain -- packages/management-client/src/generated apps/studio/src/lib/login-client/generated openapi; echo "generated code is stale: run just api-gen and commit"; exit 1; }
    scripts/scalar.sh check
    pnpm --filter @cheladb/management-client typecheck
    pnpm --filter @cheladb/management-client test
    pnpm --filter @cheladb/studio typecheck
    pnpm --filter @cheladb/studio test
    pnpm --filter @cheladb/client typecheck
    pnpm --filter @cheladb/client test

# Rust tests (nextest, one process per test) plus doctests, which nextest does not run.
# The ignored live tests stay on `cargo test ... -- --ignored` (see neon-test-live).
test:
    cargo nextest run --workspace
    cargo test --workspace --doc

# Delete build artifacts older than `days` from the main checkout's target/ and the shared
# .worktrees/target (needs cargo-sweep: cargo install cargo-sweep --locked)
sweep-builds days="14":
    #!/usr/bin/env bash
    set -euo pipefail
    main="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
    for dir in "$main/target" "$main/.worktrees/target"; do
      if [[ -d "$dir" ]]; then
        echo "sweeping $dir (older than {{days}} days)"
        # cargo-sweep takes a project path; CARGO_TARGET_DIR says which target dir to clean.
        CARGO_TARGET_DIR="$dir" cargo sweep --time {{days}} "$main"
      else
        echo "skipping $dir (does not exist)"
      fi
    done

# New worktree at .worktrees/<branch with / as ->, with its own slot (just stack); creates the
# branch from base if it is new. slot=none (third argument: just wt-add <branch> <base> none) registers no slot and writes no .env.local (an
# implementer worktree that runs no stack)
wt-add branch base="main" slot="auto":
    #!/usr/bin/env bash
    set -euo pipefail
    main="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
    branch="{{branch}}"
    dir="$main/.worktrees/${branch//\//-}"
    # Its own slot (app tier, ports, metadata database; just stack lists them), registered
    # first: a full registry stops here, before anything is created.
    . scripts/lib/slots.sh
    registry="$(slot_registry "$main")"
    if [[ "{{slot}}" != auto && "{{slot}}" != none ]]; then
      echo "slot must be auto or none, not '{{slot}}'" >&2
      exit 1
    fi
    if [[ -e "$dir" ]]; then
      echo "$dir already exists: wt-add only creates new worktrees (just stack-claim gives an existing one a slot)" >&2
      exit 1
    fi
    slot=none
    if [[ "{{slot}}" == "auto" ]]; then
      slot="$(slot_alloc_new "$registry" "$dir")" || {
        echo "no slot for $dir: free one (just stack-free <N> for a stale one, just wt-remove <branch>) or make the worktree without a slot: just wt-add {{branch}} {{base}} none" >&2
        exit 1
      }
    fi
    if git -C "$main" show-ref --verify --quiet "refs/heads/$branch"; then
      git -C "$main" worktree add "$dir" "$branch" || { slot_release "$registry" "$dir" >/dev/null; exit 1; }
    else
      git -C "$main" worktree add "$dir" -b "$branch" "{{base}}" || { slot_release "$registry" "$dir" >/dev/null; exit 1; }
    fi
    # One target dir for every worktree, so dependencies build once.
    if [[ ! -f "$main/.worktrees/.cargo/config.toml" ]]; then
      mkdir -p "$main/.worktrees/.cargo"
      printf '%s\n' \
        '# Every worktree under .worktrees/ shares one target dir, so dependencies build once.' \
        '[build]' \
        "target-dir = \"$main/.worktrees/target\"" \
        >"$main/.worktrees/.cargo/config.toml"
    fi
    ln -s "$main/.cheladb" "$dir/.cheladb"
    if [[ -e "$main/apps/studio/.env.local" ]]; then
      ln -s "$main/apps/studio/.env.local" "$dir/apps/studio/.env.local"
    fi
    if [[ "$slot" == "none" ]]; then
      (cd "$dir" && pnpm install --frozen-lockfile)
      echo "no slot: run just stack-claim there to give it one"
      echo "$dir"
      exit 0
    fi
    slot_write_env_local "$slot" "$dir" "$registry"
    (cd "$dir" && pnpm install --frozen-lockfile)
    echo "slot $slot: gateway :$(slot_port 8000 "$slot"), Studio :$(slot_port 3000 "$slot"), proxy :$(slot_port 4432 "$slot") (just stack)"
    echo "$dir"

# Remove the worktree (refuses if it has changes) and free its slot: stops its dev apps and app
# tier, deletes its databases (tenants, computes) and its metadata database. The branch is kept.
wt-remove branch:
    #!/usr/bin/env bash
    set -euo pipefail
    main="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
    branch="{{branch}}"
    dir="$main/.worktrees/${branch//\//-}"
    . scripts/lib/slots.sh
    registry="$(slot_registry "$main")"
    if [[ -d "$dir" ]] && slot="$(slot_of "$registry" "$dir")"; then
      if [[ -n "$(git -C "$dir" status --porcelain)" ]]; then
        echo "$dir has changes: commit or discard them first (nothing was stopped)" >&2
        exit 1
      fi
      here="$PWD"
      (
        # The registry entry alone says what to tear down: never an inherited environment.
        slot_load_entry "$registry" "$slot" "$here"
        slot_wipe
      )
      slot_release "$registry" "$dir" >/dev/null
      echo "freed slot $slot"
    fi
    git -C "$main" worktree remove "$dir"
    echo "removed worktree; branch $branch kept (git branch -d $branch to delete it)"

# --- Worktree slots (one app tier per worktree over the shared tier) ---

# List the shared tier and every slot in use: registry entry, app tier, dev apps, ports
stack:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    main="$(slot_main_dir "$PWD")"
    slot_stack "$main" "$(slot_registry "$main")"

# Wipe a stale slot (its worktree is gone): stop its dev apps, delete its databases, drop its
# metadata database, remove its app tier and volumes, and free it
stack-free slot:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    main="$(slot_main_dir "$PWD")"
    registry="$(slot_registry "$main")"
    if ! slot_stale "$registry" "{{slot}}"; then
      echo "slot {{slot}} is not stale (free, or its worktree exists: just wt-remove <branch>)" >&2
      exit 1
    fi
    here="$PWD"
    (
      slot_load_entry "$registry" "{{slot}}" "$here"
      slot_wipe
    )
    rm -f "$registry/{{slot}}.env"
    echo "freed slot {{slot}}"

# Give this worktree (made before slots, or by hand) its own slot and write its .env.local
stack-claim:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    main="$(slot_main_dir "$PWD")"
    if [[ "$main" == "$PWD" ]]; then
      echo "the main checkout is slot 0; stack-claim is for worktrees" >&2
      exit 1
    fi
    if [[ -f .cheladb/dev/pids ]] && slot_pids_alive .cheladb/dev/pids && [[ ! -f .env.local ]]; then
      echo "note: slot 0's dev apps are running; if they are this worktree's, run just dev-down before claiming" >&2
    fi
    registry="$(slot_registry "$main")"
    slot="$(slot_alloc "$registry" "$PWD")"
    slot_write_env_local "$slot" "$PWD" "$registry"
    echo "slot $slot: gateway :$(slot_port 8000 "$slot"), Studio :$(slot_port 3000 "$slot"), proxy :$(slot_port 4432 "$slot") (.env.local written)"

# Once per machine after pulling the slots change, and whenever deps-up reports a changed shared
# configuration: remove the old single project's NATS, Valkey and proxy containers (their data
# is disposable) and apply shared.yml (refuses while other slots run). Then: just deps-up
stack-migrate:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load "$PWD"
    mkdir -p "$CHELADB_SHARED_DIR/slots"
    chmod 755 "$CHELADB_SHARED_DIR/slots"
    others="$(slot_running_projects | grep -vx "$CHELADB_COMPOSE_PROJECT" || true)"
    if [[ -n "$others" ]]; then
      echo "other slots run ($(echo $others)): stack-migrate recreates shared containers under them; stop them first" >&2
      exit 1
    fi
    ids="$(slot_old_layout)"
    if [[ -n "$ids" ]]; then
      echo "removing the old NATS, Valkey and proxy containers from project cheladb:"
      # shellcheck disable=SC2086
      slot_describe $ids
      # shellcheck disable=SC2086
      docker rm -f $ids >/dev/null
      echo "kept their old volumes (cheladb_nats_data, cheladb_valkey_data): docker volume rm them when you like"
    fi
    # Apply shared.yml as it is now (the storage controller calls the hook relay; new pins).
    images="$(scripts/neon-images.sh)"
    # shellcheck disable=SC2086
    export $images
    # Always rebuilt, so a relay fix is picked up.
    "{{just_executable()}}" hook-relay-build
    compose_shared up -d
    echo "next: just dev-down (if dev apps run), then just deps-up (starts this slot's app tier) and just dev"

# Offline checks for slots and the Compose split (starts nothing)
stack-test:
    infra/compose/tests/slots.sh

# Build the hook relay image (a host-built binary on a pinned Ubuntu base; deps-up builds it when missing)
hook-relay-build:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -p cheladb-hook-relay
    target="$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)"
    ctx="$(mktemp -d)"; trap 'rm -rf "$ctx"' EXIT
    cp "$target/release/cheladb-hook-relay" "$ctx/"
    docker build -q -f infra/compose/hook-relay.Dockerfile -t "${CHELADB_HOOK_RELAY_IMAGE:-cheladb/hook-relay:dev}" "$ctx"

# --- Neon storage (part of the base stack; see deps-up) ---

# Build the Neon images from source at infra/neon/NEON_REF (~40 min, ~40 GB free disk)
neon-build:
    infra/neon/build.sh neon
    infra/neon/build.sh compute-node

# Show the Neon storage containers
neon-ps:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load "$PWD"
    compose_shared ps -a {{neon_services}}
    compose_slot ps -a proxy

# Check the running Neon storage stack (creates and deletes a throwaway tenant)
neon-smoke:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load "$PWD"
    exec scripts/neon-smoke.sh

# Live check through the API: create a database, then psql, SQL over HTTP and the serverless
# driver through the proxy, query cancel, a password reset that works at once, and delete
# (needs just deps-up and just dev)
neon-e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load "$PWD"
    exec scripts/neon-e2e.sh

# Live branching story through the API and the proxy: branch at a time, reset to parent, restore
# with a backup, a point outside the history window, delete (needs just deps-up and just dev)
branch-e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load "$PWD"
    exec scripts/branch-e2e.sh

# Live data API story: keys, queries through the gateway, RLS, generated types, revoke (needs just deps-up and just dev)
data-e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load "$PWD"
    exec scripts/data-e2e.sh

# Live login roles story: create a plain role, connect, back a secret key, refused delete, reset,
# revoke, delete (needs just deps-up and just dev)
role-e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load "$PWD"
    exec scripts/role-e2e.sh

# Offline checks: Neon build scripts, Compose services, neon scripts
neon-test:
    infra/neon/tests/run.sh

# Round trip through the Rust storage-controller client (needs just deps-up and just dev: attach notifies the internal listener)
neon-test-live:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load "$PWD"
    cargo test -p cheladb-neon --test storcon_live -- --ignored

# Live sign-up/sign-in/reset/logout through the gateway and auth (needs just dev)
auth-e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    . scripts/lib/slots.sh
    slot_load_all "$PWD"
    exec scripts/auth-e2e.sh
