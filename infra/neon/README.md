# Neon images

Cheladb runs Neon's storage stack (pageserver, safekeepers, storage broker, storage
controller) and, from roadmap step 4, its compute image. Both come from
[neondatabase/neon](https://github.com/neondatabase/neon) at the commit in `NEON_REF`.

## Pull (default)

`just deps-up` pulls `ghcr.io/cheladb/neon-storage` and `ghcr.io/cheladb/neon-compute-v17` (the
fork's dev images, `532981ad7de5-dev`), pinned by digest in `infra/compose/shared.yml` (the
storage pin is repeated in `slot.yml`, for the proxy). **The packages are private:** run
`docker login ghcr.io` first (a token with `read:packages`).

The compute runs Postgres 17.5. The worker's login-role check revokes `neon_superuser` once
per grantor (`REVOKE … GRANTED BY <grantor>`), which needs Postgres 16 or later.

## Build from source

`just neon-build` shallow-clones Neon at `NEON_REF` into `infra/neon/.cache/` and builds
`cheladb/neon:<tag>` and `cheladb/compute-node-v17:<tag>` (`<tag>` = first 12 chars of
`NEON_REF`). The dev tag format (e.g., `b2402faa2394-dev`) indicates a development-mode
image published from the Neon fork. The first build needs about 40 GB of free disk and about 40 minutes. When
those local images exist, `just deps-up` uses them instead of pulling.

Reclaim the build cache afterwards with `docker builder prune -f --keep-storage 5GB`.

## Recipes

| Recipe | Does |
|---|---|
| `just neon-build` | build both images from source |
| `just deps-up` | start the storage stack (local images if present, else pull) |
| `just neon-ps` | show its containers |
| `just neon-smoke` | check it: safekeeper Active, pageserver up, create/delete a tenant |
| `just deps-down` | stop it (volumes kept) |
| `just neon-test` | offline checks of these scripts and the Compose file |

The stack starts with `just deps-up` and backs every database. `just down` also stops it.
