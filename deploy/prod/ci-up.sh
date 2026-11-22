#!/usr/bin/env bash
# Starts CI for the instance: the separate Docker daemon (`dind`), the job image inside it, and the
# runner. Idempotent. Run it as root.
#
# The runner never gets the host's Docker socket. Jobs run in `dind`, which has its own image
# store under CODEHAVEN_DATA_DIR/dind.
#
# First time only, register the runner with a token from Site admin > Runners (or
# `deploy/compose.sh exec server codehaven admin runner-token`), passed in the environment so it
# stays out of `ps`. The name is the argument, else CODEHAVEN_RUNNER_NAME:
#   HAVEN_RUNNER_TOKEN=<token> sudo -E bash deploy/prod/ci-up.sh register [name]
#
# Then: sudo bash deploy/prod/ci-up.sh
set -euo pipefail
CH_SCRIPT=ci-up
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
ch_env_load
dir="$(ch_checkout)"
tag="${CODEHAVEN_TAG:-}"
[ -n "$tag" ] || { git -C "$dir" rev-parse --short=12 HEAD >/dev/null 2>&1 && tag="$(git -C "$dir" rev-parse --short=12 HEAD)"; } || true
[ -n "$tag" ] || ch_die "no tag. Set CODEHAVEN_TAG in .env."
export CODEHAVEN_TAG="$tag"
C=(bash "$dir/deploy/compose.sh" --profile ci)
bash "$dir/deploy/dev/diskcheck.sh"

if [ "${1:-}" = register ]; then
  name="${2:-${CODEHAVEN_RUNNER_NAME:-}}"
  [ -n "$name" ] || ch_die "give the runner a name: ci-up.sh register <name>, or set CODEHAVEN_RUNNER_NAME in .env"
  [ -n "${HAVEN_RUNNER_TOKEN:-}" ] || ch_die "set HAVEN_RUNNER_TOKEN to the registration token"
  "${C[@]}" run --rm -e HAVEN_RUNNER_URL=http://server:3200 -e HAVEN_RUNNER_TOKEN runner haven-runner register --name "$name"
  exit 0
fi

# Images: the runner and the job image, built on the host and (for the job image) loaded into dind.
docker image inspect "codehaven-runner:$tag" >/dev/null 2>&1 || ch_die "build the images first: deploy/prod/up.sh"
docker image inspect codehaven-job:trixie >/dev/null 2>&1 || bash "$dir/deploy/dev/job-image.sh"

"${C[@]}" up -d dind
for _ in $(seq 1 60); do "${C[@]}" exec -T dind docker info >/dev/null 2>&1 && break; sleep 2; done
if ! "${C[@]}" exec -T dind docker image inspect codehaven-job:trixie >/dev/null 2>&1; then
  echo "loading codehaven-job:trixie into dind"
  docker save codehaven-job:trixie | "${C[@]}" exec -T dind docker load
fi

"${C[@]}" up -d runner
"${C[@]}" ps
