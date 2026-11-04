# Chelabase local lifecycle — https://github.com/casey/just
# Run `just` to list recipes.

set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
set dotenv-load := false
set positional-arguments

# Seed IDs from crates/chelabase-metadata migrations (demo / default).
demo_tenant_id := "00000000-0000-4000-8000-000000000001"
demo_project_id := "00000000-0000-4000-8000-000000000002"

# List available recipes
default:
    @just --list

# --- This checkout's stack (chelabase workbench) ---
# Each checkout has its own stack (Compose project, ports, state); `workbench` manages it.
# e.g. just workbench up; just workbench test -p rest; just workbench psql; just workbench down

# The CLI's workbench commands: up, restart, down, purge, status, logs, psql, exec, test, run, env
workbench *args:
    cargo run -q -p chelabase -- workbench "$@"

# Rerun this stack's bootstrap as `workbench up` does: `chelabase-init bootstrap --demo` inside the
# stack (the identity service's setup, the dev member on a fresh stack)
stack-bootstrap admin_email="":
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build -q -p init
    # No address: dev@chelabase.localhost.
    cargo run -q -p chelabase -- dev bootstrap ${1:+--admin-email "$1"}

# A short-lived member access token of this stack's dev member (stdout)
auth-token:
    #!/usr/bin/env bash
    set -euo pipefail
    set -a
    # shellcheck disable=SC1090
    . "$(cargo run -q -p chelabase -- workbench env)"
    set +a
    cargo run -q -p chelabase -- dev token

# Pixel check of the landing page against committed baselines; `--update-snapshots` re-records; not part of just check
landing-visual *args:
    pnpm --filter @chelabase/landing build && pnpm --filter @chelabase/landing visual "$@"

# Build and deploy the landing page or the docs to Cloudflare from a clean main; `--dry-run` checks
# the build and upload on any branch without deploying
site-deploy app *args:
    scripts/deploy-site.sh {{app}} {{args}}

# --- Demo helpers ---

# POST a demo database using seeded tenant/project IDs and the dev member's access token
create-db-demo name="demo1":
    #!/usr/bin/env bash
    set -euo pipefail
    set -a
    # shellcheck disable=SC1090
    . "$(cargo run -q -p chelabase -- workbench env)"
    set +a
    TOKEN="$(cargo run -q -p chelabase -- dev token)"
    curl -sS -X POST "$CHELABASE_ENTRY_API_URL/v1/databases" \
      -H "content-type: application/json" \
      -H "Authorization: Bearer ${TOKEN}" \
      -d "$(jq -nc --arg t "{{demo_tenant_id}}" --arg p "{{demo_project_id}}" --arg n "$1" '{tenant_id: $t, project_id: $p, name: $n}')"
    echo

# Remove this stack's Neon computes (label chelabase.dev/stack), delete their storage-controller
# tenants, and clear the `databases`, `branches` and `neon_endpoints` rows. Leaves the stack's services.
sweep-dbs:
    cargo run -q -p chelabase -- workbench run scripts/sweep-dbs.sh

# Check that sweep-dbs clears every row that references a database or branch (a populated scratch
# database on the stack's metadata server; the stack's own data is not touched), and that its list
# covers every foreign key to them
sweep-dbs-test:
    cargo run -q -p chelabase -- workbench run scripts/tests/sweep-dbs.sh

# --- Checks ---

# Regenerate the committed OpenAPI documents (openapi/*.json) from the Rust handlers, then both orval clients
api-gen:
    CHELABASE_BLESS=1 cargo test -p orchestrator -p auth -p rest document_is_current
    pnpm --filter @chelabase/management-client generate
    pnpm --filter @chelabase/studio generate:login

# Install the pinned cargo-deny (latest release as of 2026-10-21)
deny-install:
    cargo install cargo-deny --version 0.20.2 --locked

# RustSec advisories and yanked crates against Cargo.lock (deny.toml); fix by upgrading, never by an unapproved ignore
deny:
    cargo deny check advisories

