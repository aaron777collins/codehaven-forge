#!/usr/bin/env bash
# deploy/tests/live-guard.sh NAME: proves the things that start, seed or wipe a test stack cannot
# touch an account on a stack that holds one, and that none of them will act on the instance people
# use. Run on the live host, as root, from a synced checkout.
#
#   bash deploy/tests/live-guard.sh test-cc
#
# It brings up one throwaway e2e project (every service labelled org.codehaven.throwaway=true by
# deploy/compose.e2e.yml), gives it the one account alice, then points every harness and script at
# it, at a project name that is not a throwaway one, and at a project holding an ordinary container. Each one must
# refuse or do nothing, and the account count must stay 1 throughout: that count is the point of
# the whole script. Nothing outside the throwaway project and the two containers named NAME-plain
# and NAME-inuse is created, and the trap removes exactly those.
#
# Needs codehaven-server:NAME, codehaven-e2e:NAME, and CODEHAVEN_E2E_SUBNET in the env file. Output
# that may hold a one-time link goes to a file under the scratch dir and is never printed.
set -uo pipefail

CH_SCRIPT=live-guard
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
. "$SRC/deploy/lib/env.sh"
. "$SRC/deploy/lib/throwaway.sh"

NAME="${1:-}"
if [ "$#" -ne 1 ] || ! [[ "$NAME" =~ ^test-[a-z0-9]+$ ]]; then
  ch_die "usage: deploy/tests/live-guard.sh NAME, where NAME matches ^test-[a-z0-9]+\$"
fi

pass=0 fail=0
ok()     { printf 'ok    %s\n' "$*"; pass=$((pass + 1)); }
bad()    { printf 'FAIL  %s\n' "$*"; fail=$((fail + 1)); }
want0()  { [ "$1" -eq 0 ] && ok "$2" || bad "$2"; }    # want0 <exit-code> <name>
wantfail(){ [ "$1" -ne 0 ] && ok "$2" || bad "$2"; }   # wantfail <exit-code> <name>
check()  { local name="$1"; shift; if "$@" >"$SCRATCH/last.out" 2>&1; then ok "$name"; else bad "$name"; fi; }

# What this run needs: the env file with a subnet for throwaway stacks, and the two images.
ch_env_load
[ -n "${CODEHAVEN_E2E_SUBNET:-}" ] || ch_die "CODEHAVEN_E2E_SUBNET is not set in $CH_ENV_LOADED (a free /20 for throwaway stacks)"
for img in "codehaven-server:$NAME" "codehaven-e2e:$NAME"; do
  docker image inspect "$img" >/dev/null 2>&1 || ch_die "$img is missing locally. Build it first: bash deploy/dev/release.sh $NAME"
done

PROJECT="e2e-${NAME#test-}"
# CODEHAVEN_TAG is exported for compose only inside cc. The scripts under test run without it, so
# the env file's own tag (the one the real instance runs) is what they compare against.
cc() { CODEHAVEN_TAG="$NAME" docker compose -p "$PROJECT" -f "$SRC/deploy/compose.e2e.yml" "$@"; }

SCRATCH="/var/tmp/ch-$NAME-guard"
[ ! -e "$SCRATCH" ] || ch_die "$SCRATCH already exists; remove it first"
mkdir -p "$SCRATCH"

# Removes only what this script made: its own compose project, the two containers it named, its
# scratch dir. Never another project, never anything unlabelled that it did not create.
cleanup() {
  rc=$?
  set +e
  cc down -v --remove-orphans >/dev/null 2>&1
  docker rm -f "$NAME-plain" "$NAME-inuse" >/dev/null 2>&1
  rm -rf "$SCRATCH"
  exit "$rc"
}
trap cleanup EXIT

# accounts(): the one number the whole script defends. running(): is a container we made up?
accounts() { cc exec -T server codehaven admin users 2>/dev/null | grep -c . || true; }
running()  { [ "$(docker inspect --format '{{.State.Running}}' "$1" 2>/dev/null)" = true ]; }
one_left() { [ "$(accounts)" = 1 ] && ok "still exactly 1 account ($1)" || bad "the account count moved ($1)"; }

printf 'live-guard %s: throwaway project %s, checkout %s\n' "$NAME" "$PROJECT" "$SRC"

# --- setup: a throwaway stack that holds exactly one account --------------------------------------

cc up -d --wait postgres server >"$SCRATCH/up.out" 2>&1
want0 "$?" "the throwaway stack $PROJECT is up"
cc exec -T server codehaven admin create-user --username alice --email alice@example.com --admin \
  >"$SCRATCH/create.out" 2>&1
want0 "$?" "alice was created (the one-time link stays in the scratch dir)"
[ "$(accounts)" = 1 ] && ok "the stack holds exactly 1 account" || bad "the stack does not hold exactly 1 account"

