#!/usr/bin/env bash
# Makes the first account of the instance: an admin who may run CI. Run it as root.
#
#   sudo bash deploy/prod/first-admin.sh <username> <email>
#
# It refuses unless the instance has no accounts at all, so it can never make a second admin behind
# the back of the first. Nothing here deletes or resets accounts. Every account after the first one
# comes from an invite (Site admin > Invites). If the first account is lost, sign in with its
# recovery codes or see docs/operations.md.
set -euo pipefail
CH_SCRIPT=first-admin
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
ch_env_load
user="${1:?usage: first-admin.sh <username> <email>}"
email="${2:?usage: first-admin.sh <username> <email>}"
C="$(ch_checkout)/deploy/compose.sh"
cid=$(bash "$C" ps -q server)
[ -n "$cid" ] || ch_die "the server is not running. Start it first: deploy/prod/up.sh"
if docker inspect "$cid" --format '{{range .Config.Env}}{{println .}}{{end}}' | grep -q '^CODEHAVEN__WEBHOOKS__ALLOW_PRIVATE=true'; then
  ch_die "the server runs with webhooks.allow_private on, which is for test stacks only. Recreate it without that first."
fi
existing=$(bash "$C" exec -T server codehaven admin users | grep -c . || true)
if [ "$existing" != 0 ]; then
  ch_die "the instance already has $existing account(s). Sign in as one of them, or ask an admin for an invite. This script never removes accounts."
fi
bash "$C" exec -T server codehaven admin create-user --username "$user" --email "$email" --admin --can-run-ci
echo "Open the link above (it works once, for 1 hour), choose a password and turn on two-factor."
