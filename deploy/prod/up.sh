#!/usr/bin/env bash
# Builds the images (when this tag has none yet) and starts or updates the instance, then checks
# that the published ports are only where CODEHAVEN_HTTP_BIND and CODEHAVEN_SSH_BIND say.
#   sudo bash deploy/prod/up.sh [tag]
# The tag is the argument, else CODEHAVEN_TAG, else the short commit id of this checkout. It is
# never defaulted to a name that something else may already run. Upgrade = git pull, then up.sh.
set -euo pipefail
CH_SCRIPT=up
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
ch_env_load
dir="$(ch_checkout)"
tag="${1:-${CODEHAVEN_TAG:-}}"
if [ -z "$tag" ] && git -C "$dir" rev-parse --short=12 HEAD >/dev/null 2>&1; then tag="$(git -C "$dir" rev-parse --short=12 HEAD)"; fi
[ -n "$tag" ] || ch_die "no tag. Set CODEHAVEN_TAG in .env, or pass one: up.sh <tag>."
export CODEHAVEN_TAG="$tag"
ch_need CODEHAVEN_PROJECT CODEHAVEN_HTTP_BIND CODEHAVEN_SSH_BIND CODEHAVEN_BUILD_ROOT
bash "$dir/deploy/prod/setup.sh"
if ! docker image inspect "codehaven-server:$tag" >/dev/null 2>&1 || ! docker image inspect "codehaven-runner:$tag" >/dev/null 2>&1; then
  # --deploy: this tag is the one the instance is about to run, so building it is the point.
  CH_STREAM=src bash "$dir/deploy/dev/release.sh" "$tag" --deploy
fi
C="$dir/deploy/compose.sh"
bash "$C" up -d
state=""
for _ in $(seq 1 40); do
  state=$(bash "$C" ps --format '{{.Service}} {{.Health}}' | grep '^server ' | cut -d' ' -f2 || true)
  [ "$state" = healthy ] && break
  sleep 3
done
bash "$C" ps
echo "--- published ports ---"
for b in "$CODEHAVEN_HTTP_BIND" "$CODEHAVEN_SSH_BIND"; do
  host="${b%:*}"; port="${b##*:}"
  ss -tln 2>/dev/null | grep -E "[:.]$port " || true
  if [ "$host" = 127.0.0.1 ] && ss -tln 2>/dev/null | grep -E "(0\.0\.0\.0|\*|\[::\]):$port " >/dev/null; then
    ch_die "port $port is bound beyond loopback but $b was asked for"
  fi
done
curl -fsS "http://$CODEHAVEN_HTTP_BIND/healthz"; echo
[ "$state" = healthy ] || ch_die "the server is not healthy. See: deploy/compose.sh logs server"
