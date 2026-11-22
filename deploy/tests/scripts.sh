#!/usr/bin/env bash
# Tests for the deploy shell scripts. Section A: nothing runs ssh or rsync unless CH_HOST and
# CH_REMOTE_ROOT say where to build. Section B: compose.sh stops naming the setting when a
# required one is missing. Section C: deploy/lib/env.sh finds and parses the env file. Section D:
# the throwaway guards. The whole run works on a copy of this checkout in a scratch directory,
# so the real checkout is only read. No Docker daemon is needed: section B uses the docker CLI
# with compose and nothing else, and skips itself when that is missing.
# Written for bash 3.2 (macOS) as well as bash 5: no arrays, no mapfile, no ${var,,}.
set -u

# --- reporting -----------------------------------------------------------------------------

passed=0
failed=0
skipped=0

ok()   { printf 'ok    %s\n' "$1"; passed=$((passed + 1)); }
bad()  { printf 'FAIL  %s\n' "$1"; failed=$((failed + 1)); }
skip() { printf 'skip  %s\n' "$1"; skipped=$((skipped + 1)); }

# check <name> <cmd...>: pass when the command exits 0.
check() {
  local name="$1"; shift
  if "$@"; then ok "$name"; else bad "$name"; fi
}

# --- the scratch tree ----------------------------------------------------------------------

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)" || { echo 'cannot find the checkout' >&2; exit 1; }
T=$(mktemp -d) || { echo 'mktemp failed' >&2; exit 1; }
trap 'rm -rf "$T"' EXIT
TREE="$T/tree"

mkdir "$TREE" || exit 1
tar -C "$ROOT" --exclude=./.git --exclude=./target --exclude=./.worktrees --exclude=./dist -cf - . |
  tar -C "$TREE" -xf -
for f in Makefile deploy/lib/env.sh deploy/lib/throwaway.sh deploy/compose.sh; do
  [ -f "$TREE/$f" ] || { echo "the scratch copy has no $f" >&2; exit 1; }
done
# Files of the machine this runs on must not travel with the copy.
rm -f "$TREE/deploy/dev/local.env" "$TREE/.env"
rm -rf "$TREE/.codehaven"

(
  cd "$TREE" || exit 1
  git init -q 2>/dev/null
  git config user.name Test
  git config user.email test@example.com
  git -c core.hooksPath=/dev/null add -A
  git -c core.hooksPath=/dev/null commit -q -m scratch
) || { echo 'cannot commit the scratch copy' >&2; exit 1; }

# --- shared helpers ------------------------------------------------------------------------

# unset_test_env: remove every variable the deploy scripts read, so nothing leaks in from the
# machine running the tests. Always called inside the subshell that runs the command under test.
unset_test_env() {
  local v names
  names=$(env | LC_ALL=C sed -n 's/^\([A-Za-z_][A-Za-z0-9_]*\)=.*/\1/p')
  for v in $names; do
    case "$v" in
      CH_*|CODEHAVEN_*|HOST_PUBLIC_IPS|WEBHOOK_BLOCKED_IPS) unset "$v" ;;
    esac
  done
}

# no_ssh_calls: true when neither fake tool was called since the log was last cleared.
no_ssh_calls() {
  ! grep -Eq '^(ssh|rsync) ' "$CALLS"
}

# ssh_to_build_host: ssh was called, and every call went to the one host CH_HOST named.
ssh_to_build_host() {
  local n wrong
  n=$(grep -c '^ssh ' "$CALLS" || true)
  [ "$n" -gt 0 ] || return 1
  wrong=$(grep '^ssh ' "$CALLS" | grep -cv '^ssh build.example.com ' || true)
  [ "$wrong" -eq 0 ]
}

printf '\n# section A: no ssh unless asked\n'

# --- section A -----------------------------------------------------------------------------

BIN="$T/bin"
CALLS="$T/calls.log"
mkdir "$BIN"
: > "$CALLS"

for tool in ssh rsync docker; do
  cat > "$BIN/$tool" <<FAKE
#!/usr/bin/env bash
printf '%s %s\n' $tool "\$*" >> "$CALLS"
FAKE
  if [ "$tool" = docker ]; then
    printf '%s\n' 'if [ "$1" = compose ] && [ "$2" = version ]; then echo 2.30.0; exit 0; fi' >> "$BIN/$tool"
  fi
  printf 'exit 1\n' >> "$BIN/$tool"
  chmod +x "$BIN/$tool"
done

# A harness check first: a vacuous pass later would hide a broken fake.
rc=0
"$BIN/ssh" self-test >/dev/null 2>&1 || rc=$?
if [ "$rc" = 1 ] && grep -qx 'ssh self-test' "$CALLS"; then
  ok 'A: the fake tools record their calls'