CLI_THIS="docker compose -p $PROJECT -f $SRC/deploy/compose.e2e.yml exec -T server codehaven"
CLI_OTHER="docker compose -p keep-${NAME#test-} -f $SRC/deploy/compose.e2e.yml exec -T server codehaven"

# --- a. the Rust harness -----------------------------------------------------------------------------

cc run --rm --no-deps -T -e E2E_STEPS=0-1 e2e >"$SCRATCH/rust.out" 2>&1
rc=$?
if [ "$rc" -ne 0 ] && grep -q 'already holds' "$SCRATCH/rust.out"; then
  ok "the Rust harness refuses a stack that holds an account"
else
  bad "the Rust harness did not refuse (rc=$rc)"
fi
one_left "after the Rust harness"

# --- b. the Python harness, same target ----------------------------------------------------------------

# The harness only talks to an E2E_BASE that is one of the project's own container addresses, so
# the account check is reached with the server's address. A loopback address (where live listens on
# this host) is refused before any request.
SERVER_IP="$(docker inspect --format '{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}' "$(cc ps -q server)" 2>/dev/null | awk '{print $1}')"
[ -n "$SERVER_IP" ] && ok "the throwaway server has an address" || bad "the throwaway server has no address"
E2E_PUBLIC=https://code.example.com E2E_BASE="http://$SERVER_IP:3200" E2E_PROJECT="$PROJECT" \
  E2E_CLI="$CLI_THIS" E2E_STEPS=0-1 python3 "$SRC/tests/e2e/e2e_full.py" >"$SCRATCH/py-holds.out" 2>&1
rc=$?
if [ "$rc" -ne 0 ] && grep -q 'already holds' "$SCRATCH/py-holds.out"; then
  ok "the python harness refuses a stack that holds an account"
else
  bad "the python harness did not refuse (rc=$rc)"
fi
one_left "after the python harness"

E2E_PUBLIC=https://code.example.com E2E_BASE=http://127.0.0.1:3200 E2E_PROJECT="$PROJECT" \
  E2E_CLI="$CLI_THIS" E2E_STEPS=0-1 python3 "$SRC/tests/e2e/e2e_full.py" >"$SCRATCH/py-loop.out" 2>&1
rc=$?
if [ "$rc" -ne 0 ] && grep -q 'is not the address of a container' "$SCRATCH/py-loop.out"; then
  ok "the python harness refuses an E2E_BASE that is not the project's own server"
else
  bad "the python harness accepted a loopback E2E_BASE (rc=$rc)"
fi
one_left "after the python harness with a loopback E2E_BASE"

# --- c. the Python harness pointed at a project that is not a throwaway one ---------------------------------------------------

E2E_PUBLIC=https://code.example.com E2E_BASE=http://127.0.0.1:1 E2E_PROJECT="keep-${NAME#test-}" \
  E2E_CLI="$CLI_THIS" E2E_STEPS=0-1 python3 "$SRC/tests/e2e/e2e_full.py" >"$SCRATCH/py-live.out" 2>&1
rc=$?
if [ "$rc" -ne 0 ] && grep -q 'must name a throwaway compose project' "$SCRATCH/py-live.out"; then
  ok "the python harness refuses a project that is not a throwaway one"
else
  bad "the python harness did not refuse a project that is not a throwaway one (rc=$rc)"
fi
one_left "after the python harness with E2E_PROJECT=keep-..."

E2E_PUBLIC=https://code.example.com E2E_BASE=http://127.0.0.1:1 E2E_PROJECT="$PROJECT" \
  E2E_CLI="$CLI_OTHER" E2E_STEPS=0-1 python3 "$SRC/tests/e2e/e2e_full.py" >"$SCRATCH/py-cli.out" 2>&1
rc=$?
if [ "$rc" -ne 0 ] && grep -q 'must address the throwaway project' "$SCRATCH/py-cli.out"; then
  ok "the python harness refuses an E2E_CLI that addresses another project"
else
  bad "the python harness accepted a stray E2E_CLI (rc=$rc)"
fi
one_left "after the python harness with a stray E2E_CLI"

# --- d. deploy/dev/e2e.sh ---------------------------------------------------------------------------------

env -u CODEHAVEN_TAG bash "$SRC/deploy/dev/e2e.sh" --tag "keep-$NAME" >"$SCRATCH/e2e-dev.out" 2>&1
wantfail "$?" "e2e.sh refuses a tag that is not a throwaway one"
grep -q 'not a throwaway tag' "$SCRATCH/e2e-dev.out" && ok "it says the tag is not a throwaway one" || bad "e2e.sh --tag keep-... gave no reason"

env -u CODEHAVEN_TAG bash "$SRC/deploy/dev/e2e.sh" --tag "$NAME" --project "keep-${NAME#test-}" >"$SCRATCH/e2e-live.out" 2>&1
wantfail "$?" "e2e.sh refuses a project that is not a throwaway one"
grep -q 'not a throwaway name' "$SCRATCH/e2e-live.out" && ok "it says the project is not a throwaway one" || bad "e2e.sh --project keep-... gave no reason"

