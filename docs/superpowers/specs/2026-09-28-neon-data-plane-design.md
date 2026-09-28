# Neon data plane in Compose (roadmap step 1)

**Date:** 2026-09-28
**Status:** Design approved in conversation; spec awaiting review.
**Roadmap:** step 1 of `docs/roadmap/2026-09-28-neon-and-data-api.md`.

## Goal

Run Neon's storage stack (object storage, storage broker, safekeeper, storage controller,
pageserver) next to the existing dev services, from pinned images, and prove it can create
and delete a tenant with a timeline. Nothing in the control plane uses it yet: provisioning
stays on plain `postgres:17` containers until step 4.

## Decisions

| Question | Decision |
|---|---|
| Where the images come from | Pull `ghcr.io/cheladb/{neon,compute-node-v17}:0.1.0`, pinned by digest. A local build from `just neon-build` wins when it exists. |
| GHCR visibility | The packages stay **private**. A failed pull stops with a message: log in, or build locally. |
| Always on or opt-in | **Opt-in until step 4.** The services carry `profiles: ["neon"]`; `just deps-up` / `just dev` don't start them. Step 4 removes the profile lines. |
| Compose layout | The **same** `infra/compose/docker-compose.yml`, so it stays one project on one network and `down` stops everything. |
| Credentials | Fixed local-dev values inline, like `metadata-db` and Zitadel. No `generate-secrets.sh`. |

## What is ported from the old repo (`<repo root>`)

| Old path | New path | Trimmed |
|---|---|---|
| `infra/neon/NEON_REF` | `infra/neon/NEON_REF` | unchanged (`fa504217c61bbcaf5c512d75830564541f917f8f`) |
| `infra/neon/build.sh` | `infra/neon/build.sh` | targets `neon` and `compute-node` only; `vm-compute` goes |
| `infra/neon/lib/tags.sh` | `infra/neon/lib/tags.sh` | `neon_ref` and `neon_tag` only; no `AUTOSCALING_REF`, `neonvm_tag` or `release_images` |
| `infra/neon/patches/README.md` | `infra/neon/patches/README.md` | rewritten for Neon patches; no patch files yet |
| `infra/neon/tests/run.sh` | `infra/neon/tests/run.sh` | only the checks that still apply, plus the Compose pin check below |
| `core/docker/docker-compose.yml` (storage services) | `infra/compose/docker-compose.yml` | see "Compose services" |

Not ported: `build-neonvm.sh`, `AUTOSCALING_REF`, `patches/autoscaling/`, `push.sh`,
`RELEASE.md`, `moon.yml`, the lint/fmt justfile, and the old Compose services for the
control plane, identity, gateway, studio, proxy, proxy TLS, docker-socket-proxy and the
compute image. The proxy and its TLS come in step 3; docker-socket-proxy comes with the
compute backend in step 4.

## Layout

```
infra/neon/
  NEON_REF            pinned upstream commit
  build.sh            build.sh neon | compute-node
  lib/tags.sh         neon_ref, neon_tag (first 12 chars of NEON_REF)
  patches/README.md   how a patch on top of NEON_REF is added (none yet)
  tests/run.sh        offline checks, no Docker images or network
  README.md           pull vs build, private GHCR packages, disk needs
scripts/
  neon-images.sh      picks local or pinned images (sourced by just neon-up)
  neon-smoke.sh       checks the running stack
```

`.gitignore` gains `infra/neon/.cache/` (the Neon source checkout for builds).

## Images

- The digest-pinned refs are the defaults in Compose itself, so the pin lives in one place:

  ```yaml
  x-neon-images:
    neon: &neon-image ${NEON_IMAGE:-ghcr.io/cheladb/neon:0.1.0@sha256:c84e9c991957184bf7aa88c70b3e1657720812ef0800226753d3ce95c8860f8a}
  ```

  `ghcr.io/cheladb/compute-node-v17:0.1.0@sha256:7a39900c5f59ba428ecee67658fdf679706d556a53d149b237a0e9bbfa361a93`
  is recorded the same way (`COMPUTE_NODE_IMAGE`), for step 4. Defaults (`:-`) are used, not
  `:?`, because Compose interpolates every service, including inactive profiles, so a
  required variable would break plain `just deps-up`.
- `scripts/neon-images.sh`:
  - sources `infra/neon/lib/tags.sh`;
  - if `cheladb/neon:<neon_tag>` exists locally, exports `NEON_IMAGE` pointing at it
    (likewise `COMPUTE_NODE_IMAGE` for `cheladb/compute-node-v17:<neon_tag>`), and says so;
  - otherwise pulls the pinned refs. On failure it exits non-zero with:
    `can't pull ghcr.io/cheladb/neon (docker's error is above). The packages are private: run
    docker login ghcr.io, or build locally with just neon-build (~40 min, ~40 GB free disk).`
- The compute image is pulled now so step 4 finds it; nothing runs it in step 1.

## Compose services

All carry `profiles: ["neon"]`. Flags and health checks are carried over from the old repo
(which checked them against the pinned Neon source); only hostnames, the hook URL,
credentials and volume names change.

