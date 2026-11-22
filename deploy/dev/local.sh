#!/usr/bin/env bash
# Builds on THIS machine, with no ssh. Same jobs as remote.sh and host.sh, run here.
#   deploy/dev/local.sh <stream> "cargo nextest run -p ch-core"          in the builder
#   deploy/dev/local.sh --host <stream> "bash deploy/dev/e2e.sh ..."     on this machine
# Needs a .env (see .env.example): CODEHAVEN_BUILD_ROOT at least.
set -euo pipefail
CH_SCRIPT=local
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
mode=builder
[ "${1:-}" != --host ] || { mode=host; shift; }
[ $# -ge 2 ] || { echo "usage: $0 [--host] <stream> <command>" >&2; exit 2; }
stream="$1"; shift
cd "$here"
bash deploy/dev/diskcheck.sh
if [ "$mode" = host ]; then CH_STREAM="$stream" bash -c "$*"; else bash deploy/dev/slot.sh "$stream" "$*"; fi
