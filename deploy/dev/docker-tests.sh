#!/usr/bin/env bash
# Runs on the build host, in a synced stream directory. Starts a throwaway Docker daemon
# (`dind-test`, never the host's), builds the job image into it, runs the runner's executor tests
# against it, and stops the daemon again.
#   CH_STREAM=runner deploy/dev/docker-tests.sh [nextest filter...]
# The daemon's image store is a volume (codehaven-dind-test-data) so reruns are quick. Remove it
# with `deploy/dev/clean.sh` or CH_DOCKER_TEST_CLEAN=1.
set -euo pipefail
CH_SCRIPT=docker-tests
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
ch_env_load
dir="$(ch_checkout)"
stream="${CH_STREAM:-runner}"
# compose.dev.yml needs these to render, even for the one service started here.
export CH_STREAM_DIR="$dir" CODEHAVEN_BUILDER_IMAGE="$(bash "$dir/deploy/dev/builder-image.sh")"
compose=(docker compose -p "${CODEHAVEN_DEV_PROJECT:-codehaven-dev}" -f "$dir/deploy/compose.dev.yml" --profile docker-tests)
bash "$dir/deploy/dev/diskcheck.sh"
cleanup() {
  "${compose[@]}" rm -sf dind-test >/dev/null 2>&1 || true
  if [ "${CH_DOCKER_TEST_CLEAN:-0}" = 1 ]; then
    docker volume rm codehaven-dind-test-data codehaven-ci-work >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT
"${compose[@]}" up -d --wait dind-test
# A run that was killed leaves containers, and the networks the runner made for them, behind in the daemon's volume. Start clean.
"${compose[@]}" exec -T dind-test sh -c 'ids=$(docker ps -aq); [ -z "$ids" ] || docker rm -f $ids >/dev/null 2>&1; nets=$(docker network ls -q --filter label=org.codehaven.network=1); [ -z "$nets" ] || docker network rm $nets >/dev/null 2>&1; true'
# The job image, built inside the test daemon. No build context needed.
if ! "${compose[@]}" exec -T dind-test docker image inspect codehaven-job:trixie >/dev/null 2>&1; then
  echo "building codehaven-job:trixie inside the test daemon (first run only)"
  "${compose[@]}" exec -T dind-test docker build -t codehaven-job:trixie - < "$dir/deploy/images/job.Dockerfile"
fi
filter=("$@")
bash "$dir/deploy/dev/slot.sh" "$stream" "HAVEN_REQUIRE_DOCKER=1 cargo nextest run -p haven-runner --test docker --no-fail-fast -j 4 ${filter[*]:-}"
