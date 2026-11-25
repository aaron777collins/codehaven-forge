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
#   HAVEN_RUNNER_TOKEN=<token> sudo -E bash deploy/prod/ci-up.sh register [name] --replace   (after a restore: replaces the saved .runner)
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
  # --replace may be the second or third argument, and is never taken as the name.
  name="" replace=""
  for arg in "${2:-}" "${3:-}"; do
    [ -n "$arg" ] || continue
    if [ "$arg" = --replace ]; then replace=--replace
    elif [ -z "$name" ]; then name="$arg"
    else ch_die "register takes one name and --replace, not: $arg"
    fi
  done
  name="${name:-${CODEHAVEN_RUNNER_NAME:-}}"
  [ -n "$name" ] || ch_die "give the runner a name: ci-up.sh register <name>, or set CODEHAVEN_RUNNER_NAME in .env"
  [ -n "${HAVEN_RUNNER_TOKEN:-}" ] || ch_die "set HAVEN_RUNNER_TOKEN to the registration token"
  R=(haven-runner register)
  if [ -n "$replace" ]; then R+=(--replace); fi
  R+=(--name "$name")
  "${C[@]}" run --rm -e HAVEN_RUNNER_URL=http://server:3200 -e HAVEN_RUNNER_TOKEN runner "${R[@]}"
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
# Codehaven's own CI (.codehaven/workflows/ci.yml) runs in the builder image. Loaded only when
# asked, because it is a few GiB in dind's storage.
if [ "${CODEHAVEN_CI_BUILDER:-}" = 1 ]; then
  ver=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$dir/rust-toolchain.toml")
  builder="codehaven-builder:$ver"
  docker image inspect "$builder" >/dev/null 2>&1 || bash "$dir/deploy/dev/builder-image.sh" >/dev/null
  if ! "${C[@]}" exec -T dind docker image inspect "$builder" >/dev/null 2>&1; then
    echo "loading $builder into dind"
    docker save "$builder" | "${C[@]}" exec -T dind docker load
  fi
fi

"${C[@]}" up -d runner
"${C[@]}" ps
