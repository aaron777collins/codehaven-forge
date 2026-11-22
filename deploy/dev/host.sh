#!/usr/bin/env bash
# From your machine: sync a stream's checkout to the build host and run a command on the HOST (not
# in the builder), inside the synced directory. For docker and compose work such as release builds
# and the end-to-end run. Needs CH_HOST and CH_REMOTE_ROOT (see deploy/dev/local.env.example).
#   deploy/dev/host.sh <stream> "bash deploy/dev/e2e.sh --tag test-x --steps 0-1"
# Building on this machine instead needs no ssh: leave CH_HOST unset and use deploy/dev/local.sh.
set -euo pipefail
CH_SCRIPT=host
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
[ $# -ge 2 ] || { echo "usage: $0 <stream> <command>" >&2; exit 2; }
stream="$1"; shift
ch_remote_load
here="$(ch_checkout)"
src="$(ch_stream_source "$stream")" || exit 1
dest="$CH_REMOTE_ROOT/streams/$stream"
# `--exclude .git` has no trailing slash on purpose: it matches a file (a worktree's .git) and a
# directory alike. With `--delete`, a copied .git file would replace the server checkout's .git
# directory, so .git is never synced, for any stream, `src` included.
git_flag="--exclude .git"
if [ "$stream" = src ]; then dest="$CH_REMOTE_ROOT/src"; fi
ssh "$CH_HOST" "mkdir -p $(printf '%q' "$dest")"
# Env files stay where they are: rsync never copies or deletes anything it excludes.
rsync -az --no-owner --no-group --delete --exclude target/ $git_flag --exclude .worktrees/ --exclude '.beads*' --exclude .DS_Store --exclude dist/ \
  --include .env.example --exclude .env --exclude '*.env' "$src/" "$CH_HOST:$dest/"
envs="export CH_STREAM=$stream CODEHAVEN_BUILD_ROOT=$(printf '%q' "$CH_REMOTE_ROOT") CH_ENV_FILE=$(printf '%q' "${CH_REMOTE_ENV_FILE:-$CH_REMOTE_ROOT/codehaven.env}")"
ssh "$CH_HOST" "cd $(printf '%q' "$dest") && $envs && bash deploy/dev/diskcheck.sh && bash -c $(printf '%q' "$*")"