check:
    scripts/tests/entry-urls.sh
    scripts/tests/entry-sh.sh
    scripts/tests/stack-urls.sh
    scripts/tests/workbench-compose.sh
    scripts/tests/compose-key-visibility.sh
    scripts/tests/compose-mask-probe.sh
    scripts/tests/compose-neon-uid-probe.sh
    scripts/tests/compose-secret-argv.sh
    scripts/tests/compose-overlay-equiv.sh
    scripts/tests/compose-secret-boundary.sh
    scripts/tests/compose-no-fixed-secrets.sh
    scripts/tests/compose-env-tunables.sh
    scripts/tests/http-no-redirects.sh
    scripts/tests/workbench-recipes.sh
    scripts/tests/retired-names.sh
    scripts/tests/no-copyleft.sh
    scripts/tests/compose-image-pins.sh
    scripts/tests/image-inputs.sh
    scripts/image-inputs.sh copies
    just deny
    cargo check --workspace
    cargo clippy --workspace --all-targets -- -D warnings
    cargo nextest run --workspace
    pnpm --filter @chelabase/management-client generate
    pnpm --filter @chelabase/studio generate:login
    test -z "$(git status --porcelain -- packages/management-client/src/generated apps/studio/src/lib/login-client/generated openapi)" || { git status --porcelain -- packages/management-client/src/generated apps/studio/src/lib/login-client/generated openapi; echo "generated code is stale: run just api-gen and commit"; exit 1; }
    pnpm --filter @chelabase/management-client typecheck
    pnpm --filter @chelabase/management-client test
    pnpm --filter @chelabase/studio typecheck
    pnpm --filter @chelabase/studio test
    pnpm --filter @chelabase/client typecheck
    pnpm --filter @chelabase/client test
    pnpm --filter @chelabase/design typecheck
    pnpm --filter @chelabase/design test
    pnpm --filter @chelabase/docs typecheck
    pnpm --filter @chelabase/docs build
    pnpm --filter @chelabase/docs test
    pnpm --filter @chelabase/landing typecheck
    pnpm --filter @chelabase/landing build
    pnpm --filter @chelabase/landing test

# Rust tests (nextest, one process per test) plus doctests, which nextest does not run.
# The ignored live tests run on the stack with `just workbench test`.
test:
    cargo nextest run --workspace
    cargo test --workspace --doc

# Build every release image (per-app, chelabase/server, chelabase/studio) from one cargo build.
# Separate from the workbench, which runs host-built debug binaries. Smoke: scripts/tests/images-smoke.sh
images:
    docker buildx bake -f infra/images/docker-bake.hcl

# Check the built release images' index annotations (GHCR repo link); run `scripts/publish-images.sh` first.
images-annotations *images:
    scripts/tests/images-annotations.sh {{images}}

# Delete build artifacts older than `days` from the main checkout's target/ and every worktree's
# (needs cargo-sweep: cargo install cargo-sweep --locked)
sweep-builds days="14":
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v cargo-sweep >/dev/null 2>&1; then
      echo "cargo-sweep isn't installed: skipping (install it: cargo install cargo-sweep --locked)"
      exit 0
    fi
    days="$1"
    main="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
    git -C "$main" worktree list --porcelain | sed -n 's/^worktree //p' | while IFS= read -r wt; do
      dir="$wt/target"
      if [[ -d "$dir" ]]; then
        echo "sweeping $dir (older than $days days)"
        # cargo-sweep takes a project path; CARGO_TARGET_DIR says which target dir to clean.
        CARGO_TARGET_DIR="$dir" cargo sweep --time "$days" "$wt"
      else
        echo "skipping $dir (does not exist)"
      fi
    done

