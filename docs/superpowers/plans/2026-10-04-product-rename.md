# Product rename Cheladb → Chelabase: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Strict TDD where there is new code (the rename script); for the sweep fix-ups, RED is the suite failing after the raw sweep and GREEN is it passing after the fix. Work only in your task's worktree, with absolute paths, `git -C <wt>` and `just --justfile <wt>/justfile --working-directory <wt>`. Read `CLAUDE.md`. Never run `cargo fmt` on the workspace. Never touch `.cheladb/` key files except as T8 says.

**Goal:** every name the product shows or uses becomes Chelabase, in this repo and the public forks, as forward commits; the dev data is wiped by moving to a fresh Compose project, with the keys kept.

**Architecture:** a committed, idempotent, tested rename script makes one raw sweep commit. Parallel fix-up tasks then repair each area (Rust, TypeScript, infra, docs) in disjoint files. The forks get branch renames and doc PRs, and the images are re-tagged at the new `NEON_REF`. A live cutover onto fresh `chelabase` volumes is the final proof; after the merge comes the folder move.

**Tech stack:** bash + perl (script), cargo/nextest, pnpm/orval, Docker Compose, `gh`, `docker buildx imagetools`.

**Spec:** `docs/superpowers/specs/2026-10-04-product-rename-design.md` (D1–D8, R1–R4).

## Status

