# Neon images

Chelabase runs Neon's storage stack (pageserver, safekeepers, storage broker, storage
controller, proxy) and a compute image. Both come from our hard fork,
[chelabase/neon](https://github.com/chelabase/neon), at the commit in `NEON_REF`
(`956332507a7be94a00db47f0813412e1fa6d5b0d`). Design: `docs/superpowers/specs/` and
`docs/superpowers/plans/2026-10-01-neon-fork.md`.

**Round 2:** `NEON_REF` pins fork `main` at `c8d1bb68b`. Shipped: detach waits for each ancestor's WAL
and multi-level detach copies every ancestor's layers; multixact redo writes the next offset like
Postgres; the worker reads the safekeepers' `commit_lsn` over HTTP (`safekeeper1:7676` on the
stack network; `CHELABASE_SAFEKEEPER_HTTP`) for a branch's head; reads at kept LSNs
(patch 4: an LSN below the GC cutoff is readable and leasable when a child's branch point or a lease
keeps it exactly, and branch create at such a point takes a short lease); DDL forwarding of role
attributes (patch 5: LOGIN and VALID UNTIL are forwarded; other attributes and GRANT trigger a
delayed per-branch resync). Published 2026-10-08 as `c8d1bb68b81f-dev` (pinned by digest in
`infra/compose/docker-compose.yml`).

**Rollback hazard (round 2):** once the new pageserver ingests multixact `CREATE_ID` records, stored
layers contain the new `NeonWalRecord::MultixactOffsetCreateWithNext` variant (enum index 8). Rolling
the pageserver image back to a pin from before round 2 (`956332507a7b` or older) makes those pages
unreadable. Never roll the pageserver back to a pre-round-2 pin once a round-2 image has run.

**Round 3 (published 2026-10-15, `629166ceef60-dev`):** `NEON_REF` pins `chelabase/neon` `629166ceef600938559ff39a1a95a7217e2cc0d2` (Postgres v17 at `chelabase/postgres` `76ce76278c5`); the images are pinned by digest in `infra/compose/docker-compose.yml`, built on the build-tools image `ghcr.io/chelabase/neon-build-tools:328cc68f1c26`. `chelabase_keys` is a logical decoding output plugin for the realtime feed in the Postgres fork (`contrib/chelabase_keys`). The `chelabase_realtime` role is restricted to that feed by the fork, identified by role name. Member roles (`cb_m_*`) can be changed only by a superuser and log in to a compute only from the orchestrator, by source address and client certificate. `REASSIGN OWNED` fires the DDL event triggers, so the protected-branch guard covers ownership moves. On a protected branch, a database a non-superuser creates is born closed and opens once the guard is installed. compute_ctl applies its migrations before it reports `running`, and the worker waits for `running` as well as `SELECT 1`. The Rust toolchain moves to 1.99.0 and the AWS SDK crates are upgraded, which takes rustls 0.21 out of the tree; the S3 client keeps the SDK's default flexible checksums. Published images will be tagged `<sha12>-dev`, where `<sha12>` is the first 12 characters of the fork commit.

**Round 4 (published 2026-10-22, `0741b56deb67-dev`):** `NEON_REF` pins `chelabase/neon` `0741b56deb67f3a2312e16f4551b625227ffeeb3` (Postgres v17 at `chelabase/postgres` `7812d66fe85`). A temporary replication slot no longer leaves its `neon-file:` copy behind after a restart (U16). `chelabase_keys` streams large transactions instead of spilling them to disk (W9). compute_ctl keeps its secrets in token files, and its internal API listens on loopback only behind a per-start secret. Storage tokens for capable computes expire after 24 h, and the worker refreshes them hourly without restarting the compute. compute_ctl's external `/metrics` and `/autoscaling_metrics` require a token with the read-only `metrics` scope. The pageserver, safekeeper and storage-controller `/metrics` and profile routes need a token. Their routes check the token's scope before they read the request, and control-plane routes (tenant configuration, location config, safekeeper timeline creation and the like) refuse tenant tokens. The proxy reads its Valkey password from a file instead of argv, and its control-plane token from a file (`--control-plane-token-file`), so neither is in its environment or argv. On PG14 and PG15, `COMMENT ON ROLE` and `SECURITY LABEL ON ROLE` run the managed-role guard too. Logins as `chelabase_realtime` are accepted only from realtime's address and with its client certificate. The storage controller retries serialization failures with backoff. Personal logins carry a role marker that the managed-role guard enforces. Removed extensions (pgrag from the published images; the others only from the unpublished `extensions-all` variant): pgjwt, pgrag (`rag`, `rag_bge_small_en_v15`, `rag_jina_reranker_v1_tiny_en`), pg_tiktoken, pg_mooncake, and the v14–v16 build of pgx_ulid 0.1.5. Bumped to their latest releases: timescaledb 2.31.0, pg_partman 5.5.0, pg_cron 1.6.8, pgbouncer 1.26.1, and the exporters.

## Pull (default)