else
  bad 'A: the fake tools record their calls'
fi

# A plain .env so the env file exists; every path then builds on this machine.
cp "$TREE/.env.example" "$TREE/.env"

# a_make <make-args...>: make in the scratch tree, fake bin first on PATH, clean environment.
a_make() (
  cd "$TREE" || exit 99
  PATH="$BIN:$PATH"
  unset_test_env
  make "$@" >/dev/null 2>&1
)

for spec in test lint fmt ci sqlx sqlx-check docker-test up down clean disk 'e2e E2E_TAG=test-x' 'release TAG=test-x'; do
  : > "$CALLS"
  a_make $spec   # unquoted: the two targets carry a variable in the spec
  check "A: make $spec uses no ssh or rsync" no_ssh_calls
done

# run_dev <script> [VAR=VALUE...]: a dev script with two arguments, fake bin on PATH, clean
# environment, then the listed variables set. Output lands in $T/out.
run_dev() {
  local script="$1" kv k v; shift
  (
    cd "$TREE" || exit 99
    PATH="$BIN:$PATH"
    unset_test_env
    for kv in "$@"; do
      k="${kv%%=*}"; v="${kv#*=}"
      export "$k=$v"
    done
    bash "deploy/dev/$script" foundation true > "$T/out" 2>&1
  )
}

: > "$CALLS"
if ! run_dev host.sh && grep -q CH_HOST "$T/out" && no_ssh_calls; then
  ok 'A: host.sh with no CH_HOST stops, no ssh'
else
  bad 'A: host.sh with no CH_HOST stops, no ssh'
fi

: > "$CALLS"
if ! run_dev remote.sh && grep -q CH_HOST "$T/out" && no_ssh_calls; then
  ok 'A: remote.sh with no CH_HOST stops, no ssh'
else
  bad 'A: remote.sh with no CH_HOST stops, no ssh'
fi

: > "$CALLS"
if ! run_dev host.sh CH_HOST=build.example.com && grep -q CH_REMOTE_ROOT "$T/out" && no_ssh_calls; then
  ok 'A: host.sh with no CH_REMOTE_ROOT stops, no ssh'
else
  bad 'A: host.sh with no CH_REMOTE_ROOT stops, no ssh'
fi

: > "$CALLS"
if ! run_dev remote.sh CH_HOST=build.example.com && grep -q CH_REMOTE_ROOT "$T/out" && no_ssh_calls; then
  ok 'A: remote.sh with no CH_REMOTE_ROOT stops, no ssh'
else
  bad 'A: remote.sh with no CH_REMOTE_ROOT stops, no ssh'
fi

a_remote_make() (
  cd "$TREE" || exit 99
  PATH="$BIN:$PATH"
  unset_test_env
  CH_HOST=build.example.com
  CH_REMOTE_ROOT=/srv/x
  export CH_HOST CH_REMOTE_ROOT
  make test >/dev/null 2>&1
)

: > "$CALLS"
a_remote_make
check 'A: with CH_HOST set, make test ssh to that host only' ssh_to_build_host

printf '\n# section B: the settings compose.sh requires\n'

# --- section B -----------------------------------------------------------------------------

# The real docker, original PATH: this section only renders, it starts nothing.
if ! docker compose version >/dev/null 2>&1; then
  skip 'B: the whole section (docker compose is not available)'
