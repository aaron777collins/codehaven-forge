#!/usr/bin/env bash
# The one way scripts call `docker compose` for the instance. It loads the env file, works out the
# host's public addresses, picks the compose files for CODEHAVEN_PROXY and passes the project name.
#   deploy/compose.sh ps
#   deploy/compose.sh --profile ci up -d
set -euo pipefail
CH_SCRIPT=compose
dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
. "$dir/deploy/lib/env.sh"
ch_env_load
ch_need CODEHAVEN_PROJECT CODEHAVEN_PROXY
# After a deploy that up.sh rolled back, every later compose call (ci-up.sh, a restart by hand)
# keeps running the rolled back images without migrating, until up.sh deploys a fixed version.
# Without this the next routine `up` would bring back the tag in the env file, with migrations on.
rolled_back="${CODEHAVEN_BUILD_ROOT:+$CODEHAVEN_BUILD_ROOT/run/$CODEHAVEN_PROJECT.rolled-back}"
if [ -n "$rolled_back" ] && [ -f "$rolled_back" ] && [ -z "${CODEHAVEN_DEPLOYING:-}" ]; then
  export CODEHAVEN_TAG CODEHAVEN_NO_MIGRATE=1
  CODEHAVEN_TAG="$(head -n 1 "$rolled_back")"
  echo "compose: the last deploy was rolled back, so this uses tag $CODEHAVEN_TAG without migrating. Deploy a fixed version with deploy/prod/up.sh." >&2
fi
# The image tag: CODEHAVEN_TAG, else the short commit id of this checkout. Never a fixed name.
if [ -z "${CODEHAVEN_TAG:-}" ] && git -C "$dir" rev-parse --short=12 HEAD >/dev/null 2>&1; then
  export CODEHAVEN_TAG; CODEHAVEN_TAG="$(git -C "$dir" rev-parse --short=12 HEAD)"
fi
# Addresses the host answers on from outside. dind keeps CI jobs off them and webhooks may not reach
# them. Worked out here so they never go stale; an empty list is fine on a host with no public address.
if [ -z "${HOST_PUBLIC_IPS+x}" ]; then export HOST_PUBLIC_IPS; HOST_PUBLIC_IPS="$(sh "$dir/deploy/prod/host-ips.sh")"; fi
if [ -z "${WEBHOOK_BLOCKED_IPS+x}" ]; then export WEBHOOK_BLOCKED_IPS; WEBHOOK_BLOCKED_IPS="$(sh "$dir/deploy/prod/host-ips.sh" --json)"; fi
files=(-f "$dir/deploy/compose.yml")
case "$CODEHAVEN_PROXY" in
  none) ;;
  network) files+=(-f "$dir/deploy/compose.proxy-network.yml") ;;
  caddy) files+=(-f "$dir/deploy/compose.caddy.yml") ;;
  *) ch_die "CODEHAVEN_PROXY must be none, network or caddy (it is '$CODEHAVEN_PROXY')" ;;
esac
# Rolling back to a version older than the database: serve without migrating (docs/operations.md).
[ "${CODEHAVEN_NO_MIGRATE:-}" != 1 ] || files+=(-f "$dir/deploy/compose.no-migrate.yml")
# An extra compose file of your own (a fixed subnet for a network, resource limits). Optional.
if [ -n "${CODEHAVEN_COMPOSE_EXTRA:-}" ]; then
  [ -f "$CODEHAVEN_COMPOSE_EXTRA" ] || ch_die "CODEHAVEN_COMPOSE_EXTRA=$CODEHAVEN_COMPOSE_EXTRA does not exist"
  files+=(-f "$CODEHAVEN_COMPOSE_EXTRA")
fi
exec docker compose -p "$CODEHAVEN_PROJECT" "${files[@]}" "$@"