docker run -d --name "$NAME-plain" --label com.docker.compose.project=test-plain \
  --entrypoint sleep "codehaven-server:$NAME" 600 >/dev/null 2>&1
want0 "$?" "an ordinary container of the fake project test-plain runs"
env -u CODEHAVEN_TAG bash "$SRC/deploy/dev/e2e.sh" --tag "$NAME" --project test-plain --no-build >"$SCRATCH/e2e-plain.out" 2>&1
wantfail "$?" "e2e.sh refuses a project that holds an ordinary container"
grep -q 'already holds a container that is not a throwaway one' "$SCRATCH/e2e-plain.out" \
  && ok "it says the project already holds a container that is not a throwaway one" || bad "e2e.sh --project test-plain gave no reason"
check "the ordinary container is still running" running "$NAME-plain"
one_left "after the e2e.sh refusals"

# --- e. deploy/dev/shots.sh: the same three refusals ---------------------------------------------------------

env -u CODEHAVEN_TAG bash "$SRC/deploy/dev/shots.sh" --tag "keep-$NAME" >"$SCRATCH/shots-dev.out" 2>&1
wantfail "$?" "shots.sh refuses a tag that is not a throwaway one"
grep -q 'not a throwaway tag' "$SCRATCH/shots-dev.out" && ok "it says the tag is not a throwaway one" || bad "shots.sh --tag keep-... gave no reason"

env -u CODEHAVEN_TAG bash "$SRC/deploy/dev/shots.sh" --tag "$NAME" --project "keep-${NAME#test-}" >"$SCRATCH/shots-live.out" 2>&1
wantfail "$?" "shots.sh refuses a project that is not a throwaway one"
grep -q 'not a throwaway name' "$SCRATCH/shots-live.out" && ok "it says the project is not a throwaway one" || bad "shots.sh --project keep-... gave no reason"

env -u CODEHAVEN_TAG bash "$SRC/deploy/dev/shots.sh" --tag "$NAME" --project test-plain >"$SCRATCH/shots-plain.out" 2>&1
wantfail "$?" "shots.sh refuses a project that holds an ordinary container"
grep -q 'already holds a container that is not a throwaway one' "$SCRATCH/shots-plain.out" \
  && ok "it says the project already holds a container that is not a throwaway one" || bad "shots.sh --project test-plain gave no reason"
check "the ordinary container is still running" running "$NAME-plain"
one_left "after the shots.sh refusals"

# --- f. deploy/dev/release.sh refuses a tag in use -------------------------------------------------------------
# The tag-in-use check is the first thing release.sh does after parsing, before diskcheck, the
# build slots and buildx, so this refusal means nothing was built or started. Never pass --deploy.

docker run -d --name "$NAME-inuse" --entrypoint sleep "codehaven-server:$NAME" 600 >/dev/null 2>&1
want0 "$?" "an ordinary container runs the image codehaven-server:$NAME"
env -u CH_STREAM bash "$SRC/deploy/dev/release.sh" "$NAME" >"$SCRATCH/release.out" 2>&1
wantfail "$?" "release.sh refuses to replace a tag a running container uses"
grep -q 'Pick another tag, or pass --deploy' "$SCRATCH/release.out" \
  && ok "it says to pick another tag or pass --deploy" || bad "release.sh gave no reason"
one_left "after the release.sh refusal"

# --- g. the ordinary containers were never removed by any of the above -------------------------------------------

check "the plain container is still running after d, e and f" running "$NAME-plain"
check "the in-use container is still running after d, e and f" running "$NAME-inuse"

# --- h. first-admin.sh is not run here -----------------------------------------------------------------------
# deploy/prod/first-admin.sh is covered end to end by deploy/tests/clean-clone.sh: it creates the
# account there and refuses the second run, so it is not pointed at this stack.

# --- i. nothing that can wipe an instance ships in the repository ----------------------------------------------
# The character classes below keep this grep from matching its own line in this file.

strays="$(ls "$SRC/deploy/prod" "$SRC/deploy/dev" 2>/dev/null | grep -xE 'reset-live\.sh|live-e2e\.sh|switch-test[3]\.sh')"
[ -z "$strays" ] && ok "no reset script ships in deploy/prod or deploy/dev" || bad "deploy ships a reset script: $strays"
wipers="$(grep -rIl -e 'DROP[ ]SCHEMA' -e 'TRUNCA[T]E' "$SRC/deploy" "$SRC/Makefile" 2>/dev/null)"
[ -z "$wipers" ] && ok "no schema drop or table truncate in deploy or the Makefile" || bad "found in: $wipers"

# --- the count ----------------------------------------------------------------------------------------------------

printf 'live-guard: %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
