#!/usr/bin/env bash
# Screenshots of the demo pages, built and shot on the build host.
#
#   bash crates/ch-web/screens/shoot.sh [name-filter] [outdir]
#
# Needs CH_HOST and CH_REMOTE_ROOT (see deploy/dev/local.env.example) and headless Chrome on that
# host. STREAM (default web) names the worktree to build.
#
# 1. builds the `snapshot` example in the builder container and renders every demo page to HTML
#    (light and dark) under <CH_REMOTE_ROOT>/streams/<stream>/screens-out on the host;
# 2. serves that folder on 127.0.0.1:3292 with python (loopback only) and shoots each page with
#    the headless Chrome, at 1280x800 and 390x844;
# 3. copies the PNGs back and removes everything it made on the host.
set -euo pipefail
filter="${1:-}"
out="${2:-${TMPDIR:-/tmp}/ch-screens}"
stream="${STREAM:-web}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
. "$root/deploy/lib/env.sh"
CH_SCRIPT=shoot
ch_remote_load
cd "$(git -C "$root" rev-parse --show-toplevel)"
repo_main="$(git rev-parse --git-common-dir)/.."
cd "$repo_main"
bash deploy/dev/remote.sh "$stream" "cargo run -q -p ch-web --example snapshot -- /work/screens-out $filter"
ssh "$CH_HOST" "SHOTS=${SHOTS:-light} SHOTS_DIR=$(printf '%q' "$CH_REMOTE_ROOT/streams/$stream/screens-out") bash -s" -- "$filter" <<'REMOTE'
set -euo pipefail
filter="$1"
cd "$SHOTS_DIR"
rm -rf /tmp/ch-shots && mkdir -p /tmp/ch-shots
python3 -m http.server 3292 --bind 127.0.0.1 >/dev/null 2>&1 </dev/null &
srv=$!
trap 'kill $srv 2>/dev/null || true' EXIT
sleep 1
# Which shots: light desktop for every page, dark desktop and light mobile when SHOTS=all.
shots="${SHOTS:-light}"
jobs=/tmp/ch-shots/jobs.txt
: > "$jobs"
for f in *.html; do
  base="${f%.html}"
  theme="${base##*.}"
  case "$shots:$theme" in
    light:dark) continue;;
  esac
  echo "1280 800 desktop $base $f" >> "$jobs"
  if [ "$theme" = light ] && [ "$shots" = all ]; then echo "390 844 mobile $base $f" >> "$jobs"; fi
done
xargs -P 4 -L1 bash -c 'google-chrome --headless=new --no-sandbox --disable-gpu --hide-scrollbars --force-device-scale-factor=1 --window-size="$0,$1" --virtual-time-budget=1500 --screenshot="/tmp/ch-shots/$3.$2.png" "http://127.0.0.1:3292/$4" >/dev/null 2>&1 || true' < "$jobs"
kill "$srv" 2>/dev/null || true
REMOTE
mkdir -p "$out"
rsync -az "$CH_HOST:/tmp/ch-shots/" "$out/"
ssh "$CH_HOST" "rm -rf /tmp/ch-shots $(printf '%q' "$CH_REMOTE_ROOT/streams/$stream/screens-out")"
echo "screenshots in $out"
