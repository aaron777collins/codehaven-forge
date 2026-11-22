#!/usr/bin/env bash
# From your machine: sync a stream's checkout to the build host and run a command in the builder.
# Needs CH_HOST and CH_REMOTE_ROOT (see deploy/dev/local.env.example).
#   deploy/dev/remote.sh <stream> "cargo nextest run -p ch-core"
# Env:
#   CH_FETCH="a b"   paths (relative to the repo) to copy back from the host afterwards (e.g. .sqlx)
# No cargo runs on this machine. To build here instead, leave CH_HOST unset and use local.sh.
set -euo pipefail
CH_SCRIPT=remote
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
[ $# -ge 2 ] || { echo "usage: $0 <stream> <command>" >&2; exit 2; }
stream="$1"; shift
ch_remote_load
src="$(ch_stream_source "$stream")" || exit 1
dest="$CH_REMOTE_ROOT/streams/$stream"
# `--exclude .git` has no trailing slash on purpose: it matches a file (a worktree's .git) and a
# directory alike. With `--delete`, a copied .git file would replace the server checkout's .git
# directory, so .git is never synced, for any stream, `src` included.
git_flag="--exclude .git"
if [ "$stream" = src ]; then dest="$CH_REMOTE_ROOT/src"; fi

ssh "$CH_HOST" "mkdir -p $(printf '%q' "$dest")"
rsync -az --no-owner --no-group --delete \
  --exclude target/ $git_flag --exclude .worktrees/ --exclude '.beads*' --exclude .DS_Store \
  --include .env.example --exclude .env --exclude '*.env' "$src/" "$CH_HOST:$dest/"
envs="export CODEHAVEN_BUILD_ROOT=$(printf '%q' "$CH_REMOTE_ROOT") CH_ENV_FILE=$(printf '%q' "${CH_REMOTE_ENV_FILE:-$CH_REMOTE_ROOT/codehaven.env}")"
rc=0
ssh "$CH_HOST" "cd $(printf '%q' "$dest") && $envs && bash deploy/dev/diskcheck.sh && bash deploy/dev/slot.sh $stream $(printf '%q' "$*")" || rc=$?
for p in ${CH_FETCH:-}; do
  rsync -az "$CH_HOST:$dest/$p" "$src/$(dirname "$p")/" || true
done
exit "$rc"
