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

# Neon storage services (Compose profile "neon").
neon_services := "seaweedfs create-bucket storage-broker safekeeper1 storcon-db storcon-hook storage-controller pageserver register-safekeepers"
# The long-running ones: `up --wait` fails on a one-shot that has exited, even with 0.
neon_daemons := "seaweedfs storage-broker safekeeper1 storcon-db storcon-hook storage-controller pageserver"

# List available recipes
default:
    @just --list

# --- Dependencies (Compose: metadata Postgres, NATS, Zitadel) ---

# Start control-plane infrastructure containers
deps-up:
    mkdir -p .cheladb/zitadel
    chmod 777 .cheladb/zitadel 2>/dev/null || true
    docker compose -f {{compose_file}} --env-file {{compose_env}} up -d

# Stop control-plane infrastructure containers (and the Neon storage stack, if running)
deps-down:
    docker compose -f {{compose_file}} --env-file {{compose_env}} --profile neon down

# Show Compose service status
deps-ps:
    docker compose -f {{compose_file}} --env-file {{compose_env}} ps

# Alias: start deps
up: deps-up

# Alias: stop Compose only (does not stop host apps from `just dev`)
down: deps-down

# Idempotent Zitadel IAM ensure (project, API app, demo machine key, tenant bind)
zitadel-bootstrap:
    cargo run -q -p cheladb -- auth bootstrap

# Wipe Zitadel volume only so FirstInstance rewrites admin.pat
zitadel-reset:
    COMPOSE_FILE={{compose_file}} COMPOSE_ENV={{compose_env}} ./scripts/zitadel-reset.sh

# Mint a short-lived demo access token (stdout)
auth-token:
    @cargo run -q -p cheladb -- auth token

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

ui-auth:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -f .cheladb/zitadel/demo.env ]]; then
      set -a
      # shellcheck disable=SC1091
      source .cheladb/zitadel/demo.env
      set +a
    fi
    cargo run -p ui-auth

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
    if ! cargo run -q -p cheladb -- auth bootstrap; then
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

    cargo build -p orchestrator -p worker -p gateway -p ui-auth -p cheladb

    # Start orchestrator first so migrations add tenants.zitadel_org_id, then re-bind.
    nohup setsid cargo run -p orchestrator >"{{dev_dir}}/orchestrator.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"
    sleep 3
    cargo run -q -p cheladb -- auth bootstrap >/dev/null || true

    nohup setsid cargo run -p worker >"{{dev_dir}}/worker.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"
    nohup setsid cargo run -p gateway >"{{dev_dir}}/gateway.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"
    nohup setsid cargo run -p ui-auth >"{{dev_dir}}/ui-auth.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"
    nohup setsid pnpm --filter @cheladb/studio dev >"{{dev_dir}}/studio.log" 2>&1 & echo $! >>"{{dev_dir}}/pids"

    sleep 2
    echo ""
    echo "=== Cheladb dev stack (detached) ==="
    echo "Seed tenant_id:  {{demo_tenant_id}}"
    echo "Seed project_id: {{demo_project_id}}"
    echo "Gateway:         http://127.0.0.1:8000  (front door — call the API here)"
    echo "Orchestrator:    http://127.0.0.1:8081  (internal; requires gateway identity)"
    echo "Worker health:   http://127.0.0.1:8082/health"
    echo "ui-auth:         http://127.0.0.1:8084  (sign-in backend; via the gateway at /auth/v1)"
    echo "Studio:          http://localhost:3000  (sign in / create account)"
    echo "Mailpit (email): http://localhost:8025  (signup verification codes)"
    echo "Zitadel console: http://localhost:8080/ui/console"
    echo "Demo IAM:        .cheladb/zitadel/ (cargo run -p cheladb -- auth token)"
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
    for port in 8000 8081 8082 8084 3000; do
      for pid in $(ss -ltnpH "sport = :$port" 2>/dev/null | grep -oE 'pid=[0-9]+' | cut -d= -f2 | sort -u); do
        cmd="$(tr '\0' ' ' 2>/dev/null <"/proc/$pid/cmdline" || true)"
        if [[ "$cmd" =~ (gateway|orchestrator|worker|ui-auth|vite|node) ]]; then
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
    TOKEN="$(cargo run -q -p cheladb -- auth token)"
    curl -sS -X POST "http://127.0.0.1:8000/v1/databases" \
      -H "content-type: application/json" \
      -H "Authorization: Bearer ${TOKEN}" \
      -d "{\"tenant_id\":\"{{demo_tenant_id}}\",\"project_id\":\"{{demo_project_id}}\",\"name\":\"{{name}}\"}"
    echo

# Stop + remove worker-provisioned Postgres containers (label cheladb.managed=true).
# Does not touch Compose deps. If metadata Postgres is up, also clears `databases` rows.
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

    if docker compose -f "{{compose_file}}" --env-file "{{compose_env}}" exec -T metadata-db \
         pg_isready -U cheladb -d cheladb_metadata >/dev/null 2>&1; then
      deleted="$(docker compose -f "{{compose_file}}" --env-file "{{compose_env}}" exec -T metadata-db \
        psql -U cheladb -d cheladb_metadata -tAc \
        "WITH d AS (DELETE FROM databases RETURNING 1) SELECT count(*) FROM d;" | tr -d '[:space:]')"
      echo "cleared ${deleted:-0} metadata database row(s)"
    else
      echo "metadata Postgres not running — skipped clearing databases table (just up && just sweep-dbs)"
    fi

# --- Checks ---

check:
    cargo check --workspace
    pnpm --filter @cheladb/types typecheck
    pnpm --filter @cheladb/studio typecheck
    pnpm --filter @cheladb/studio test

# --- Neon storage (opt-in until provisioning moves to Neon) ---

# Build the Neon images from source at infra/neon/NEON_REF (~40 min, ~40 GB free disk)
neon-build:
    infra/neon/build.sh neon
    infra/neon/build.sh compute-node

# Start the Neon storage stack (a local neon-build if present, else the pinned GHCR images)
neon-up:
    #!/usr/bin/env bash
    set -euo pipefail
    images="$(scripts/neon-images.sh)"
    # shellcheck disable=SC2086
    export $images
    compose=(docker compose -f {{compose_file}} --env-file {{compose_env}} --profile neon)
    # create-bucket runs as a dependency; register-safekeepers is run last, with its own status.
    "${compose[@]}" up -d --wait {{neon_daemons}}
    "${compose[@]}" run --rm register-safekeepers

# Stop and remove the Neon storage containers (volumes kept)
neon-down:
    docker compose -f {{compose_file}} --env-file {{compose_env}} --profile neon rm -sf {{neon_services}}

# Show the Neon storage containers
neon-ps:
    docker compose -f {{compose_file}} --env-file {{compose_env}} --profile neon ps -a {{neon_services}}

# Check the running Neon storage stack (creates and deletes a throwaway tenant)
neon-smoke:
    scripts/neon-smoke.sh

# Offline checks: Neon build scripts, Compose services, neon scripts
neon-test:
    infra/neon/tests/run.sh

# Round trip through the Rust storage-controller client (needs just neon-up)
neon-test-live:
    cargo test -p cheladb-neon --test storcon_live -- --ignored

# Live sign-up/sign-in/reset/logout through the gateway and ui-auth (needs just dev)
auth-e2e:
    scripts/auth-e2e.sh