# New worktree at .worktrees/<branch with / as ->; creates the branch from base if it is new.
# It builds the CLI in the new worktree (`workbench slug-of`) and starts no stack: a task worktree
# records the base's stack in `.workbench-stack`; a main-based one gets its own from `just workbench up`.
wt-add branch base="main":
    #!/usr/bin/env bash
    set -euo pipefail
    main="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
    branch="$1"
    base="$2"
    dir="$main/.worktrees/${branch//\//-}"
    if git -C "$main" show-ref --verify --quiet "refs/heads/$branch"; then
      git -C "$main" worktree add "$dir" "$branch"
    else
      git -C "$main" worktree add "$dir" -b "$branch" "$base"
    fi
    ln -s "$main/.chelabase" "$dir/.chelabase"
    if [ "$base" != main ] && [ "$base" != origin/main ]; then
      # A task worktree of a plan lands on the plan's stack: `workbench test`/`run` read this file.
      slug="$(env -C "$dir" cargo run -q -p chelabase -- workbench slug-of "$base" || true)"
      printf '%s\n' "$slug" > "$dir/.workbench-stack"
      if [ -z "$slug" ]; then
        echo "no stack for $base: bring it up there (just workbench up) or pass --stack" >&2
      fi
    fi
    (cd "$dir" && pnpm install --frozen-lockfile)
    echo "$dir"

# Purge the worktree's stack (containers, volumes, state), then remove the worktree (refuses if it
# has changes) and its target/; the branch is kept
wt-remove branch:
    #!/usr/bin/env bash
    set -euo pipefail
    main="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
    branch="$1"
    name="${branch//\//-}"
    dir="$main/.worktrees/$name"
    # The CLI resolves the worktree's own stack from its working directory, so the directory must
    # still be that worktree: anywhere else git would resolve another checkout (main). A missing
    # stack isn't an error.
    top="$(git -C "$dir" rev-parse --show-toplevel 2>/dev/null || true)"
    if [ "$top" = "$dir" ]; then
      env -C "$dir" cargo run -q -p chelabase -- workbench purge ||
        echo "no stack purged for $name (none registered, or the purge failed: see above)" >&2
    else
      echo "warning: $dir isn't the worktree's checkout (git toplevel: ${top:-none}); its stack isn't purged (just workbench purge --stack <slug>)" >&2
    fi
    git -C "$main" worktree remove "$dir"
    echo "removed worktree; branch $branch kept (git branch -d $branch to delete it)"

# --- Neon storage (part of every stack) and the e2e scripts (they run in the stack's runner) ---

# Build the Neon images from source at infra/neon/NEON_REF (~40 min, ~40 GB free disk)
neon-build:
    infra/neon/build.sh neon
    infra/neon/build.sh compute-node

# Check the running Neon storage stack (creates and deletes a throwaway tenant)
neon-smoke:
    cargo run -q -p chelabase -- workbench run scripts/neon-smoke.sh

# Live check through the API: create a database, then psql, SQL over HTTP and the serverless
# driver through the proxy, query cancel, a password reset that works at once, and delete
#
neon-e2e:
    cargo run -q -p chelabase -- workbench run scripts/neon-e2e.sh

# Live branching story through the API and the proxy: branch at a time, reset to parent, restore
# with a backup, a point outside the history window, delete
branch-e2e:
    cargo run -q -p chelabase -- workbench run scripts/branch-e2e.sh

# Live check that deleting a database, a branch or a superseded timeline reclaims the safekeeper's
# local WAL and S3 backup
storage-leak-e2e:
    cargo run -q -p chelabase -- workbench run scripts/storage-leak-e2e.sh

# Live projects and app users story: a project and its database, enable app users, SDK sign-up
# with data, own-row RLS, a branch gets the rows, refresh rotation and reuse, a user deleted, the
# project deleted
app-users-e2e:
    cargo run -q -p chelabase -- workbench run scripts/app-users-e2e.sh

# Live check of what a compute can reach (RT1 R11): no NATS, Valkey or databases; every listener
# on the compute network needs a credential or is public by design
compute-network-e2e:
    cargo run -q -p chelabase -- workbench run scripts/compute-network-e2e.sh

# Live data API story: keys, queries through the gateway, RLS, generated types, revoke
data-e2e:
    cargo run -q -p chelabase -- workbench run scripts/data-e2e.sh

