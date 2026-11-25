#!/usr/bin/env bash
# A throwaway, seeded copy of the stack, only to take screenshots. Everything runs in a compose
# project of its own (deploy/compose.shots.yml, tmpfs and named volumes only), the whole project is
# torn down by the trap at the end whatever happened, and the only thing left behind is the PNGs
# and results.json in the outdir. Nothing prints the session file or any cookie or token.
#
# Safety: the tag and the project must be throwaway names (see deploy/lib/throwaway.sh), and a
# project that already holds a container not started as a throwaway one is never touched.
#
# Runs on the build host from a synced checkout. Build the images first:
#   bash deploy/dev/host.sh <stream> 'CH_STREAM=<stream> bash deploy/dev/release.sh shots-mine'
#   bash deploy/dev/host.sh <stream> 'bash deploy/dev/shots.sh --tag shots-mine [outdir]'
# Needs in the env file: CODEHAVEN_BUILD_ROOT, CODEHAVEN_E2E_SUBNET, CODEHAVEN_SHOTS_RUNNERS_SUBNET,
# CODEHAVEN_SHOTS_NODE_MODULES (a directory that holds playwright-core).
set -euo pipefail
CH_SCRIPT=shots
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
. "$(dirname "${BASH_SOURCE[0]}")/../lib/throwaway.sh"
ch_env_load
tag=""; project=""; out=""
while [ $# -gt 0 ]; do
  case "$1" in
    --tag) tag="${2:?}"; shift 2;;
    --project) project="${2:?}"; shift 2;;
    -*) echo "unknown argument $1" >&2; exit 2;;
    *) out="$1"; shift;;
  esac
done
[ -n "$tag" ] || ch_die "give a throwaway image tag with --tag (for example shots-mine). There is no default."
[ -n "$project" ] || project="shots-${tag#*-}"
project="$(printf '%s' "$project" | tr 'A-Z_.' 'a-z--')"
ch_require_throwaway "$project" "$tag"
ch_project_only_throwaway "$project"
ch_need CODEHAVEN_BUILD_ROOT CODEHAVEN_E2E_SUBNET CODEHAVEN_SHOTS_RUNNERS_SUBNET CODEHAVEN_SHOTS_NODE_MODULES
dir="$(ch_checkout)"
work="$CODEHAVEN_BUILD_ROOT/shots-throwaway"
out="${out:-$work/out}"

for img in "codehaven-server:$tag" "codehaven-runner:$tag" codehaven-job:trixie; do
  docker image inspect "$img" >/dev/null 2>&1 || {
    echo "shots: $img is missing on this host. Build the images first: bash deploy/dev/release.sh $tag" >&2
    exit 1
  }
done

export CODEHAVEN_TAG="$tag" CODEHAVEN_E2E_SUBNET CODEHAVEN_SHOTS_RUNNERS_SUBNET
# dind's guard needs the host's public addresses; the server's webhooks refuse the same ones.
export HOST_PUBLIC_IPS="$(bash "$dir/deploy/prod/host-ips.sh")"
export WEBHOOK_BLOCKED_IPS="$(bash "$dir/deploy/prod/host-ips.sh" --json)"
compose=(docker compose -p "$project" -f "$dir/deploy/compose.shots.yml")
public=https://shots.example.invalid
if [ -n "${SHOTS_DOGFOOD:-}" ]; then
  # The server's own fixed address on the runners network, which jobs use to reach it.
  SHOTS_SERVER_RUNNERS_IP="$(python3 -c 'import ipaddress,sys; print(list(ipaddress.ip_network(sys.argv[1]).hosts())[99])' "$CODEHAVEN_SHOTS_RUNNERS_SUBNET")"
  export SHOTS_SERVER_RUNNERS_IP
  compose+=(-f "$dir/deploy/compose.shots-dogfood.yml")
  public="http://$SHOTS_SERVER_RUNNERS_IP:3200"
fi

# Tears the whole project down. Safe when nothing was started, and never fails the script.
cleanup() {
  set +e
  # SHOTS_KEEP=1 leaves the stack up for debugging; remove it with `docker compose -p <project> down -v`.
  [ -z "${SHOTS_KEEP:-}" ] || { echo "shots: SHOTS_KEEP is set, leaving $project running" >&2; return; }
  "${compose[@]}" down -v --remove-orphans
  rm -rf "$work"/session.json "$work"/work
}
trap cleanup EXIT

