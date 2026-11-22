#!/usr/bin/env bash
# Runs on the build host, in a synced checkout. Builds release binaries in the builder (shared
# cargo and sccache volumes, one of the build slots) and the runtime images from them.
#   CH_STREAM=foundation deploy/dev/release.sh <tag> [--deploy]
# Produces codehaven-server, codehaven-e2e and codehaven-runner images tagged <tag>.
#
# The tag is required: there is no default. A tag that a running container of the instance people
# use already runs is refused, because the next restart would start the new build. Pass --deploy
# only when that is the point.
set -euo pipefail
CH_SCRIPT=release
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
. "$(dirname "${BASH_SOURCE[0]}")/../lib/throwaway.sh"
ch_env_load
tag=""; deploy=0
for a in "$@"; do
  case "$a" in --deploy) deploy=1;; -*) ch_die "unknown argument $a";; *) [ -z "$tag" ] || ch_die "one tag only"; tag="$a";; esac
done
[ -n "$tag" ] || ch_die "usage: release.sh <tag> [--deploy]. The tag is required."
[[ "$tag" =~ ^[A-Za-z0-9_.-]+$ ]] || ch_die "tag '$tag' has characters docker does not allow"
if [ "$deploy" = 0 ] && ch_tag_in_use "$tag"; then
  ch_die "tag '$tag' is the one a running instance uses. Pick another tag, or pass --deploy to replace it."
fi
stream="${CH_STREAM:-foundation}"
dir="$(ch_checkout)"
bash "$dir/deploy/dev/diskcheck.sh"
rm -rf "$dir/dist" && mkdir -p "$dir/dist"
bash "$dir/deploy/dev/slot.sh" "$stream" \
  'cargo build --release -p codehaven -p ch-e2e -p haven-runner -p haven-cli \
   && for b in codehaven ch-e2e haven-runner haven; do cp /target/release/$b /work/dist/$b; done \
   && ls -la /work/dist'
for target in server e2e runner; do
  docker buildx build ${CODEHAVEN_BUILDX_BUILDER:+--builder "$CODEHAVEN_BUILDX_BUILDER"} --load --target "$target" \
    -t "codehaven-$target:$tag" -f "$dir/deploy/images/runtime.Dockerfile" "$dir/dist"
done
rm -rf "$dir/dist"
docker image ls --filter label=org.codehaven=1 --format '{{.Repository}}:{{.Tag}} {{.Size}}'
