#!/usr/bin/env bash
# Builds the images (when this tag has none yet) and starts or updates the instance, then checks
# that the published ports are only where CODEHAVEN_HTTP_BIND and CODEHAVEN_SSH_BIND say.
#   sudo bash deploy/prod/up.sh [tag]
# The tag is the argument, else CODEHAVEN_TAG, else the short commit id of this checkout. It is
# never defaulted to a name that something else may already run. Upgrade = git pull, then up.sh.
# Before it changes anything it dumps the database to CODEHAVEN_BACKUP_DIR (pre-deploy-*.dump,
# the newest 5 are kept) and tags the running server image <project>-previous. After the deploy
# the server must be healthy and deploy/dev/public-smoke.sh must pass against CODEHAVEN_SMOKE_URL
# (default http://CODEHAVEN_HTTP_BIND), within CODEHAVEN_DEPLOY_WAIT seconds (default 300). If
# not, the server goes back to the previous image, started without migrating. The database is
# never rolled back.
set -euo pipefail
CH_SCRIPT=up
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
ch_env_load
# server_health: the server's health as compose reports it (healthy, starting, unhealthy, or empty).
server_health() { bash "$C" ps --format '{{.Service}} {{.Health}}' 2>/dev/null | grep '^server ' | cut -d' ' -f2 || true; }
# wait_healthy: up to CODEHAVEN_DEPLOY_WAIT seconds (default 300) for the server to be healthy.
# Generous, because a server applying a long migration is not healthy until it has finished.
wait_healthy() {
  local waited=0 limit="${CODEHAVEN_DEPLOY_WAIT:-300}"
  while [ "$waited" -lt "$limit" ]; do [ "$(server_health)" = healthy ] && return 0; sleep 3; waited=$((waited + 3)); done
  return 1
}
dir="$(ch_checkout)"
tag="${1:-${CODEHAVEN_TAG:-}}"
if [ -z "$tag" ] && git -C "$dir" rev-parse --short=12 HEAD >/dev/null 2>&1; then tag="$(git -C "$dir" rev-parse --short=12 HEAD)"; fi
[ -n "$tag" ] || ch_die "no tag. Set CODEHAVEN_TAG in .env, or pass one: up.sh <tag>."
export CODEHAVEN_TAG="$tag"
ch_need CODEHAVEN_PROJECT CODEHAVEN_HTTP_BIND CODEHAVEN_SSH_BIND CODEHAVEN_BUILD_ROOT
# This script picks the tag itself. compose.sh otherwise keeps a rolled back deploy pinned.
export CODEHAVEN_DEPLOYING=1
rolled_back="$CODEHAVEN_BUILD_ROOT/run/$CODEHAVEN_PROJECT.rolled-back"
# Set when the pre-deploy dump below is taken, so the rollback message can point at it.
dump=""
bash "$dir/deploy/prod/setup.sh"
C="$dir/deploy/compose.sh"
# What runs now: the running server's image, kept under a second tag so a failed deploy can be
# undone. prev_image is the image id from docker inspect (sha256:...), which docker tag accepts.
prev_cid="$(bash "$C" ps -q server 2>/dev/null || true)"
prev_image=""
if [ -n "$prev_cid" ]; then prev_image="$(docker inspect --format '{{.Image}}' "$prev_cid" 2>/dev/null || true)"; fi
rollback_tag="$CODEHAVEN_PROJECT-previous"
if [ -n "$prev_image" ]; then
  docker tag "$prev_image" "codehaven-server:$rollback_tag"
  echo "up: the running server image is kept as codehaven-server:$rollback_tag"
fi
# The runner's image too, when CI runs, so a rolled back instance can start CI again on it.
prev_runner="$(bash "$C" ps -q runner 2>/dev/null || true)"
if [ -n "$prev_runner" ] && runner_image="$(docker inspect --format '{{.Image}}' "$prev_runner" 2>/dev/null)" && [ -n "$runner_image" ]; then
  docker tag "$runner_image" "codehaven-runner:$rollback_tag"
