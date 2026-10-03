# Neon images

Cheladb runs Neon's storage stack (pageserver, safekeepers, storage broker, storage
controller, proxy) and a compute image. Both come from our hard fork,
[ChelaDB/neon](https://github.com/ChelaDB/neon), at the commit in `NEON_REF`
(`2d2a6acd7be2c82c652d83d3276bfa1ed5d57292`). Design: `docs/superpowers/specs/` and
`docs/superpowers/plans/2026-10-01-neon-fork.md`.

## Pull (default)

`just deps-up` pulls `ghcr.io/cheladb/neon-storage` and `ghcr.io/cheladb/neon-compute-v17`
(the fork's dev images, tagged `<sha12>-dev`), pinned by digest in `infra/compose/docker-compose.yml`.
The packages are public on ghcr.io, so pulling needs no login. (Pushing from a fork checkout needs `docker login ghcr.io` with a
token that has `write:packages`.)

The old `ghcr.io/cheladb/neon` and `compute-node-v17` packages hold images built from
upstream Neon; they stay for old pins and are no longer used.

## Build from source

`just neon-build` shallow-clones the fork at `NEON_REF` into `infra/neon/.cache/` and builds
`cheladb/neon-storage:<tag>` and `cheladb/neon-compute-v17:<tag>`; `<tag>` is the first 12
chars of `NEON_REF`, plus `-dev` for the dev variant. The first build needs about 40 GB of
free disk and about 40 minutes. When those local images exist, `just deps-up` uses them
instead of pulling. Reclaim the build cache afterwards with
`docker builder prune -f --keep-storage 5GB`.

`VARIANT` picks what is built (default `dev`):

| `VARIANT` | Storage image | Compute image | Tags |
|---|---|---|---|
| `dev` | `PG_VERSIONS=v17` | `EXTENSIONS=minimal` | `<sha12>-dev` |
| `platform` | every Postgres version | `EXTENSIONS=all` | `<sha12>` |

`EXTENSIONS=<set>` overrides the compute's extension set for either variant. Only the dev
tag is picked up by `just deps-up`.

## Safekeeper timelines

The storage controller runs with `--timelines-onto-safekeepers --timeline-safekeeper-count=1`, so it
creates each safekeeper timeline and deletes it (local WAL and the S3 backup) with the database,
branch or superseded timeline; `just storage-leak-e2e` checks this. Databases whose timelines ALL
predate these flags still leak (a tenant with any controller-managed timeline gets a tenant-wide
safekeeper delete that removes its old timelines too): another machine needs the same L8 (the Neon
storage reset) in `docs/superpowers/plans/2026-10-03-live-queue.md` to reclaim them.

Each deleted database used to leave its `timelines` rows and a tenant `Delete` pending op in the
storage controller's database (a generation mismatch in `tenant_delete_safekeepers`). The fix
landed in ChelaDB/neon#25 (https://github.com/ChelaDB/neon/pull/25, `NEON_REF` `2d2a6acd7be2`);
`just storage-leak-e2e` checks that the controller forgets a deleted database's tenant.

## Bump `NEON_REF`

1. Merge the change in `ChelaDB/neon` (`main`).
2. In a fork checkout, run `.github/scripts/publish-dev.sh`. It builds the dev images, pushes
   them to GHCR and prints the digests. (Publishing is by hand: pushes to `main` publish
   nothing, and cheladb-v1 has no copy of the script.)
3. Put the full commit sha in `infra/neon/NEON_REF`, and paste the new tags and digests into
   `infra/compose/docker-compose.yml` (both images).
4. `just neon-test` (it checks the ref exists on the fork and that both pins carry
   `:<sha12>-dev@sha256:`).
5. Move the stack onto the new pins with `just live just deps-up`, then run the e2e scripts
   through the queue (`just live just neon-e2e`, and likewise `branch-e2e`, `data-e2e`,
   `console-e2e`, `role-e2e`, `auth-e2e` and `storage-leak-e2e`).

The compute image runs Postgres 17.11 (the fork's `REL_17_STABLE_cheladb`).

## Minors and upstream

Upstream Neon releases and the Postgres minors flow into the fork through its minor watcher
and the process in
[`FORK.md`](https://github.com/ChelaDB/neon/blob/main/FORK.md). The fork carries our
patches; cheladb-v1 only moves `NEON_REF` forward as above.

## Recipes

| Recipe | Does |
|---|---|
| `just neon-build` | build both images from source (`VARIANT=dev` by default) |
| `just deps-up` | start the storage stack (local images if present, else pull) |
| `just neon-ps` | show its containers |
| `just neon-smoke` | check it: safekeeper Active, pageserver up, create/delete a tenant |
| `just deps-down` | stop it (volumes kept) |
| `just neon-test` | offline checks of these scripts and the Compose files |

The stack starts with `just deps-up` and backs every database. `just down` also stops it.