# A leftover project from an earlier run would hold the subnet and the volumes.
"${compose[@]}" down -v --remove-orphans
"${compose[@]}" up -d postgres server dind work-init

st=""
for _ in $(seq 1 40); do
  st=$("${compose[@]}" ps --format '{{.Service}} {{.Health}}' | grep '^server ' | cut -d' ' -f2 || true)
  [ "$st" = healthy ] && break; sleep 3
done
[ "$st" = healthy ] || { echo "shots: the server did not become healthy" >&2; exit 1; }

ok=""
for _ in $(seq 1 60); do "${compose[@]}" exec -T dind docker info >/dev/null 2>&1 && { ok=1; break; }; sleep 2; done
[ -n "$ok" ] || { echo "shots: dind did not come up" >&2; exit 1; }

docker save codehaven-job:trixie | "${compose[@]}" exec -T dind docker load

# SHOTS_DOGFOOD=1 (ch-d9s): also the builder image and Postgres for Codehaven's own workflow, which
# the harness's step 26 pushes the source of this checkout to and waits for. A few GiB in dind.
if [ -n "${SHOTS_DOGFOOD:-}" ]; then
  builder="$(bash "$dir/deploy/dev/builder-image.sh")"
  docker image inspect postgres:17-alpine >/dev/null 2>&1 || docker pull -q postgres:17-alpine >/dev/null
  docker save "$builder" | "${compose[@]}" exec -T dind docker load
  docker save postgres:17-alpine | "${compose[@]}" exec -T dind docker load
  export E2E_SOURCE_DIR="$dir"
fi

# A fresh database knows no runner. The registration token comes from the server's own CLI and
# goes through the environment only, so it never lands in `ps` or the log.
tok=$("${compose[@]}" exec -T server codehaven admin runner-token | tail -1)
"${compose[@]}" run --rm -T -e HAVEN_RUNNER_URL=http://server:3200 -e HAVEN_RUNNER_TOKEN="$tok" \
  runner haven-runner register --replace --name shots-1 2>&1 | grep -i registered
"${compose[@]}" up -d runner
sleep 5

server_ip=$(docker inspect "$project-server-1" --format "{{(index .NetworkSettings.Networks \"${project}_shots\").IPAddress}}")

mkdir -p "$work" "$out"
cp "$dir/tests/e2e/e2e_full.py" "$dir/tests/e2e/shoot.mjs" "$dir/tests/e2e/workspace.mjs" "$dir/tests/e2e/livelog.mjs" "$dir/tests/e2e/terminal.mjs" "$work/"
# playwright-core lives in $CODEHAVEN_SHOTS_NODE_MODULES; shoot.mjs uses `import`, so the work dir
# needs a package.json that says ESM.
ln -sfn "$CODEHAVEN_SHOTS_NODE_MODULES" "$work/node_modules"
printf '{"name":"shots","private":true,"type":"module"}\n' > "$work/package.json"

# The host is the gateway of the `shots` subnet, which the server trusts.
gateway=$(python3 -c 'import ipaddress,sys; print(next(ipaddress.ip_network(sys.argv[1]).hosts()))' "$CODEHAVEN_E2E_SUBNET")
export E2E_BASE="http://$server_ip:3200" E2E_PUBLIC="$public" E2E_SSH="$server_ip:2222"
export E2E_HOST_IP="$gateway" E2E_STEPS="${SHOTS_STEPS:-0-20}" E2E_TRACE=1 E2E_SHOTS="$work/session.json" E2E_PROJECT="$project"
export E2E_CLI="docker compose -p $project -f $dir/deploy/compose.shots.yml ${SHOTS_DOGFOOD:+-f $dir/deploy/compose.shots-dogfood.yml} exec -T server codehaven"
rc=0
python3 "$work/e2e_full.py" || rc=$?
[ "$rc" = 0 ] || exit "$rc"
# SHOTS_NO_SHOTS=1: the steps were the point, there is nothing to photograph.
[ -z "${SHOTS_NO_SHOTS:-}" ] || exit 0

rm -rf "$out"/*.png "$out/results.json"
# shoot.mjs exits non-zero when it finds problems in the pages it shot: keep that exit code, but
# let the script reach its last line first (the trap cleans up either way).
(cd "$work" && node shoot.mjs "$E2E_BASE" "$work/session.json" "$out") || rc=$?
echo "shots: $(find "$out" -name '*.png' | wc -l) PNGs in $out"
exit "$rc"