else
  BASE_ENV="$T/base.env"
  base_env() {
    cat <<'ENVEOF'
CODEHAVEN_PROJECT=test-keys
CODEHAVEN_TAG=test-keys
CODEHAVEN_EXTERNAL_URL=https://code.example.com
CODEHAVEN_NAME=Codehaven
CODEHAVEN_DATA_DIR=/tmp/ch-keys/data
CODEHAVEN_SECRETS_DIR=/tmp/ch-keys/secrets
CODEHAVEN_TRUSTED_PROXIES='[]'
CODEHAVEN_HTTP_BIND=127.0.0.1:13200
CODEHAVEN_SSH_BIND=127.0.0.1:12222
CODEHAVEN_SSH_EXTERNAL=127.0.0.1:12222
CODEHAVEN_PROXY=none
CODEHAVEN_PROXY_NETWORK=proxy
CODEHAVEN_PROXY_ALIAS=codehaven-web
CODEHAVEN_SITE_HOST=code.example.com
CODEHAVEN_EDGE_SUBNET=10.20.16.0/24
ENVEOF
  }
  base_env > "$BASE_ENV"

  # write_env <file> <key>...: the base env without the lines that set the named keys.
  write_env() {
    local f="$1" k; shift
    cp "$BASE_ENV" "$f"
    for k in "$@"; do
      grep -v "^$k=" "$f" > "$f.new" || true
      mv "$f.new" "$f"
    done
  }

  # compose_run <env-file> <compose-args...>: compose.sh against one env file. It runs from
  # outside the tree so docker compose cannot silently load the tree's own .env as well. The
  # exit status lands in $run_rc, the combined output in $run_out. HOST_PUBLIC_IPS and
  # WEBHOOK_BLOCKED_IPS are set in the environment so compose.sh never runs host-ips.sh, which
  # needs the Linux ip command.
  compose_run() {
    local envfile="$1"; shift
    rm -f "$T/rc" "$T/out"
    (
      unset_test_env
      cd "$T" || exit 99
      CH_ENV_FILE="$envfile"
      HOST_PUBLIC_IPS=
      WEBHOOK_BLOCKED_IPS='[]'
      export CH_ENV_FILE HOST_PUBLIC_IPS WEBHOOK_BLOCKED_IPS
      bash "$TREE/deploy/compose.sh" "$@" > "$T/out" 2>&1
      echo $? > "$T/rc"
    ) >/dev/null 2>&1
    run_rc=$(cat "$T/rc" 2>/dev/null || true)
    run_out=$(cat "$T/out" 2>/dev/null || true)
    [ -n "$run_rc" ] || run_rc=999
  }

  compose_run "$BASE_ENV" --profile ci config
  if [ "$run_rc" = 0 ]; then
    ok 'B: with every key set the render works'
  else
    bad 'B: with every key set the render works'
  fi

  if [ "$run_rc" = 0 ]; then
    # CODEHAVEN_TAG is not in the loop: compose.sh derives it from git when it is unset, so the
    # render must not fail. It gets its own check below.
    for key in CODEHAVEN_PROJECT CODEHAVEN_EXTERNAL_URL CODEHAVEN_DATA_DIR CODEHAVEN_SECRETS_DIR \
               CODEHAVEN_TRUSTED_PROXIES CODEHAVEN_HTTP_BIND CODEHAVEN_SSH_BIND \
               CODEHAVEN_SSH_EXTERNAL CODEHAVEN_PROXY; do
      write_env "$T/k.env" "$key"
      compose_run "$T/k.env" --profile ci config
      if [ "$run_rc" != 0 ] && printf '%s' "$run_out" | grep -Fq "$key"; then
        ok "B: $key missing stops naming it"
      else
        bad "B: $key missing stops naming it"
      fi
    done

    write_env "$T/k.env" CODEHAVEN_TAG
    compose_run "$T/k.env" --profile ci config
    short=$(git -C "$TREE" rev-parse --short=12 HEAD 2>/dev/null || true)
    if [ "$run_rc" = 0 ] && [ -n "$short" ] && printf '%s' "$run_out" | grep -Fq "codehaven-server:$short"; then
      ok 'B: with no CODEHAVEN_TAG the commit id is the tag'
    else
      bad 'B: with no CODEHAVEN_TAG the commit id is the tag'
    fi

    # b_proxy <name> <proxy-value> <key>: the env with CODEHAVEN_PROXY=<proxy-value> and the key
    # removed must stop, and the message must name the key.
    b_proxy() {
      local name="$1" proxy="$2" key="$3"
      write_env "$T/p.env" CODEHAVEN_PROXY "$key"
      printf 'CODEHAVEN_PROXY=%s\n' "$proxy" >> "$T/p.env"
      compose_run "$T/p.env" --profile ci config
      if [ "$run_rc" != 0 ] && printf '%s' "$run_out" | grep -Fq "$key"; then
        ok "$name"
      else
        bad "$name"
      fi
    }
    b_proxy 'B: network proxy needs CODEHAVEN_PROXY_NETWORK' network CODEHAVEN_PROXY_NETWORK
    b_proxy 'B: network proxy needs CODEHAVEN_PROXY_ALIAS' network CODEHAVEN_PROXY_ALIAS
    b_proxy 'B: caddy proxy needs CODEHAVEN_SITE_HOST' caddy CODEHAVEN_SITE_HOST
    b_proxy 'B: caddy proxy needs CODEHAVEN_EDGE_SUBNET' caddy CODEHAVEN_EDGE_SUBNET
    b_proxy 'B: an unknown proxy value is refused' bogus CODEHAVEN_PROXY

    # Not covered: WEBHOOK_BLOCKED_IPS unset with HOST_PUBLIC_IPS set, which would run
    # host-ips.sh. That needs a Linux host, so this file does not exercise it.

    compose_run "$BASE_ENV" --profile ci config
    # Built by concatenation so this test file never holds the strings it looks for.
    leak_name1='aa';    leak_name1="$leak_name1"'ron'
    leak_name2='col';   leak_name2="$leak_name2"'lins'
    leak_host='de';     leak_host="$leak_host"'v3'
    leak_tag='te';      leak_tag="$leak_tag"'st3'
    leak_path='/opt/';  leak_path="$leak_path"'codehaven'
    if [ "$run_rc" = 0 ] \
      && ! printf '%s' "$run_out" | grep -Fq "$leak_name1" \
      && ! printf '%s' "$run_out" | grep -Fq "$leak_name2" \
      && ! printf '%s' "$run_out" | grep -Fq "$leak_host" \
      && ! printf '%s' "$run_out" | grep -Fq "$leak_tag" \
      && ! printf '%s' "$run_out" | grep -Fq "$leak_path"; then
      ok 'B: the render holds nothing of one deployment'
    else
      bad 'B: the render holds nothing of one deployment'
    fi
  else
    skip 'B: the rest of the section (the control render failed)'
  fi
