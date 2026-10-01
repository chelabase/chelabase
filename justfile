# Cheladb local lifecycle — https://github.com/casey/just
# Run `just` to list recipes.

set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
set dotenv-load := false

compose_file := "infra/compose/docker-compose.yml"
compose_env := "infra/compose/.env.example"
dev_dir := ".cheladb/dev"

# Seed IDs from crates/cheladb-metadata migrations (demo / default).
demo_tenant_id := "00000000-0000-4000-8000-000000000001"
demo_project_id := "00000000-0000-4000-8000-000000000002"

# Neon storage services (part of the base stack).
neon_services := "seaweedfs create-bucket storage-broker safekeeper1 storcon-db storage-controller pageserver register-safekeepers proxy-tls proxy"

# List available recipes
default:
    @just --list

# --- Dependencies (Compose: metadata Postgres, NATS, Zitadel, Valkey, the Neon storage stack) ---

# Start the infrastructure containers, including Neon (a local neon-build if present, else the pinned GHCR images)
deps-up:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p .cheladb/zitadel
    chmod 777 .cheladb/zitadel 2>/dev/null || true
    images="$(scripts/neon-images.sh)"
    # shellcheck disable=SC2086
    export $images
    compose=(docker compose -f {{compose_file}} --env-file {{compose_env}})
    "${compose[@]}" up -d
    # Provisioning needs an active safekeeper: fail here if registering one failed.
    "${compose[@]}" wait register-safekeepers ||
      { echo "register-safekeepers failed: ${compose[*]} logs register-safekeepers" >&2; exit 1; }
    # The proxy's CA, for clients that verify its certificate.
    # `compose cp` writes progress to stderr: show it only if the copy fails.
    mkdir -p .cheladb/neon
    cp_err="$("${compose[@]}" cp proxy:/tls/ca.crt .cheladb/neon/proxy-ca.crt 2>&1 >/dev/null)" ||
      { echo "copying the proxy's CA certificate failed: $cp_err" >&2; exit 1; }

# Stop control-plane infrastructure containers, the Neon storage stack and any compute containers
deps-down:
    docker ps -aq --filter name=^cheladb-compute- | xargs -r docker rm -f
    docker compose -f {{compose_file}} --env-file {{compose_env}} down

# Show Compose service status
deps-ps:
    docker compose -f {{compose_file}} --env-file {{compose_env}} ps

# Alias: start deps
up: deps-up

# Alias: stop Compose only (does not stop host apps from `just dev`)
down: deps-down

# Idempotent Zitadel IAM ensure (project, API app, demo machine key, tenant bind)
zitadel-bootstrap:
    cargo run -q -p cheladb -- dev bootstrap

# Wipe Zitadel volume only so FirstInstance rewrites admin.pat
zitadel-reset:
    COMPOSE_FILE={{compose_file}} COMPOSE_ENV={{compose_env}} ./scripts/zitadel-reset.sh

# Mint a short-lived demo access token (stdout)
auth-token:
    @cargo run -q -p cheladb -- dev token

# --- Individual processes (foreground) ---

orchestrator:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -f .cheladb/zitadel/demo.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .cheladb/zitadel/demo.env
      set +a
    fi
    cargo run -p orchestrator

worker:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -f .cheladb/zitadel/demo.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .cheladb/zitadel/demo.env
      set +a
    fi
    eval "$(scripts/neon-images.sh)"
    export CHELADB_COMPUTE_IMAGE="$COMPUTE_NODE_IMAGE"
    cargo run -p worker

gateway:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -f .cheladb/zitadel/demo.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .cheladb/zitadel/demo.env
      set +a
    fi
    cargo run -p gateway

rest:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -f .cheladb/zitadel/demo.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .cheladb/zitadel/demo.env
      set +a
    fi
    cargo run -p rest

auth:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -f .cheladb/zitadel/demo.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .cheladb/zitadel/demo.env
      set +a
    fi
    cargo run -p auth

studio:
    pnpm --filter @cheladb/studio dev

# --- Detached full stack ---