`just workbench up` pulls `ghcr.io/chelabase/neon-storage` and `ghcr.io/chelabase/neon-compute-v17`
(the fork's dev images, tagged `<sha12>-dev`), pinned by digest in `infra/compose/docker-compose.yml`.
The packages are public on ghcr.io, so pulling needs no login. (Pushing from a fork checkout needs `docker login ghcr.io` with a
token that has `write:packages`.)

The old `ghcr.io/chelabase/neon` and `compute-node-v17` packages hold images built from
upstream Neon; they stay for old pins and are no longer used.

## Build from source

`just neon-build` shallow-clones the fork at `NEON_REF` into `infra/neon/.cache/` and builds
`chelabase/neon-storage:<tag>` and `chelabase/neon-compute-v17:<tag>`; `<tag>` is the first 12
chars of `NEON_REF`, plus `-dev` for the dev variant. The first build needs about 40 GB of
free disk and about 40 minutes. When those local images exist, `just workbench up` uses them
instead of pulling. Reclaim the build cache afterwards with
`docker builder prune -f --keep-storage 5GB`.

`VARIANT` picks what is built (default `dev`):

| `VARIANT` | Storage image | Compute image | Tags |
|---|---|---|---|
| `dev` | `PG_VERSIONS=v17` | `EXTENSIONS=minimal` | `<sha12>-dev` |
| `platform` | every Postgres version | `EXTENSIONS=all` | `<sha12>` |

`EXTENSIONS=<set>` overrides the compute's extension set for either variant. Only the dev
tag is picked up by `just workbench up` (`scripts/neon-images.sh`).

## Safekeeper timelines

The storage controller runs with `--timelines-onto-safekeepers --timeline-safekeeper-count=1`, so it
creates each safekeeper timeline and deletes it (local WAL and the S3 backup) with the database,
branch or superseded timeline; `just storage-leak-e2e` checks this. Databases whose timelines ALL
predate these flags still leak (a tenant with any controller-managed timeline gets a tenant-wide
safekeeper delete that removes its old timelines too): another machine needs the same L8 (the Neon
storage reset) in `docs/superpowers/plans/2026-10-03-live-queue.md` to reclaim them.

Each deleted database used to leave its `timelines` rows and a tenant `Delete` pending op in the
storage controller's database (a generation mismatch in `tenant_delete_safekeepers`). The fix
landed in chelabase/neon-pre-replay#25 (https://github.com/chelabase/neon-pre-replay/pull/25, `NEON_REF` `8ae1b8f227e9`);
`just storage-leak-e2e` checks that the controller forgets a deleted database's tenant.

## Bump `NEON_REF`

> **Tag names after the 2026-10-04 history replay:** images published before that date carry pre-replay fork SHAs in their tags (e.g. `39b641053212-dev`), and those tags are kept as they are. The earlier `8ae1b8f227e9-dev` images are re-tags of the `2d2a6acd7be2-dev` builds (same digests), so their binaries may still report the old `GIT_VERSION`. The next real bump publishes images built from a post-replay commit.
>
> The `956332507a7b-dev` tags are re-tags of the same digests (made with `docker buildx imagetools create --prefer-index=false`).

1. Build once and test: in a fork checkout on the round branch, run
   `.github/scripts/publish-dev.sh --no-push --allow-unpushed`, re-tag the two
   `ghcr.io/chelabase/neon-*:<sha12>-dev` images as `chelabase/neon-*:<sha12>-dev` (what
   `scripts/neon-images.sh` picks up), and run the live suites on them.
2. Fast-forward `chelabase/neon` `main` to that commit (same SHA, no merge commit).
3. Run `.github/scripts/publish-dev.sh --push-only`. It pushes those exact local images without
   rebuilding (refusing unless each image's `org.opencontainers.image.revision` label equals
   HEAD) and prints the digests. (Publishing is by hand: pushes to `main` publish nothing, and
   chelabase has no copy of the script.) Plain `publish-dev.sh` builds and pushes in one go.
4. Put the full commit sha in `infra/neon/NEON_REF`, and paste the new tags and digests into
   `infra/compose/docker-compose.yml` (both images).
5. `just neon-test` (it checks the ref exists on the fork and that both pins carry
   `:<sha12>-dev@sha256:`).
6. Move your checkout's stack onto the new pins with `just workbench up`, then run the e2e
   recipes on it (`just neon-e2e`, and likewise `branch-e2e`, `data-e2e`, `console-e2e`,
   `role-e2e`, `auth-e2e` and `storage-leak-e2e`).

The compute image runs Postgres 17.11 (the fork's `REL_17_STABLE_chelabase`).

## Minors and upstream

Upstream Neon releases and the Postgres minors flow into the fork through its minor watcher
and the process in
[`FORK.md`](https://github.com/chelabase/neon/blob/main/FORK.md). The fork carries our
patches; chelabase only moves `NEON_REF` forward as above.

## Recipes

| Recipe | Does |
|---|---|
| `just neon-build` | build both images from source (`VARIANT=dev` by default) |
| `just workbench up` | start this checkout's stack, storage included (local images if present, else pull) |
| `just workbench status` | every stack, running or stopped |
| `just neon-smoke` | check it: safekeeper Active, pageserver up, create/delete a tenant |
| `just workbench down` | stop it (volumes kept) |
| `just neon-test` | offline checks of these scripts and the Compose files |

The storage stack is part of every checkout's stack and backs every database there.
