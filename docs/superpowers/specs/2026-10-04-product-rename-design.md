# Product rename Cheladb → Chelabase: design

**Status:** design approved in conversation 2026-10-04, part by part (4 parts); written spec approved by the user 2026-10-04.
**Builds on:** the org rename (`d6172fb`, GitHub org and GHCR now `chelabase`) and the fork replay (`f239e346`, `NEON_REF` `8ae1b8f227e9`).
**Branch:** `product-rename` (`.worktrees/product-rename`), from `main` at `8d8f9cca`.

## Goal

Every name the product shows or uses becomes **Chelabase**, in this repo and the public forks. The change lands as forward commits only, so git history keeps the old name. The dev data is wiped by moving to a fresh Compose project. Key material survives unchanged. After the merge, this machine moves to `<repo root>`.

## Decisions (user, 2026-10-04)

| # | Question | Answer |
|---|---|---|
| D1 | Display casing | **Chelabase** (prose, UI titles, type names such as `ChelabaseClient`). The legacy `ChelaDB` spelling also becomes `Chelabase`. |
| D2 | CLI binary | **`chelabase`** (`chelabase dev …`, `chelabase gen types`). No alias. |
| D3 | Env prefix | **`CHELABASE_`** (1:1 for all ~105 `CHELADB_*` variables). |
| D4 | In-database names | Schema **`chelabase`** (`chelabase.user_id()`, `chelabase.claims()`, …), roles **`chelabase_*`** (`chelabase_admin`, `chelabase_auth_sync`, `chelabase_live_probe`), settings **`chelabase.*`**. |
| D5 | Fork scope | **Docs, scripts and branch names**: `REL_{17,16,15,14}_STABLE_chelabase`, `chelabase/simple-query-tags-v2`; new `NEON_REF`, images re-tagged with the same digests. |
| D6 | Docs scope | **Living docs only.** Dated specs and plans (`docs/superpowers/specs/**`, `docs/superpowers/plans/**`) and the roadmap's dated handoff notes stay as history; the roadmap gets a note that names before 2026-10-04 use Cheladb. |
| D7 | Wipe | Fresh `chelabase` Compose volumes. The old `cheladb_*` volumes stay until `just dev` and an e2e pass from the new folder, then they are removed. Key files are copied to `.chelabase/` unchanged. |
| D8 | Approach | **A committed, scripted sweep**, then parallel fix-up by area. No compatibility shim for old names. |

**Rulings (mechanical, listed to the user):**
- R1. Crates `chelabase-*` (directories and package names), npm scope `@chelabase/*`, headers `Chelabase-User` / `Chelabase-Identity`, Compose project `chelabase` (so its volumes, network `chelabase_default` and containers follow), container prefix `chelabase-compute-`, Docker label `chelabase.dev/endpoint-id`, Zitadel app names (`chelabase-studio`, …), metadata database `chelabase_metadata`, state folder `.chelabase/`.
- R2. The 18 metadata migrations are **edited in place**, not given a rename migration. This is safe only because D7 gives every database fresh volumes; a machine that keeps its old metadata volume would see checksum mismatches.
- R3. Generated files are regenerated, never swept: `openapi/*.json` and the orval clients (`just api-gen`), `Cargo.lock`, `pnpm-lock.yaml`.
- R4. Old names that refer to things that really still exist keep their spelling: `cheladb-v1` (archived repo), the `cheladb-scaffold` folder (until the folder move), and the `github.com/ChelaDB` placeholder-org mention. (The `-pre-replay` repo names contain no `cheladb`; plan ruling P2.)

## Part 1: the rename script

`scripts/rename-chelabase.sh`, committed and **idempotent** (a second run changes nothing).