fi
# The pre-deploy dump: nothing is changed until the database as it is now has been saved.
if bash "$C" ps --status running --format '{{.Service}}' 2>/dev/null | grep -qx postgres; then
  ch_need CODEHAVEN_BACKUP_DIR
  mkdir -p "$CODEHAVEN_BACKUP_DIR"; chmod 700 "$CODEHAVEN_BACKUP_DIR"
  case "$(cd "$CODEHAVEN_BACKUP_DIR" && pwd)/" in "$dir"/*) ch_die "CODEHAVEN_BACKUP_DIR is inside the checkout." ;; esac
  dump="$CODEHAVEN_BACKUP_DIR/pre-deploy-$CODEHAVEN_PROJECT-$(date -u +%Y%m%dT%H%M%SZ)-$tag.dump"
  ( umask 077; bash "$C" exec -T postgres pg_dump -U codehaven -d codehaven --format=custom > "$dump" ) || { rm -f "$dump"; ch_die "the pre-deploy database dump failed, so nothing was deployed"; }
  [ -s "$dump" ] || { rm -f "$dump"; ch_die "the pre-deploy database dump is empty, so nothing was deployed"; }
  chmod 600 "$dump"
  echo "up: database dumped to $dump"
  # Keep the newest 5 pre-deploy dumps of this project.
  ls -1t "$CODEHAVEN_BACKUP_DIR"/pre-deploy-"$CODEHAVEN_PROJECT"-*.dump 2>/dev/null | tail -n +6 | while IFS= read -r old; do rm -f -- "$old"; done
fi
if ! docker image inspect "codehaven-server:$tag" >/dev/null 2>&1 || ! docker image inspect "codehaven-runner:$tag" >/dev/null 2>&1; then
  # --deploy: this tag is the one the instance is about to run, so building it is the point.
  CH_STREAM=src bash "$dir/deploy/dev/release.sh" "$tag" --deploy
fi
# A new version that cannot even start (compose fails) is a failed deploy too, not a reason to
# stop here with nothing running.
started=1
bash "$C" up -d || { started=0; echo "up: compose could not start the new version" >&2; }
# The deploy worked only if the server becomes healthy and then the smoke check passes.
mkdir -p "$CODEHAVEN_BUILD_ROOT/run"
deployed=0
if [ "$started" = 1 ] && wait_healthy; then
  smoke_url="${CODEHAVEN_SMOKE_URL:-http://$CODEHAVEN_HTTP_BIND}"
  if bash "$dir/deploy/dev/public-smoke.sh" "$smoke_url" > "$CODEHAVEN_BUILD_ROOT/run/up-smoke.log" 2>&1; then
    deployed=1
  else
    echo "up: the smoke check failed against $smoke_url:" >&2
    grep '^FAIL' "$CODEHAVEN_BUILD_ROOT/run/up-smoke.log" >&2 || true
  fi
else
  echo "up: the server did not become healthy" >&2
fi
if [ "$deployed" = 0 ]; then
  if [ -z "$prev_image" ]; then
    bash "$C" ps
    ch_die "the deploy failed and there was no earlier server to go back to. See: deploy/compose.sh logs server"
  fi
  # Only the server image goes back. The database is left as it is: the old server runs on it
  # without migrating, because the new one may have added migrations the old one does not know.
  echo "up: rolling the server back to the previous image (the database is not touched)" >&2
  CODEHAVEN_TAG="$rollback_tag" CODEHAVEN_NO_MIGRATE=1 bash "$C" up -d server
  # Later compose calls keep this tag and stay off migrations until a deploy succeeds.
  mkdir -p "$CODEHAVEN_BUILD_ROOT/run"
  printf '%s\n' "$rollback_tag" > "$rolled_back"
  if wait_healthy; then
    ch_die "the deploy of $tag failed and the server was rolled back to the previous image, which is healthy. The data was not rolled back${dump:+; the database as it was before the deploy is in $dump}. To return to normal migrations later, deploy a fixed version with up.sh."
  else
    ch_die "the deploy of $tag failed and the rolled back server is not healthy either. See: deploy/compose.sh logs server"
  fi
fi
rm -f "$rolled_back"
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
