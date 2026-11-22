#!/usr/bin/env bash
# Runs on the build host. Takes one of a few build slots, then runs a command in the builder.
#   slot.sh <stream> <command...>
# At most CODEHAVEN_BUILD_SLOTS (default 3) builds run at once, so the host keeps room for
# everything else. Settings come from the env file (see .env.example):
#   CODEHAVEN_BUILD_ROOT   where the slot locks live (required)
#   CODEHAVEN_BUILD_CPUS   cpus per build, default min(nproc, 3)
#   CODEHAVEN_BUILD_MEMORY memory per build, default 8g
set -euo pipefail
CH_SCRIPT=slot
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
ch_env_load
stream="${1:?usage: slot.sh <stream> <command...>}"; shift
ch_need CODEHAVEN_BUILD_ROOT
root="$CODEHAVEN_BUILD_ROOT"
# CH_DIR lets a CI job build its own checkout with the same caps and caches.
dir="${CH_DIR:-$(ch_checkout)}"
slots="${CODEHAVEN_BUILD_SLOTS:-3}"
mkdir -p "$root/run"

img="$(bash "$dir/deploy/dev/builder-image.sh")"
cpus="${CODEHAVEN_BUILD_CPUS:-$(n=$(nproc); [ "$n" -lt 3 ] && echo "$n" || echo 3)}"
export CODEHAVEN_BUILD_CPUS="$cpus" CODEHAVEN_BUILD_MEMORY="${CODEHAVEN_BUILD_MEMORY:-8g}" CODEHAVEN_BUILDER_IMAGE="$img"

got=""
fd=""
while [ -z "$got" ]; do
  for n in $(seq 1 "$slots"); do
    exec {fd}>"$root/run/slot.$n"
    if flock -n "$fd"; then got="$n"; break; fi
    exec {fd}>&-
  done
  [ -n "$got" ] || { echo "slot: all $slots build slots busy, waiting..." >&2; sleep 5; }
done
echo "slot: using slot $got for $stream" >&2

export CH_STREAM="$stream" CH_STREAM_DIR="$dir"
cd "$dir"
compose=(docker compose -p "${CODEHAVEN_DEV_PROJECT:-codehaven-dev}" -f "$dir/deploy/compose.dev.yml")
# `bash -c`, not `-lc`: a login shell resets PATH on Debian and loses /usr/local/cargo/bin.
# --rm removes the container; -T so ssh without a tty works.
"${compose[@]}" run --rm -T builder bash -c "$*"
