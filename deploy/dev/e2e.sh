#!/usr/bin/env bash
# Runs on the build host, in a synced checkout. Builds the images and runs the end-to-end harness
# in a throwaway compose project of its own, then tears that project down.
#   CH_STREAM=foundation deploy/dev/e2e.sh --tag test-mine [--steps 0-1] [--no-build] [--project e2e-mine]
#
# Safety: the image tag and the project name must both be throwaway names (test-, e2e-, shots-
# or tmp- then a word), never the instance people use. A project that already holds a container
# not started as a throwaway one is left alone. There is no flag that relaxes this.
# Needs CODEHAVEN_E2E_SUBNET in the env file: a free /20 on this host.
set -euo pipefail
CH_SCRIPT=e2e
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
. "$(dirname "${BASH_SOURCE[0]}")/../lib/throwaway.sh"
ch_env_load
steps="0-1"; build=1; tag=""; project=""
while [ $# -gt 0 ]; do
  case "$1" in
    --steps) steps="${2:?}"; shift 2;;
    --tag) tag="${2:?}"; shift 2;;
    --project) project="${2:?}"; shift 2;;
    --no-build) build=0; shift;;
    *) echo "unknown argument $1" >&2; exit 2;;
  esac
done
[ -n "$tag" ] || ch_die "give a throwaway image tag with --tag (for example test-mine). There is no default."
[ -n "$project" ] || project="e2e-${tag#*-}"
project="$(printf '%s' "$project" | tr 'A-Z_.' 'a-z--')"
ch_require_throwaway "$project" "$tag"
ch_project_only_throwaway "$project"
ch_need CODEHAVEN_E2E_SUBNET
dir="$(ch_checkout)"
[ "$build" = 0 ] || bash "$dir/deploy/dev/release.sh" "$tag"
compose=(docker compose -p "$project" -f "$dir/deploy/compose.e2e.yml")
export CODEHAVEN_TAG="$tag" E2E_STEPS="$steps" CODEHAVEN_E2E_SUBNET
rc=0
"${compose[@]}" up --abort-on-container-exit --exit-code-from e2e || rc=$?
"${compose[@]}" logs --no-color server 2>&1 | tail -40 || true
"${compose[@]}" down -v --remove-orphans
exit "$rc"
