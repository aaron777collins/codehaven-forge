#!/usr/bin/env bash
# Stops (exit 1) when a disk is at LIMIT percent or more. Run on the build host.
#   CODEHAVEN_DISK_PATHS   paths to check, default "/"
#   CODEHAVEN_DISK_LIMIT   percent, default 85
set -euo pipefail
CH_SCRIPT=diskcheck
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
ch_env_load
LIMIT="${CODEHAVEN_DISK_LIMIT:-85}"
bad=0
for fs in ${CODEHAVEN_DISK_PATHS:-/}; do
  use=$(df -P "$fs" | tail -1 | tr -s ' ' | cut -d' ' -f5 | tr -dc '0-9')
  printf 'diskcheck %-6s %s%%\n' "$fs" "$use"
  if [ "$use" -ge "$LIMIT" ]; then
    echo "diskcheck: $fs is at ${use}% (limit ${LIMIT}%). Clean up before building." >&2
    bad=1
  fi
done
exit "$bad"
