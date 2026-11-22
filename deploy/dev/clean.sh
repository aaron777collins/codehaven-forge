#!/usr/bin/env bash
# Runs on the build host. Removes only what Codehaven's builds made. Never `docker system prune`.
#   clean.sh                 dangling codehaven images and the codehaven buildx cache
#   clean.sh <stream> ...    also removes the codehaven-target-<stream> volumes and the stream's checkout
set -euo pipefail
CH_SCRIPT=clean
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
ch_env_load
ch_need CODEHAVEN_BUILD_ROOT
for s in "$@"; do
  [[ "$s" =~ ^[a-z0-9][a-z0-9_-]*$ ]] || ch_die "stream '$s' is not a plain name"
  [ "$s" != src ] || ch_die "'src' is the checkout an instance runs from. Not removing it."
  docker volume rm "codehaven-target-$s" 2>/dev/null && echo "removed codehaven-target-$s" || true
  rm -rf "${CODEHAVEN_BUILD_ROOT:?}/streams/$s"
done
# The runner's test daemon and the directory it shares with the builder.
docker volume rm codehaven-dind-test-data codehaven-ci-work 2>/dev/null && echo "removed the runner test volumes" || true
docker image ls --filter dangling=true --filter label=org.codehaven=1 -q | xargs -r docker image rm
docker buildx prune ${CODEHAVEN_BUILDX_BUILDER:+--builder "$CODEHAVEN_BUILDX_BUILDER"} --force --filter until=168h || true
bash "$(ch_checkout)/deploy/dev/diskcheck.sh" || true