# Data API round-trip benchmark (step 6e): four execution variants under netem RTTs; results in docs/benchmarks
bench-data-api *args:
    #!/usr/bin/env bash
    set -euo pipefail
    # The harness and the live-test binary are built on the host: the runner has no toolchain.
    # Their paths come from cargo's JSON output, so CARGO_TARGET_DIR and profiles are respected.
    harness="$(cargo build -q --release -p bench-data-api --message-format=json |
      jq -rs '[.[] | select(.reason == "compiler-artifact" and .target.name == "bench-data-api" and (.target.kind | index("bin")) and .executable != null)] | last | .executable')"
    [[ -n "$harness" && "$harness" != null ]] || { echo "no bench-data-api binary in cargo's output" >&2; exit 1; }
    bin="$(cargo test -q -p bench-data-api --test variants_live --no-run --message-format=json |
      jq -rs '[.[] | select(.reason == "compiler-artifact" and .target.name == "variants_live" and .executable != null)] | last | .executable')"
    [[ -n "$bin" && "$bin" != null ]] || { echo "no variants_live test binary in cargo's output" >&2; exit 1; }
    cargo run -q -p chelabase -- workbench run --env CHELABASE_BENCH_TEST_BIN="$bin" --env CHELABASE_BENCH_BIN="$harness" scripts/bench-data-api.sh "$@"

# Live data API across databases and Studio's console: a second database, a key allowed only on it, console CRUD as owner, anon under RLS and other roles, disable
console-e2e:
    cargo run -q -p chelabase -- workbench run scripts/console-e2e.sh

# Live SQL editor API: a script as owner and anon under RLS, notice, error position, cancel, a 75 s run through the gateway, the guard (confirm, protected branch), saved queries, the schema document
sql-editor-e2e:
    cargo run -q -p chelabase -- workbench run scripts/sql-editor-e2e.sh

# Live schema merge (step 6g) through the API: a branch and main both change, one conflict resolved, rehearse, a protected main needs confirm, apply, main checked with psql
merge-e2e:
    cargo run -q -p chelabase -- workbench run scripts/merge-e2e.sh

# Live login roles story: create a plain role, connect, back a secret key, refused delete, reset,
# revoke, delete
role-e2e:
    cargo run -q -p chelabase -- workbench run scripts/role-e2e.sh

# Live members story: an email invite (link from Mailpit) and a link invite, a grant, the SQL editor
# acting as a group role, revoke, open sign-up and approval (with
# Mailpit as the identity service's SMTP server)
members-e2e:
    cargo run -q -p chelabase -- workbench run scripts/members-e2e.sh

# Live personal Postgres logins: create and connect through the proxy, a child branch, reset,
# revocation on grant loss (compute suspended), suspend, revoke-now, offboarding, expiry
personal-logins-e2e:
    cargo run -q -p chelabase -- workbench run scripts/personal-logins-e2e.sh

# Live teams story (members hub step 2): a team-only member runs SQL as a Developer team role, a
# downgrade to Viewer refuses it, unlinks the member role and revokes the personal login, a
# Viewer team role reads a full cell through the data API, removal and delete, MH-8
teams-e2e:
    cargo run -q -p chelabase -- workbench run scripts/teams-e2e.sh

# Offline checks: Neon build scripts, Compose services, neon scripts
neon-test:
    infra/neon/tests/run.sh

# Live sign-up/sign-in/reset/logout through the gateway and auth
auth-e2e:
    cargo run -q -p chelabase -- workbench run scripts/auth-e2e.sh

# The self-hosted install end to end, on a throwaway Compose project of its own (no workbench).
# Builds the images from this checkout; SELFHOST_E2E_IMAGES=local uses the ones `just images` built.
selfhost-e2e:
    scripts/selfhost-e2e.sh

# The self-hosted install behind a TLS proxy with no port in the hosts (#24): nginx with a
# throwaway CA in front of a throwaway project, on the local release images (`just images`;
# CHELABASE_IMAGE_REPO / CHELABASE_IMAGE_TAG pick others). Needs node for its socket client.
selfhost-tls-e2e:
    scripts/selfhost-tls-e2e.sh
