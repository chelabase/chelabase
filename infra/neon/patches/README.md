# Patches on upstream Neon

We run Neon at the commit pinned in `../NEON_REF` and don't fork it. If we ever need a
change (for example an extension in the compute image), it goes here as a
`git format-patch` file, and `build.sh` applies it to the pristine checkout before building.

There are no patches yet, so `build.sh` doesn't apply any. The first patch also has to:
- make `build.sh` apply `patches/*.patch` after `checkout`;
- add a patch hash suffix to `neon_tag` in `lib/tags.sh`, so patched and unpatched builds
  never share a tag;
- publish new images and update the digests in `infra/compose/docker-compose.yml`.

Write each patch as a standalone commit in upstream's style, so it could be sent upstream.