| Service | Image | Host port | Role |
|---|---|---|---|
| `seaweedfs` | `chrislusf/seaweedfs:4.47` | none | S3 on :8333 for pageserver layers and safekeeper WAL |
| `create-bucket` | neon | none | one-shot: creates `s3://neon` |
| `storage-broker` | neon | none | :50051 |
| `safekeeper1` | neon | none | one safekeeper, WAL offloaded to S3 |
| `storcon-db` | `postgres:17-alpine` | none | the storage controller's database |
| `storage-controller` | neon | `127.0.0.1:1234` | `--dev` (no JWT between storage components) |
| `pageserver` | neon | `127.0.0.1:9898` | HTTP only; its pg port (6400) stays internal |
| `register-safekeepers` | neon | none | one-shot: registers safekeeper 1 and marks it Active |
| `storcon-hook` | `hashicorp/http-echo:1.0.0` | none | answers the storage controller's hook with 200 until step 3 |

- **Why the two host ports:** the orchestrator and worker run on the host, not in Compose.
  The step 2 storcon client and the step 4 provisioning reach the storage controller and
  pageserver on `127.0.0.1`. Nothing binds to `0.0.0.0`.
- **Storage-controller hook:** `--control-plane-url=http://storcon-hook:5678/storcon` and
  `--control-plane-jwt-token=${CHELADB_STORCON_HOOK_TOKEN:-cheladb-dev-storcon-hook}`. Every tenant create is an attachment
  change, so the controller calls `notify-attach` each time. With nothing answering, each
  create retries for ~15 s and leaves the tenant's reconcile in error (found in the final
  review). So `storcon-hook` (`hashicorp/http-echo`, pinned by digest, profile `neon`)
  answers 200 to everything until step 3, which points the flag at the control plane's
  internal listener on the host (`host.docker.internal:8083`, with
  `extra_hosts: host-gateway`; Zitadel already has 8080).
- **Credentials (local dev only, commented as such):**
  - S3: `cheladb-dev` / `cheladb-dev-secret`, shared by seaweedfs, create-bucket, the
    safekeeper and the pageserver through one `x-neon-s3-env` anchor.
  - storcon-db: user `storcon`, password `storcon`, database `storage_controller`.
  - The hook token is also added to `infra/compose/.env.example` (commented, for step 3).
- **Volumes:** `neon_seaweedfs_data`, `neon_pageserver_data`, `neon_safekeeper1_data`,
  `neon_storcon_pg_data`.
- **Network:** the project's default network (`cheladb_default`). Step 4's computes will join it.

## Just recipes

| Recipe | Does |
|---|---|
| `neon-build` | `infra/neon/build.sh neon` then `compute-node` |
| `neon-up` | exports what `scripts/neon-images.sh` prints, `docker compose --profile neon up -d --wait` on the long-running services (`--wait` fails on an exited one-shot), then `docker compose run --rm register-safekeepers` |
| `neon-down` | stops and removes only the profile's containers (volumes kept) |
| `neon-ps` | `docker compose --profile neon ps -a` (shows the one-shots' exit status) |
| `neon-test` | `infra/neon/tests/run.sh` |
| `neon-smoke` | `scripts/neon-smoke.sh` |

`deps-down` (and so `down` and `stop`) passes `--profile neon`, so it also stops the storage
stack if it's running. `infra/compose/moon.yml`'s `down` task gets the same flag.

## Verification

**`just neon-test`** (offline):
- `NEON_REF` is a full 40-char sha, and `neon_tag` is its first 12 chars.
- `build.sh` with no target or an unknown one (including `vm-compute`) exits 2 with usage.
- The `NEON_IMAGE` and `COMPUTE_NODE_IMAGE` defaults in the Compose file are pinned by
  `@sha256:` digest.

**`just neon-smoke`** (against the running stack; all calls to `127.0.0.1:1234` unless noted):
1. `GET /control/v1/safekeeper` lists safekeeper 1 with `scheduling_policy` `Active`.
2. `GET 127.0.0.1:9898/v1/status` answers 200.
3. `POST /v1/tenant` with a random tenant id and `placement_policy: {"Attached": 0}`.
4. `GET /control/v1/tenant/{id}`: the shard's compute notification isn't pending (the hook
   answered). Then `POST /v1/tenant/{id}/timeline` with a random timeline id and `pg_version: 17`, then
   `GET` it back.
5. `DELETE` the timeline, then the tenant. Deletion is asynchronous: repeat while the
   answer is 202, until 200 or 404 (at most 60 s).
6. On any failure, print the failing call and its body (or curl's error); clean up the tenant
   on exit, and say so if that delete is refused. The tenant counts as created from just
   before its POST. Every request is limited to `NEON_SMOKE_MAX_TIME` seconds (default 60).

It creates only a throwaway tenant and never touches the metadata database or any
user database.

**Done when:**
- `just neon-up && just neon-smoke` passes on fresh volumes;
- `just deps-up` alone still starts only the existing services;
- `just down` stops the storage stack too;
- `just neon-test` passes.

## Out of scope

Rust code (step 2), the internal listener and the proxy (step 3), any change to
provisioning, the worker, reveal, reset, delete or reconcile (step 4), more than one
safekeeper or pageserver, and publishing images from this repo.
