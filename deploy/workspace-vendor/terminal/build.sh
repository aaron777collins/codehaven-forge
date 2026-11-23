#!/usr/bin/env bash
# Rebuilds crates/ch-web/workspace/vendor/terminal/ from the pinned inputs in this folder.
# Needs node 20 or later, npm, and access to the npm registry. Nothing in a normal Codehaven
# build runs it; the output is committed, so a checkout already holds what it produces.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; root="$(cd "$here/../../.." && pwd)"
work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
cp -R "$here/package.json" "$here/package-lock.json" "$here/entry.js" "$here/stubs" "$work/"
(cd "$work" && npm ci --ignore-scripts --no-audit --no-fund)
(cd "$work" && npm pack --silent @xterm/xterm@6.0.0 >/dev/null)   # writes xterm-xterm-6.0.0.tgz
(cd "$work" && ./node_modules/.bin/esbuild entry.js --bundle --format=iife --global-name=JustBash --platform=browser --target=es2022 --minify --legal-comments=eof --alias:turndown=./stubs/turndown.js --alias:node:zlib=./stubs/zlib.js --metafile=meta.json --outfile=just-bash.js --log-level=warning)
python3 "$here/terminal.py" --xterm "$work/xterm-xterm-6.0.0.tgz" --bundle "$work/just-bash.js" --meta "$work/meta.json" --modules "$work/node_modules" --lock "$work/package-lock.json" --out "$root/crates/ch-web/workspace/vendor/terminal"
