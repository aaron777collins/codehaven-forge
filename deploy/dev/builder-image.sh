#!/usr/bin/env bash
# Builds the builder image (codehaven-builder:<rust toolchain>) when this host does not have it.
# Slow the first time (it compiles sqlx-cli and cargo-insta), then never again for that toolchain.
#   deploy/dev/builder-image.sh            prints the image name
set -euo pipefail
CH_SCRIPT=builder-image
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
ch_env_load
dir="$(ch_checkout)"
ver=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$dir/rust-toolchain.toml")
[ -n "$ver" ] || ch_die "no channel in rust-toolchain.toml"
img="codehaven-builder:$ver"
if ! docker image inspect "$img" >/dev/null 2>&1; then
  echo "builder-image: building $img (first run only, this takes a while)" >&2
  docker buildx build ${CODEHAVEN_BUILDX_BUILDER:+--builder "$CODEHAVEN_BUILDX_BUILDER"} --load \
    --build-arg "RUST_VERSION=$ver" -t "$img" -f "$dir/deploy/images/builder.Dockerfile" "$dir/deploy/images" >&2
fi
echo "$img"
