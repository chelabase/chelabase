# shellcheck shell=bash disable=SC2034,SC2154
# (Sourced: $here comes from the caller, and the variables are used there.)
#
# Image tags derived from the pinned ref. Sourced (with $here set to infra/neon) by build.sh,
# tests/run.sh and scripts/neon-images.sh, so they can never disagree.
#
#   neon_tag  first 12 chars of NEON_REF: cheladb/neon and cheladb/compute-node-v17

neon_ref="$(tr -d '[:space:]' <"$here/NEON_REF")"
neon_tag="${neon_ref:0:12}"
