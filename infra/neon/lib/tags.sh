# shellcheck shell=bash disable=SC2034,SC2154
# (Sourced: $here comes from the caller, and the variables are used there.)
#
# Image tags derived from the pinned ref. Sourced (with $here set to infra/neon) by build.sh,
# tests/run.sh and scripts/neon-images.sh, so they can never disagree.
#
#   neon_tag      first 12 chars of NEON_REF
#   neon_dev_tag  neon_tag plus -dev: the tag of the dev variant's images
#                 (cheladb/neon-storage and cheladb/neon-compute-v17 locally)

neon_ref="$(tr -d '[:space:]' <"$here/NEON_REF")"
neon_tag="${neon_ref:0:12}"
neon_dev_tag="${neon_tag}-dev"
