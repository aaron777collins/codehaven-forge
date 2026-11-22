#!/usr/bin/env bash
# Runs on the build host, in a synced checkout. Starts a throwaway stack with the Claude connector
# on, drives it with tests/e2e/connector_e2e.py (a real MCP client included) and tears it down.
#   bash deploy/tests/connector-e2e.sh --tag test-conn [--project e2e-conn] [--no-build] [--shots DIR]
#
# Safety: the image tag and the project name must be throwaway names, and the stack must hold no
# account. Needs CODEHAVEN_E2E_SUBNET in the env file, python3 with venv and pip, and outbound
# https (the server fetches the real Claude Code client document from claude.ai). --shots DIR also
# takes screenshots of the consent, return, Connected apps and waiting-run pages into DIR; it needs
# node, Chrome and CODEHAVEN_SHOTS_NODE_MODULES (a directory that holds playwright-core).
set -euo pipefail
CH_SCRIPT=connector-e2e
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
. "$(dirname "${BASH_SOURCE[0]}")/../lib/throwaway.sh"
ch_env_load
tag=""; project=""; build=1; shots=""
while [ $# -gt 0 ]; do
  case "$1" in
    --tag) tag="${2:?}"; shift 2;;
    --project) project="${2:?}"; shift 2;;
    --no-build) build=0; shift;;
    --shots) shots="${2:?}"; shift 2;;
    *) ch_die "unknown argument $1";;
  esac
done
[ -n "$tag" ] || ch_die "give a throwaway image tag with --tag (for example test-conn). There is no default."
[ -n "$project" ] || project="e2e-mcp-${tag#*-}"
project="$(printf '%s' "$project" | tr 'A-Z_.' 'a-z--')"
ch_require_throwaway "$project" "$tag"
ch_project_only_throwaway "$project"
ch_need CODEHAVEN_E2E_SUBNET
dir="$(ch_checkout)"
[ "$build" = 0 ] || CH_STREAM="${CH_STREAM:-foundation}" bash "$dir/deploy/dev/release.sh" "$tag"
compose=(docker compose -p "$project" -f "$dir/deploy/compose.mcp-e2e.yml")
export CODEHAVEN_TAG="$tag" CODEHAVEN_E2E_SUBNET
work="$(mktemp -d)"
cleanup() { "${compose[@]}" down -v --remove-orphans >/dev/null 2>&1 || true; rm -rf "$work"; }
trap cleanup EXIT
# The MCP SDK in a virtualenv of its own, so nothing is installed on the host.
python3 -m venv "$work/venv"
"$work/venv/bin/pip" install --quiet --disable-pip-version-check "mcp==2.3.0"
"${compose[@]}" up -d --wait
cid="$("${compose[@]}" ps -q server)"
ip="$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$cid")"
[ -n "$ip" ] || ch_die "could not find the server's address"
if [ -n "$shots" ]; then
  ch_need CODEHAVEN_SHOTS_NODE_MODULES
  mkdir -p "$shots"
  # playwright-core is an ES module: the script is run from a directory that says so.
  ln -sfn "$CODEHAVEN_SHOTS_NODE_MODULES" "$work/node_modules"
  printf '{"name":"connector-shots","private":true,"type":"module"}\n' > "$work/package.json"
  cp "$dir/tests/e2e/shoot_connector.mjs" "$work/"
  export E2E_SHOTS_DIR="$shots" E2E_SHOTS_WORK="$work"
fi
E2E_BASE="http://$ip:3200" E2E_PUBLIC="https://code.example.com" E2E_PROJECT="$project" \
  E2E_CLI="docker compose -p $project -f $dir/deploy/compose.mcp-e2e.yml exec -T server codehaven" \
  E2E_MCP_PYTHON="$work/venv/bin/python" python3 "$dir/tests/e2e/connector_e2e.py"