- **Current (2026-10-04):** DONE. T1–T10 done; merged into `main` (`bdc2168`).
- **T10 (opus, 2026-10-04):** dev apps stopped in `.worktrees/product-rename`; fresh clone at `<repo root>` (HEAD `bdc2168`); `.chelabase/{secrets,gateway,auth,neon,zitadel}` copied with `cp -a` (14 files, sha256 identical, nothing regenerated; `dev/` and `live/` not copied); `.claude/settings.local.json` copied (`settings.json` came with the clone); Claude project memory copied to `<claude project dir>`. `pnpm install --frozen-lockfile` ok; `just live just dev` (deps recreated from the new folder: Zitadel's bind mount now points at the new `.chelabase/zitadel`), `neon-e2e` and `data-e2e` passed. Then `docker compose -p cheladb down -v` removed the 16 stopped `cheladb-*` containers, 9 `cheladb_*` volumes and the `cheladb_default` network; no endpoint-labelled `cheladb` computes were left. Old checkout: worktrees `product-rename` (clean) and `product-rename-t3`…`-t6` (forced) removed, their branches deleted. `.worktrees/{org-rename,fork-replay-pins,sql-editor}` in the old folder are leftover folders, not registered worktrees; left untouched. The old folder `<repo root>` is the user's to delete.
- **User's live run (2026-10-04):** with `demo.env`: worker `neon_live`, `branch_live`, `roles_live` and `app_users_live` passed; `rest_live` 29/29.
- **T9:** opus review With fixes (C0 I5 M9; code rename clean); fix batch `df6227b` plus `94ebaab` (runbook order and keys, worktree re-point, live-test headers export `CHELABASE_COMPUTE_IMAGE`, restored `cheladb_platform` and the old-repo comment, stale names); scoped re-review: all addressed; N1 parked (R-l: `compose -p` works by name), N2 fixed (R-m). I3: the user commits `.claude/settings.json` on `main`.
- **T8 (opus):** the old stack stopped (containers, volumes and the `cheladb_default` network parked); 7 key files plus `neon/compute-jwt.pub` (R-j: `compute_jwt::ensure` needs the pair) copied to `<main>/.chelabase/` with matching checksums; nothing regenerated. `deps-up` and `dev` on fresh `chelabase` volumes. 11/11 e2e (neon-smoke, neon-test-live, neon-e2e, branch, data, console, role, auth, app-users, sql-editor, storage-leak). Ignored live tests: 178 pass; the harness can't load `demo.env`, so `rest_live` and the worker's `neon_live`/`branch_live`/`roles_live`/`app_users_live` are left for the user; `docker_backend_live lifecycle` flaked once and passed on rerun. Playwright: "Chelabase Studio" titles, `select chelabase.user_id()` ok. Test data left: project `t8-rename`, db `t8db`.
- **Done:**
  - T1 `609f841` (sonnet; RED 20 failures → 7/7 GREEN; sonnet review: spec ✅, Approved, minors deferred).
  - T2 `2a002eb` (haiku): the raw sweep, 342 files, 96 renames; a second dry-run moved nothing.
  - T6 `76270ff` (sonnet; docs, copied in from its worktree): history note, present-tense renames, second-computer runbook.
  - T7 (opus, user's go "Go, all of it"): `chelabase/postgres` branches `REL_{17,16,15,14}_STABLE_chelabase` + PR #2 (merge `1915cc9bba5`); `chelabase/neon` PR #2 (merge `956332507a7b`, the new `NEON_REF`); rust-postgres branch `chelabase/simple-query-tags-v2` (the old `cheladb/simple-query-tags` no longer existed); re-tags `neon-storage`/`neon-compute-v17:956332507a7b-dev` with the same digests (`--prefer-index=false` is needed; two untagged wrapper indexes left in GHCR); local clones renamed to `chelabase-neon`/`chelabase-postgres`.
  - T3 `3c49e32`, T4 `c0feb1d`, T5 `5385e05`, T7 repin `0a91c49` (one sonnet agent, sequentially in `product-rename`): nextest 1384/1384, clippy clean, `just check` green (vitest 17/477/108), Studio typecheck and build, compose config, `rename-test`, `live-test`, `neon-test`; the leftover gate is clean.
- **Incident (2026-10-04):** this background session's `EnterWorktree` pin made the harness refuse git, Edit and Write in the sibling `-tN` worktrees. Three wave 2 agents hung for about 5 h before being stopped (ruling R-i: redone sequentially). Options are being discussed with the user: `worktree.bgIsolation: "none"`, or the Agent tool's native `isolation: "worktree"` plus `worktree.baseRef: "head"` (a spike is proposed).
- **User decision:** the old GHCR packages `cheladb-controlplane`/`-identity`/`-gateway`/`-studio` are left alone.
- **Execution rulings (pre-flight table, coordinator):** R-a: only T3 commits `Cargo.lock` (T4 discards its lockfile changes). R-b: T6 owns every `*.md`, including package READMEs; T4 excludes `*.md`. R-c: `infra/compose/.env.example` belongs to T5. *Cost if wrong: a merge conflict, nothing more.*
- **Pre-flight scan (2026-10-04, static, coordinator):**
  - 330 tracked files match `cheladb` (all text); about 3,300 matches outside `docs/`. 9 crate directories under `crates/cheladb-*`; the CLI package and binary are both `cheladb` (`tools/cli/Cargo.toml`).
  - No ignored env file exists on this machine (`apps/studio/.env.local` and `infra/compose/.env` are absent), so the spec's script "file mode" is dropped (P1).
  - `.cheladb/` key material: `secrets/secrets.key`, `gateway/internal-signing.pem`, `gateway/internal-verify.pem`, `auth/app-jwt.key`, `auth/app-jwt.pub`, `auth/cookie.key`, `neon/compute-jwt.pem`. Regenerated state: `zitadel/` (`admin.pat`, `login-client.pat`, `demo-sa.json`, `demo.env`, `actions.env`), `dev/`, `live/`.
  - `wt-add` (`justfile:534-536`) symlinks `<main>/.cheladb` into each worktree; `scripts/live.sh:18` queues in `<main checkout>/.cheladb/live`. After the sweep, both use `.chelabase`, so old-name and new-name code queue separately: T8 must stop the old stack and check that nobody holds the old queue.
  - `NEON_REF` lives in `infra/neon/NEON_REF`; image pins are in `infra/compose/docker-compose.yml:10-12`; bump procedure: `infra/neon/README.md` "Bump `NEON_REF`". `just neon-test` checks that the ref exists on the fork.
  - Recipes for live verification: `neon-e2e`, `branch-e2e`, `data-e2e`, `console-e2e`, `role-e2e`, `auth-e2e`, `app-users-e2e`, `sql-editor-e2e`, `storage-leak-e2e`, `neon-smoke`, `neon-test-live`, plus the ignored Rust live tests.
  - Only this branch's worktree exists (`git worktree list`); no other session holds a worktree.
- **Rulings (each with its cost if wrong):**
  - **P1. No script file mode.** Nothing ignored needs a rewrite here; the second computer's runbook uses one `perl -pi` line with the same map. *Cost: a second-computer env file missed until its first failed run.*
  - **P2. The keep list is `cheladb-v1`, `cheladb-scaffold` and `github.com/ChelaDB`.** The `-pre-replay` repo names contain no `cheladb`, so they need no entry (corrects spec R4). *Cost: none.*
  - **P3. Excluded from the sweep:** all of `docs/superpowers/` (including this step's spec and plan, which name the old spellings on purpose; corrects the spec's Part 1), `docs/roadmap/`, the script and its test fixtures, `Cargo.lock`, `pnpm-lock.yaml`, `openapi/`, `packages/management-client/src/generated/`, `apps/studio/src/lib/login-client/generated/`. *Cost: a stale generated file, caught by `just check`'s stale check.*
  - **P4. Crypto domain-separation strings change with the sweep** (HKDF info, AAD, JWT issuer and audience, key ids containing `cheladb`). Existing sealed values and tokens become unreadable, which is acceptable only because D7 wipes the data. T3 lists every such string in its report, for the record. *Cost: none on fresh volumes.*
  - **P5. Studio's per-browser state** (localStorage keys such as the SQL history) starts empty under the new key names. *Cost: lost dev-browser history.*
- **More execution rulings:** R-d: `.superpowers/` and the `.cheladb` symlink must never be committed (explicit staging; `.gitignore` covers them). R-e: `wt-add`'s frozen `pnpm install` fails before the lockfile regenerates. R-f: short-lived `-tN` branches are not pushed. R-g/R-i: worktree-pin workaround (see Incident). R-h: re-tags use `--prefer-index=false`.
- **Deferred minors (end batch):** T1: keep-list-only files are rewritten with identical content; no move-target collision check; the `excluded_trees_untouched` test can pass vacuously, and edge-case tests are missing (CRLF, no trailing newline, exec bit, usage exit code). T5: `bash -n` over the scripts not run (harness guard). T7: the runbook should cover the v17 submodule's single-branch refspec and the stale local `REL_*_cheladb` branches; the `infra/neon/README.md` re-tag note should name `--prefer-index=false`.
- **P4 record (T3):** changed with the sweep: the secrets key-check AAD and plaintext, the login/session AADs, the token issuers, the cookie names, and the NATS stream, subject and consumer names. Fresh volumes (D7) make this a non-issue.
- **Left running:** the `chelabase` deps and dev apps, run from `<repo root>`.

## Global Constraints

- Display name **Chelabase**; CLI binary **`chelabase`**; env prefix **`CHELABASE_`**; in-DB schema **`chelabase`**, roles `chelabase_*`, settings `chelabase.*`; crates `chelabase-*`; npm `@chelabase/*`; headers `Chelabase-User` / `Chelabase-Identity`; Compose project `chelabase`; label `chelabase.dev/endpoint-id`; state folder `.chelabase/`.
- Map order: `ChelaDB`→`Chelabase`, `CHELADB`→`CHELABASE`, `Cheladb`→`Chelabase`, `cheladb`→`chelabase`; the keep list (P2) is masked first.
- Forward commits only; dated specs and plans are never rewritten (D6).
- Key files are copied unchanged, never regenerated or overwritten (CLAUDE.md).
- No compatibility shim for old names (D8).
- Live runs only through `just live <recipe>`, one at a time; implementer worktrees never run the stack.
- Commit identity: `git commit`, with the `Co-Authored-By` and `Claude-Session` trailers; push after every commit.

## Review Focus

1. **Identifiers that only look alike:** a `cheladb` that isn't the product name (the keep list) must survive; T1's fixture covers each entry.
2. **Names built at runtime** (`format!("cheladb_{x}")`, `` `cheladb-${id}` ``, `"CHELADB_" + key`): the substring sweep catches the literals, but a split literal (`"chela" + "db"`) would not. T3 and T4 grep for `chela"` and `chela'` and report any hits.
3. **The two live queues** (old `.cheladb/live` versus new `.chelabase/live`): T8 checks that the old queue is empty and the old stack is stopped before the first new-name `just live`.
4. **Leftover old-name runtime objects:** containers, networks and volumes. T8's gate lists everything Docker has with `cheladb` in it, and only the parked old volumes and stopped old containers may remain.
5. **Generated or locked files edited by hand instead of regenerated:** T4's `just check` stale check and T3's `cargo build --locked` after `Cargo.lock` is regenerated.

## Models and schedule

| Task | Implementer | Review | Wave |
|---|---|---|---|
| T1 rename script + test | sonnet | sonnet (later tasks build on it) | 0 |
| T2 raw sweep | haiku | none (wave 2 builds and the end review catch it) | 1 |
| T3 Rust fix-up | sonnet | none (gated by build/clippy/nextest; end review) | 2 |
| T4 TypeScript + generated | sonnet | none (gated by `just check`; end review) | 2 |
| T5 infra + scripts | sonnet | none (gated by its checks; end review) | 2 |
| T6 living docs + runbook | sonnet | none (docs-only; end review) | 2 |
| T7 forks + repin | opus | none of its own (the user gates each outward step; end review) | 0–2, repin after wave 2 |
| T8 cutover + live verification | opus | none (it is the verification) | 3 |
| T9 end-of-step review, then fix batch | opus review, sonnet fixes | — | 4 |
| T10 folder move + cleanup (after the user's merge) | opus | none | 5 |

Wave 2 tasks each run in their own `just wt-add product-rename-tN product-rename` worktree and are merged back `--no-ff` in the order T3, T4, T5, T6. T7's fork work starts in wave 0, in the fork clones (not in this repo); its repin commit lands on `product-rename` after wave 2 merges.

## Task 1: the rename script

**Files:** create `scripts/rename-chelabase.sh`, `scripts/tests/rename-chelabase.sh`; add recipe `rename-test` to `justfile` (next to `live-test`).

**Interface (produced, used by T2 and in the T10 runbook):**
- `scripts/rename-chelabase.sh [--dry-run]`, run from a repo root. It rewrites file contents, then `git mv`s paths. `--dry-run` prints the files it would change and the moves it would make, and changes nothing. Exit 0 on success, non-zero on any failure. It never commits.
- Content map, as a single perl expression (the T10 runbook reuses it):
  ```bash
  MAP='s/cheladb-v1/\x01A\x01/g; s/cheladb-scaffold/\x01B\x01/g; s{github\.com/ChelaDB}{\x01C\x01}g;
       s/ChelaDB/Chelabase/g; s/CHELADB/CHELABASE/g; s/Cheladb/Chelabase/g; s/cheladb/chelabase/g;
       s/\x01A\x01/cheladb-v1/g; s/\x01B\x01/cheladb-scaffold/g; s/\x01C\x01/github.com\/ChelaDB/g'
  ```
- Files: `git ls-files -z`, minus the P3 exclusions, minus binary files (`grep -Iq .`), restricted to files containing `cheladb` case-insensitively; contents rewritten with `perl -pi -e "$MAP"`.
- Paths: every remaining tracked path containing `cheladb` (P3 exclusions apply) moves to `perl -pe "$MAP"` of itself, with `mkdir -p` of the new parent and `git mv`. Contents are rewritten before paths move.

**Tests (`scripts/tests/rename-chelabase.sh`, same style as `scripts/tests/live.sh`):** each builds a temp git repo fixture, runs the script there and asserts on the result.
- `every_casing_is_mapped`: `ChelaDB`, `CHELADB_MODE`, `Cheladb-User`, `cheladb_admin`, `@cheladb/client`, `cheladb.user_id` all become their Chelabase forms.
- `keep_list_survives`: `chelabase/cheladb-v1`, `<repo root>`, `github.com/ChelaDB` are unchanged, even on a line that also has a renamed token.
- `excluded_trees_untouched`: files under `docs/superpowers/`, `docs/roadmap/`, `openapi/`, both generated dirs, `Cargo.lock` and `pnpm-lock.yaml` keep their bytes.
- `paths_are_moved`: `crates/cheladb-x/src/lib.rs` → `crates/chelabase-x/src/lib.rs` through `git mv` (`git status` shows a rename); a file named `cheladb.toml` moves too.
- `binary_file_untouched`: a file with a NUL byte that contains `cheladb` keeps its bytes.
- `second_run_is_a_no_op`: after one run, a second run leaves `git status --porcelain` and every hash unchanged.
- `dry_run_changes_nothing`: lists the changes, and the fixture tree is byte-identical afterwards.

**Gotchas:** the script excludes itself and its test (both contain the old names on purpose). Use `perl -pi` with `-0777` only if a test shows multi-line matches matter (none should). Use `set -euo pipefail`, and read file lists with `-z` and `mapfile -d ''`.

**Verify:** `just rename-test` (RED shown first: the script missing), `bash -n` on both files. Commit and push.

## Task 2: the raw sweep

**Files:** whatever the script changes. **Consumes:** T1's script.

Steps: `scripts/rename-chelabase.sh --dry-run | tail`, then run it; commit **only** the script's output as one commit `rename: raw sweep cheladb -> chelabase (scripts/rename-chelabase.sh)`, with no hand edits; push. Then record in the Status: files changed, paths moved, and `scripts/rename-chelabase.sh` run a second time shows nothing to change. Don't build or fix anything; wave 2 does that.

## Task 3: Rust fix-up

**Files:** `Cargo.toml`, `Cargo.lock`, `crates/**`, `services/**`, `tools/**` (Rust only; not `*.md`).

- RED: from the post-sweep tree, `cargo build --workspace` / `just test` output showing what fails (or a recorded "clean after sweep", which is also acceptable).
- Regenerate `Cargo.lock` with `cargo update --workspace` (package names only; no version bumps: diff the lockfile and confirm only `cheladb*` → `chelabase*` entries changed). Then `cargo build --workspace --locked`.
- Fix what fails: tests that pin lengths, byte offsets, snapshots or sorted orders affected by the longer name.
- Review Focus 2: `git grep -nE "chela[\"']|\"db\"" -- crates services tools` and report the hits.
- P4: list every crypto or token string that changed (`git grep -nE 'info|aad|issuer|audience|kid' | grep -i chelabase`) in the report.
- **Verify:** `cargo build --workspace --locked`, `cargo clippy --workspace --all-targets -- -D warnings`, `just test` (nextest; the count is recorded in the Status). Commit and push.

## Task 4: TypeScript and generated code

**Files:** `apps/**`, `packages/**`, `pnpm-lock.yaml`, `openapi/**`, the two generated dirs, `package.json` files.

- `pnpm install` (workspace names `@chelabase/*`; respect pnpm's minimum-release-age; no version bumps), then `just api-gen`, then `just check` (includes the generated-code stale check).
- Fix the Studio and SDK tests and typecheck: golden files such as `packages/client/fixtures/gen/*.ts` and `gen-golden.test-d.ts` should come out right from the sweep; regenerate them only if their recipe says so.
- Review Focus 2: `git grep -nE "chela[\"'\`]" -- apps packages`, report the hits.
- **Verify:** `just check`, Studio `pnpm --filter @chelabase/studio test`, `typecheck`, `build`; package tests. Commit and push.

## Task 5: infra and scripts

**Files:** `infra/**` (not `*.md`), `scripts/**` (not the rename script or its test), `justfile`, `.github/**` if present, `.gitignore`.

- `.gitignore` must ignore `.chelabase/` (and keep ignoring `.cheladb/` until T10, so the parked folder is never committed).
- `docker compose -f infra/compose/docker-compose.yml config -q`, `just --list`, `bash -n` on every `scripts/**/*.sh` and `infra/**/*.sh`, `just live-test`, `just neon-test` (offline; it passes against the *current* `NEON_REF` until T7 repins), `just rename-test`.
- Check that `wt-add` links `.chelabase` and that `live.sh` uses `.chelabase/live` and `CHELABASE_LIVE_DIR`.
- **Verify:** the commands above. Commit and push.

## Task 6: living docs and the second-computer runbook

**Files:** `README.md`, `CLAUDE.md`, `infra/**/*.md`, `packages/*/README.md`, `infra/compose/.env.example` (already swept; check it reads right), `docs/roadmap/2026-09-28-neon-and-data-api.md` (current sections only).

- After the sweep, re-read the swept living docs for sense (articles, "a Chelabase"), and fix the fork branch names in CLAUDE.md (`chelabase/simple-query-tags-v2`) and the `REL_17_STABLE_chelabase` mention in `infra/neon/README.md`.
- Roadmap, by hand: add the note "Names before 2026-10-04 use Cheladb (`cheladb`, `CHELADB_*`, `.cheladb/`); see `docs/superpowers/specs/2026-10-04-product-rename-design.md`" under the title. Rename current headings and paragraphs that describe the present (not dated handoff notes).
- In "Resume here", write the **second-computer runbook**:
  1. `git fetch && git pull` on `main`.
  2. Stop the old stack: `docker compose -p cheladb stop`, and stop the computes labelled `cheladb.dev/endpoint-id`.
  3. Copy the key files from `.cheladb/` to `.chelabase/` (the P-list paths).
  4. Rewrite any ignored env file: `perl -pi -e '<MAP>' <file>`.
  5. Rename the fork clones and run `git fetch --prune && git submodule sync --recursive`.
  6. `just zitadel-bootstrap`, `just live just deps-up`, `just live just dev`, one e2e run.
  7. Then `docker compose -p cheladb down -v`.
- **Verify:** `git grep -n -i cheladb -- README.md CLAUDE.md infra packages` shows only keep-list hits. Commit and push.

## Task 7: the forks and the repin

Outward-facing. **Ask the user for one go before step 1** (it covers the PRs, the branch renames, the merges and the re-tag), and report each step as it lands.

1. **`chelabase/postgres`:** `gh api -X POST repos/chelabase/postgres/branches/REL_<N>_STABLE_cheladb/rename -f new_name=REL_<N>_STABLE_chelabase` for N in 17 16 15 14. In the local clone, update `minor_watch.sh`, `minor_watch_test.sh`, `minor-watch.yml` and `CHERRY-PICKS.md` on a branch from `REL_17_STABLE_chelabase`; `minor_watch_test.sh` passes; open a PR into `REL_17_STABLE_chelabase` and merge it.
2. **`chelabase/neon`:** a branch from `main` updating `.gitmodules` (four `branch =` lines; the submodule SHAs unchanged), `FORK.md`, `README.md` and the comments in `.github/scripts/{publish-dev,ci-local,check_tag_free}.sh` and `.github/workflows/images.yml`; PR, merge. The merge commit's sha is the new `NEON_REF`.
3. **`chelabase/rust-postgres`:** rename `cheladb/simple-query-tags-v2` → `chelabase/simple-query-tags-v2` (same API). Delete the old `cheladb/simple-query-tags` branch if it still exists (roadmap leftover).
4. **Re-tag:** list the packages that carry `8ae1b8f227e9-dev` (`gh api /orgs/chelabase/packages?package_type=container`, then their versions). For each, run `docker buildx imagetools create -t ghcr.io/chelabase/<pkg>:<new12>-dev ghcr.io/chelabase/<pkg>:8ae1b8f227e9-dev`, and confirm the digest is unchanged (`docker buildx imagetools inspect`).
5. **Repin, on `product-rename` after wave 2 merges:** `infra/neon/NEON_REF` and both pins in `infra/compose/docker-compose.yml` (`:<new12>-dev@sha256:<same digest>`); add a line under `infra/neon/README.md`'s replay note saying these tags are also re-tags. `just neon-test` must pass. Commit and push.
6. **Local clones:** `mv ChelaDB-neon chelabase-neon`, `mv ChelaDB-postgres chelabase-postgres`; in each, `git fetch --prune`, check out the renamed branch, and in neon `git submodule sync --recursive`.

**Gotchas:** a branch rename doesn't change commit SHAs, so the neon submodule pins stay valid. GitHub redirects old branch names for API reads but not for `git fetch` of the old ref, so the clones must switch to the new branches.

## Task 8: cutover and live verification

Runs in the `product-rename` worktree, after T7's repin. Destructive steps were already approved by D7; still report each one.

1. Check the old queue is empty: `ls <main>/.cheladb/live/queue` (and no holder). Then stop the old stack from the main checkout (`just dev-down`; `docker compose -p cheladb stop`), and stop, without removing, the containers labelled `cheladb.dev/endpoint-id`.
2. In the main checkout, `mkdir -m 700 .chelabase/{secrets,gateway,auth,neon}` and `cp -a` the seven key files from the Status into the same subpaths. Compare checksums against the originals.
3. In this worktree, replace the `.cheladb` symlink with `.chelabase → <main>/.chelabase`.
4. `just live just deps-up`, then `just live just dev` (fresh volumes, migrations, bootstrap; Zitadel's first-instance step recreates the PATs in `.chelabase/zitadel/`).
5. Run through `just live`, one at a time: `neon-smoke`, `neon-test-live`, `neon-e2e`, `branch-e2e`, `data-e2e`, `console-e2e`, `role-e2e`, `auth-e2e`, `app-users-e2e`, `sql-editor-e2e`, `storage-leak-e2e`, and the ignored Rust live tests (`cargo nextest run --run-ignored ignored-only` for the live suites, as their recipes run them).
6. A Playwright pass over Studio: sign in, create a project and a database, open a branch, open the SQL editor, run `select chelabase.user_id()`, and check that the page titles say Chelabase.
7. Leftover gate (Review Focus 4): `docker ps -a`, `docker network ls`, `docker volume ls` filtered on `cheladb`. Only the parked `cheladb_*` volumes and the stopped old containers may remain. Also run `git grep -i cheladb -- . ':!docs/superpowers' ':!docs/roadmap' ':!scripts/rename-chelabase.sh' ':!scripts/tests/rename-chelabase.sh'`, which may show keep-list hits only.

Record every result in the Status. Any failure is fixed on a short `product-rename-fixN` worktree (sonnet) and rerun.

## Task 9: end-of-step review

An opus whole-branch review against the spec and this plan (Review Focus first). Important or Critical findings, plus every deferred minor, are fixed in one batch (sonnet), re-reviewed, and the live runs they affect are repeated. **Pause for the user's merge decision.**

## Task 10: folder move and cleanup (after the user merges)

1. Merge `--no-ff` into `main`, push `main`; `just wt-remove product-rename`; delete the branch on origin and locally.
2. `git clone https://github.com/chelabase/chelabase.git <repo root>`; copy `.chelabase/` (`cp -a`), plus `.claude/settings.local.json` if present, and `.claude/settings.json` (`worktree.bgIsolation: "none"`) as a fallback if it isn't committed yet; then `cp -a <claude project dir> <claude project dir>`.
3. In the new folder: `pnpm install`, `just live just dev` and `just live just neon-e2e`.
4. Only after step 3 passes: `docker compose -p cheladb down -v`, and remove the stopped containers labelled `cheladb.dev/endpoint-id`. Tell the user the old folder (`<repo root>`, which still holds `.cheladb/`) is theirs to delete.
5. Update "Resume here" (done; the runbook stays for the second computer), commit and push `main`.
