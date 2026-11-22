#!/usr/bin/env bash
# Runs on the build host in a checkout. Builds codehaven-job:trixie into the host's image store so
# deploy/prod/ci-up.sh can load it into the runner's own Docker daemon. Tagged <tag> too when given.
#   deploy/dev/job-image.sh [tag]
set -euo pipefail
CH_SCRIPT=job-image
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
ch_env_load
dir="$(ch_checkout)"
bash "$dir/deploy/dev/diskcheck.sh"
docker buildx build ${CODEHAVEN_BUILDX_BUILDER:+--builder "$CODEHAVEN_BUILDX_BUILDER"} --load -t codehaven-job:trixie -f "$dir/deploy/images/job.Dockerfile" "$dir/deploy/images"
[ $# -ge 1 ] && docker tag codehaven-job:trixie "codehaven-job:$1"
docker image ls codehaven-job --format '{{.Repository}}:{{.Tag}} {{.Size}}'
