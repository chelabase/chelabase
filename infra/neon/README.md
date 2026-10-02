# Neon images

Cheladb runs Neon's storage stack (pageserver, safekeepers, storage broker, storage
controller, proxy) and a compute image. Both come from our hard fork,
[ChelaDB/neon](https://github.com/ChelaDB/neon), at the commit in `NEON_REF`
(`39b641053212a25645384df1eabd843a5d117b69`). Design: `docs/superpowers/specs/` and
`docs/superpowers/plans/2026-10-01-neon-fork.md`.

## Pull (default)

`just deps-up` pulls `ghcr.io/cheladb/neon-storage` and `ghcr.io/cheladb/neon-compute-v17`
(the fork's dev images, tagged `<sha12>-dev`), pinned by digest in `infra/compose/shared.yml`
(the storage pin is repeated in `slot.yml`, for the proxy). The packages are public on ghcr.io, so
pulling needs no login. (Pushing from a fork checkout needs `docker login ghcr.io` with a
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

## Bump `NEON_REF`

1. Merge the change in `ChelaDB/neon` (`main`).
2. In a fork checkout, run `.github/scripts/publish-dev.sh`. It builds the dev images, pushes
   them to GHCR and prints the digests. (Publishing is by hand: pushes to `main` publish
   nothing, and cheladb-v1 has no copy of the script.)
3. Put the full commit sha in `infra/neon/NEON_REF`, and paste the new tags and digests into
   `infra/compose/shared.yml` (both images) and `infra/compose/slot.yml` (storage).
4. `just neon-test` (it checks the ref exists on the fork and that both Compose files carry
   `:<sha12>-dev@sha256:`), then `just stack-test`.
5. Run the e2e scripts in your slot (`just neon-e2e`, `just branch-e2e`, `just data-e2e`,
   `just console-e2e`, `just role-e2e` and `just auth-e2e`), then `just stack-migrate` (when no other slot is running)
   to move the shared tier's storage onto the new pins.

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