- **Content map**, applied in this order to every tracked text file not excluded below: `ChelaDB`→`Chelabase`, `CHELADB`→`CHELABASE`, `Cheladb`→`Chelabase`, `cheladb`→`chelabase`. Plain substring matching covers `cheladb_admin`, `cheladb-neon`, `@cheladb/client`, `Cheladb-User`, `cheladb.user_id` and `CHELADB_MODE`.
- **Keep list (R4):** these patterns are masked before the map and restored after it: `cheladb-v1`, `cheladb-scaffold`, `github.com/ChelaDB`.
- **Excluded files:** `docs/superpowers/specs/**`, `docs/superpowers/plans/**` (this step's own spec and plan included, since they name the old spellings on purpose; plan ruling P3), the script and its test fixtures, `docs/roadmap/**` (edited by hand, D6), `Cargo.lock`, `pnpm-lock.yaml`, `openapi/**` and the orval output directories (R3), and binary files.
- **Path renames:** `git mv` every tracked path whose name contains `cheladb`, outside the excluded trees (9 crate directories plus any files).
- **Test first:** a fixture tree covering each casing, each keep-list entry, each exclusion, a path rename and a binary file; the test asserts the exact output and that a second run makes no change.

## Part 2: fix-up and verification

All on `product-rename`.

- **Wave 1:** run the script; commit the raw sweep alone, so reviewers can tell the mechanical diff apart from the hand fixes.
- **Wave 2**, in parallel on disjoint files (each in its own `just wt-add product-rename-tN product-rename` worktree, merged back `--no-ff`):
  - **Rust:** `cargo build`, clippy `-D warnings`, `just test`. Fix tests that pin lengths or literals.
  - **TypeScript:** `pnpm install` (lockfile), `just api-gen` plus the stale check, Studio typecheck, tests and build, SDK package tests.
  - **Infra and scripts:** `docker compose config`, `just --list`, `bash -n` on every shell script, the offline `just neon-test`.
  - **Living docs (by hand):** README, CLAUDE.md, the roadmap's current sections plus the D6 note, infra READMEs, `.env.example`.
- **Leftover gate:** `git grep -i cheladb` outside the excluded trees returns only keep-list hits.
- **TDD:** the script follows Part 1's test. For the sweep, RED means the existing suites fail after the raw sweep, and GREEN means they pass after the fix-up.
- **Review:** the script gets its own review, since everything builds on it. The rest waits for the opus end-of-step whole-branch review.

## Part 3: the forks

Outward-facing: each action happens at a task boundary after the user says go, through PRs like the org rename's.

- **`chelabase/postgres`:** rename the branches `REL_{17,16,15,14}_STABLE_cheladb` → `REL_*_STABLE_chelabase` through GitHub's branch-rename API, which keeps redirects and moves the default branch and open PRs. A PR on `REL_17_STABLE_chelabase` updates `minor_watch.sh`, `minor_watch_test.sh`, `minor-watch.yml` and `CHERRY-PICKS.md`; `minor_watch_test.sh` must pass. The neon submodule pins keep their current SHAs, which stay ancestors of the renamed branches, so the build inputs don't change.
- **`chelabase/neon`:** one PR updating the four `branch =` lines in `.gitmodules`, `FORK.md`, `README.md` and the comments in `publish-dev.sh`, `ci-local.sh`, `check_tag_free.sh` and `images.yml`. Its merge commit is the new `NEON_REF`.
- **Images:** re-tag the five GHCR packages to `<new sha12>-dev` with the same digests, as in the 2026-10-04 replay; nothing is rebuilt. As then, the binaries still report the old git version.
- **`chelabase/rust-postgres`:** rename `cheladb/simple-query-tags-v2` → `chelabase/simple-query-tags-v2`. Cargo pins `rev = d20c5b3…`, so only the docs change.
- **This repo:** bump `NEON_REF` and the image tags on `product-rename`; `just neon-test` must pass, and the new images are pulled in Part 4's live run.
- **Local fork clones:** rename `ChelaDB-neon` → `chelabase-neon` and `ChelaDB-postgres` → `chelabase-postgres`, then `git fetch --prune` and `git submodule sync --recursive`.

## Part 4: cutover and the folder move

**Ignored state (before the first live run):**
1. Stop the old stack: `docker compose -p cheladb stop` (volumes kept) and `just dev-down` from the main checkout.
2. In the main checkout, `cp -a` only the key material from `.cheladb/` to `.chelabase/`: `secrets/` (`secrets.key`), `gateway/` (`internal-signing.pem`, `internal-verify.pem`), `auth/` (`app-jwt.key`, `app-jwt.pub`, `cookie.key`) and `neon/` (`compute-jwt.pem`). The plan pins any other key files found. `.cheladb/` stays for rollback.
3. `zitadel/`, `dev/` and `live/` are not copied: bootstrap and the live queue recreate them against the wiped Zitadel.
4. Ignored env files holding `CHELADB_*` (`apps/studio/.env.local`, plus `infra/compose/.env` if present) are copied, then rewritten with the script's map (none exist on this machine; plan ruling P1).

**Live verification**, queued through `just live` from the `product-rename` worktree, one at a time:
- `just dev` (fresh volumes, migrations, `zitadel-bootstrap`), then `neon-e2e`, `data-e2e`, `auth-e2e`, `app-users-e2e`, `branch-e2e`, `storage-leak-e2e` and the ignored live tests.
- A Playwright pass over Studio: login, projects, databases, the SQL editor.
- A check that no running container, network or volume uses the `cheladb` name, apart from the parked old volumes.

**Merge:** end-of-step review (opus), fixes, then the user's pause. After the merge: push `main`, delete `product-rename` on origin and locally.

**Folder move (after the merge):**
1. Fresh clone into `<repo root>`.
2. Copy `.chelabase/`, `apps/studio/.env.local` and the local `.claude/` settings.
3. Copy `<claude project dir>/` to `<project>/`.
4. Rebuild `node_modules` and `target`; run `just dev` and one e2e from the new folder.
5. Only after that run passes: `docker compose -p cheladb down -v` and remove leftover `cheladb-compute-*` containers. The old folder is the user's to delete.

**Second computer:** the roadmap's "Resume here" gets a runbook. Pull; copy the `.cheladb` keys to `.chelabase`; stop, and later remove, the old Compose project; rename and sync the fork clones; run `just zitadel-bootstrap` and `just deps-up`.

## Out of scope

- Rewriting dated specs, plans or git history (D6, forward commits only).
- Renaming the Neon/Postgres internals or image names (`neon-storage`, `neon-compute-v17`), which carry no product name.
- Publishing crates or npm packages under the new names.