fi

printf '\n# section C: env loading\n'

# --- section C -----------------------------------------------------------------------------

cp "$TREE/.env.example" "$TREE/.env"

# c_run: compose.sh with no CH_ENV_FILE, so env.sh has to find the file itself. Runs outside
# the tree, like section B.
c_run() (
  cd "$T" || exit 99
  unset_test_env
  bash "$TREE/deploy/compose.sh" config > "$T/out" 2>&1
)

# 1. Two env files at once are refused.
printf 'CODEHAVEN_PROJECT=x\n' > "$T/codehaven.env"
if ! c_run && grep -qF 'two env files' "$T/out"; then
  ok 'C: two env files are refused'
else
  bad 'C: two env files are refused'
fi
rm -f "$T/codehaven.env"

# 2. No env file at all points at .env.example.
rm -f "$TREE/.env"
if ! c_run && grep -qF '.env.example' "$T/out"; then
  ok 'C: no env file points at .env.example'
else
  bad 'C: no env file points at .env.example'
fi
cp "$TREE/.env.example" "$TREE/.env"

# 3. A value that looks like a command is stored, never run.
cat > "$T/evil.env" <<EVIL
CODEHAVEN_NAME=\$(touch $T/pwned)
CODEHAVEN_X="a b"
EVIL
rm -f "$T/pwned"
xval=$(
  (
    unset_test_env
    . "$TREE/deploy/lib/env.sh"
    ch_parse_file "$T/evil.env" || exit 1
    printf '%s' "${CODEHAVEN_X:-}"
  ) 2>/dev/null
)
rc=$?
if [ "$rc" = 0 ] && [ ! -e "$T/pwned" ] && [ "$xval" = 'a b' ]; then
  ok 'C: a value is never executed'
else
  bad 'C: a value is never executed'
fi

# 4. A variable already in the environment wins over the file.
printf 'CODEHAVEN_NAME=from the file\n' > "$T/win.env"
xval=$(
  (
    unset_test_env
    CODEHAVEN_NAME='from the environment'
    export CODEHAVEN_NAME
    . "$TREE/deploy/lib/env.sh"
    ch_parse_file "$T/win.env" || exit 1
    printf '%s' "${CODEHAVEN_NAME:-}"
  ) 2>/dev/null
)
if [ "$xval" = 'from the environment' ]; then
  ok 'C: a variable in the environment wins'
else
  bad 'C: a variable in the environment wins'
fi

# 5. A line that is not KEY=value is refused.
printf 'this line is not an assignment\n' > "$T/bad.env"
if ! (
  unset_test_env
  . "$TREE/deploy/lib/env.sh" 2>/dev/null
  ch_parse_file "$T/bad.env"
) >/dev/null 2>&1; then
  ok 'C: a line that is not KEY=value is refused'
else
  bad 'C: a line that is not KEY=value is refused'
fi

printf '\n# section D: throwaway guards\n'

# --- section D -----------------------------------------------------------------------------

d_guard() (
  unset_test_env
  CH_SCRIPT=t
  CODEHAVEN_PROJECT=codehaven
  CODEHAVEN_TAG=abc123
  export CH_SCRIPT CODEHAVEN_PROJECT CODEHAVEN_TAG
  . "$TREE/deploy/lib/env.sh"
  . "$TREE/deploy/lib/throwaway.sh"
  ch_require_throwaway "$@"
)

check 'D: test-x with test-x is allowed' d_guard test-x test-x

d_refuse() {
  if d_guard "$1" "$2" >/dev/null 2>&1; then bad "D: $1 with $2 refused"; else ok "D: $1 with $2 refused"; fi
}
d_refuse codehaven test-x
d_refuse test-x abc123
d_refuse test-x dev
d_refuse prod-x test-x
d_refuse test-x latest
d_refuse test- test-x

# --- the count -----------------------------------------------------------------------------

printf '\n# %d passed, %d failed, %d skipped\n' "$passed" "$failed" "$skipped"
[ "$failed" -eq 0 ] || exit 1
exit 0