# Start deps + control-plane apps + Studio in background; print seed info
dev: deps-up
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p "{{dev_dir}}"
    if [[ -f "{{dev_dir}}/pids" ]]; then
      echo "dev already running ({{dev_dir}}/pids exists). Run: just dev-down"
      exit 1
    fi

    echo "waiting for metadata postgres…"
    for i in $(seq 1 30); do
      if docker compose -f "{{compose_file}}" exec -T metadata-db pg_isready -U cheladb -d cheladb_metadata >/dev/null 2>&1; then
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

    export CHELADB_METADATA_DATABASE_URL="${CHELADB_METADATA_DATABASE_URL:-postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata}"
    export CHELADB_NATS_URL="${CHELADB_NATS_URL:-nats://127.0.0.1:4222}"
    if [[ -f .cheladb/zitadel/demo.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .cheladb/zitadel/demo.env
      set +a
    fi

    # The worker starts computes from this image (a local neon-build if present).
    eval "$(scripts/neon-images.sh)"
    export CHELADB_COMPUTE_IMAGE="$COMPUTE_NODE_IMAGE"

    cargo build -p orchestrator -p worker -p gateway -p rest -p auth -p cheladb

    # Start orchestrator first so migrations add tenants.zitadel_org_id, then re-bind.
    nohup setsid cargo run -p orchestrator >"{{dev_dir}}/orchestrator.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"
    sleep 3
    cargo run -q -p cheladb -- dev bootstrap >/dev/null || true

    nohup setsid cargo run -p worker >"{{dev_dir}}/worker.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"
    nohup setsid cargo run -p gateway >"{{dev_dir}}/gateway.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"
    nohup setsid cargo run -p rest >"{{dev_dir}}/rest.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"
    nohup setsid cargo run -p auth >"{{dev_dir}}/auth.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"
    nohup setsid pnpm --filter @cheladb/studio dev >"{{dev_dir}}/studio.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"

    sleep 2
    echo ""
    echo "=== Cheladb dev stack (detached) ==="
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
    echo "Demo IAM:        .cheladb/zitadel/ (cargo run -p cheladb -- dev token)"
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
    rm -f "{{dev_dir}}/pids"
    echo "dev host processes stopped"

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
    if [[ -f .cheladb/zitadel/demo.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .cheladb/zitadel/demo.env
      set +a
    fi
    TOKEN="$(cargo run -q -p cheladb -- dev token)"
    curl -sS -X POST "http://127.0.0.1:8000/v1/databases" \
      -H "content-type: application/json" \
      -H "Authorization: Bearer ${TOKEN}" \
      -d "{\"tenant_id\":\"{{demo_tenant_id}}\",\"project_id\":\"{{demo_project_id}}\",\"name\":\"{{name}}\"}"
    echo

# Remove worker-provisioned containers (label cheladb.managed=true) and Neon computes (label
# cheladb.dev/endpoint-id), delete their storage-controller tenants, and clear the `databases`,
# `branches` and `neon_endpoints` rows. Does not touch Compose deps.
sweep-dbs:
    #!/usr/bin/env bash
    set -euo pipefail
    ids="$(docker ps -aq --filter 'label=cheladb.managed=true' 2>/dev/null || true)"
    if [[ -z "${ids}" ]]; then
      echo "no managed tenant containers to remove"
    else
      echo "removing managed tenant containers:"
      docker ps -a --filter 'label=cheladb.managed=true' --format '  {{ "{{.Names}}" }}\t{{ "{{.Status}}" }}'
      docker stop ${ids} >/dev/null
      # -v also drops anonymous volumes of databases created before named volumes.
      docker rm -f -v ${ids} >/dev/null
      echo "removed $(echo "${ids}" | wc -w | tr -d ' ') container(s)"
    fi

    volumes="$(docker volume ls -q --filter 'label=cheladb.managed=true' 2>/dev/null || true)"
    if [[ -z "${volumes}" ]]; then
      echo "no managed data volumes to remove"
    else
      docker volume rm -f ${volumes} >/dev/null
      echo "removed $(echo "${volumes}" | wc -w | tr -d ' ') data volume(s)"
    fi

    ids="$(docker ps -aq --filter 'label=cheladb.dev/endpoint-id' 2>/dev/null || true)"
    if [[ -z "${ids}" ]]; then
      echo "no Neon compute containers to remove"
    else
      echo "removing Neon compute containers:"
      docker ps -a --filter 'label=cheladb.dev/endpoint-id' --format '  {{ "{{.Names}}" }}\t{{ "{{.Status}}" }}'
      docker rm -f ${ids} >/dev/null
      echo "removed $(echo "${ids}" | wc -w | tr -d ' ') compute container(s)"
    fi

    if docker compose -f "{{compose_file}}" --env-file "{{compose_env}}" exec -T metadata-db \
         pg_isready -U cheladb -d cheladb_metadata >/dev/null 2>&1; then
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

check:
    cargo check --workspace
    pnpm --filter @cheladb/types typecheck
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

# New worktree at .worktrees/<branch with / as ->; creates the branch from base if it is new
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
    (cd "$dir" && pnpm install --frozen-lockfile)
    echo "$dir"

# Remove the worktree (refuses if it has changes); the branch is kept
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
neon-smoke:
    scripts/neon-smoke.sh

# Live check through the API: create a database, then psql, SQL over HTTP and the serverless
# driver through the proxy, query cancel, a password reset that works at once, and delete
# (needs just deps-up and just dev)
neon-e2e:
    scripts/neon-e2e.sh

# Live branching story through the API and the proxy: branch at a time, reset to parent, restore
# with a backup, a point outside the history window, delete (needs just deps-up and just dev)
branch-e2e:
    scripts/branch-e2e.sh

# Live data API story: keys, queries through the gateway, RLS, generated types, revoke (needs just deps-up and just dev)
data-e2e:
    scripts/data-e2e.sh

# Offline checks: Neon build scripts, Compose services, neon scripts
neon-test:
    infra/neon/tests/run.sh

# Round trip through the Rust storage-controller client (needs just deps-up and just dev: attach notifies the internal listener)
neon-test-live:
    cargo test -p cheladb-neon --test storcon_live -- --ignored

# Live sign-up/sign-in/reset/logout through the gateway and auth (needs just dev)
auth-e2e:
    scripts/auth-e2e.sh
