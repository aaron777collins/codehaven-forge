#!/usr/bin/env bash
# deploy/tests/clean-clone.sh NAME: the stranger test. From a clean copy of the checkout to a first
# admin, a runner, a backup, a restore into a second empty project, an upgrade and a rollback, on
# one Linux host with Docker, using images that already exist. Run as root.
#
#   bash deploy/tests/clean-clone.sh test-cc
#
# NAME must match ^test-[a-z0-9]+$. The images codehaven-server:NAME, codehaven-runner:NAME and
# codehaven-job:trixie must exist locally. Everything the script makes (the copy under
# /var/tmp/ch-NAME, the compose projects NAME and NAME-r, the network NAME-proxy) is removed by the
# trap, and nothing outside it is touched: no docker command runs against anything this script did
# not create and name after NAME. Output that may hold a link, token or password goes to a file
# under the scratch dir, is never printed, and is removed with the rest; only exit codes and counts
# are reported.
#
# On a host whose Docker address pools are used up, give the networks fixed subnets:
#   CH_CLEAN_CLONE_SUBNETS="10.20.40.0/24 10.20.41.0/24 10.20.42.0/24 10.20.43.0/24" bash deploy/tests/clean-clone.sh test-cc
set -uo pipefail

CH_SCRIPT=clean-clone
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
. "$SRC/deploy/lib/env.sh"
. "$SRC/deploy/lib/throwaway.sh"

NAME="${1:-}"
if [ "$#" -ne 1 ] || ! [[ "$NAME" =~ ^test-[a-z0-9]+$ ]]; then
  ch_die "usage: deploy/tests/clean-clone.sh NAME, where NAME matches ^test-[a-z0-9]+\$"
fi

pass=0 fail=0
ok()     { printf 'ok    %s\n' "$*"; pass=$((pass + 1)); }
bad()    { printf 'FAIL  %s\n' "$*"; fail=$((fail + 1)); }
want0()  { [ "$1" -eq 0 ] && ok "$2" || bad "$2"; }    # want0 <exit-code> <name>
wantfail(){ [ "$1" -ne 0 ] && ok "$2" || bad "$2"; }   # wantfail <exit-code> <name>
check()  { local name="$1"; shift; if "$@" >"$SCRATCH/last.out" 2>&1; then ok "$name"; else bad "$name"; fi; }

# ch_run <envfile> <cmd...>: run a command with only that env file speaking. Every CODEHAVEN_*
# variable of this shell is dropped first (env -u), so the file is the single source.
ch_run() {
  local envfile="$1" k; shift
  local -a cmd=(env)
  for k in $(env | grep -E '^CODEHAVEN_[A-Za-z0-9_]+=' | cut -d= -f1); do cmd+=(-u "$k"); done
  cmd+=("CH_ENV_FILE=$envfile")
  "${cmd[@]}" "$@"
}

# run_out <file> <envfile> <cmd...>: ch_run with all output kept in the file (and the exit code in
# RC). The file lives under the scratch dir and is never printed: it may hold a link or a password.
run_out() { local out="$1" envfile="$2"; shift 2; ch_run "$envfile" "$@" >"$out" 2>&1; RC=$?; }

# set_env <file> <key> <value>: write KEY=value, replacing the uncommented line or appending. None
# of the values used here carries a | or an &, which sed would otherwise read.
set_env() {
  local file="$1" key="$2" val="$3"
  if grep -q "^$key=" "$file"; then sed -i "s|^$key=.*|$key=$val|" "$file"
  else printf '%s=%s\n' "$key" "$val" >>"$file"; fi
}

# users <envfile>: how many accounts `codehaven admin users` lists for that project.
users() { ch_run "$1" bash "$CO/deploy/compose.sh" exec -T server codehaven admin users 2>/dev/null | grep -c . || true; }

# wait_healthy <envfile>: block until compose ps shows the server healthy, or give up after 3 min.
wait_healthy() {
  local i st
  for i in $(seq 1 60); do
    st=$(ch_run "$1" bash "$CO/deploy/compose.sh" ps --format '{{.Service}} {{.Health}}' 2>/dev/null | grep '^server ' | cut -d' ' -f2)
    [ "$st" = healthy ] && return 0
    sleep 3
  done
  return 1
}

# --- what this run needs on the host ------------------------------------------------------------

HTTP1=13300 SSH1=13322 HTTP2=13301 SSH2=13323
for p in "$HTTP1" "$SSH1" "$HTTP2" "$SSH2"; do
  ss -tln 2>/dev/null | grep -Eq "[:.]$p " && ch_die "port $p is already in use on this host"
done

BASE="/var/tmp/ch-$NAME"
[ ! -e "$BASE" ] || ch_die "$BASE already exists; remove it first"

for img in "codehaven-server:$NAME" "codehaven-runner:$NAME" codehaven-job:trixie; do
  docker image inspect "$img" >/dev/null 2>&1 || ch_die "$img is missing locally. build them first: bash deploy/dev/release.sh $NAME"
done

CO="$BASE/checkout"
ENV_MAIN="$CO/.env"
ENV_REST="$CO/.env.restore"
NET="$NAME-proxy"
SCRATCH="$BASE/scratch"
mkdir -p "$SCRATCH"

# write_pools <file> <backend-cidr> <runners-cidr>: a compose file that fixes the two network subnets.
write_pools() {
  cat >"$1" <<EOF
networks:
  backend:
    ipam:
      config:
        - subnet: $2
  runners:
    ipam:
      config:
        - subnet: $3
EOF
}

# Removes everything this script made and nothing else, even when a check failed. The two compose
# projects go down before the network does (containers are still attached to it). The data dirs hold
# files owned by other uids; root removes them.
cleanup() {
  rc=$?
  set +e
  for f in "$ENV_MAIN" "$ENV_REST"; do
    [ -f "$f" ] && ch_run "$f" bash "$CO/deploy/compose.sh" --profile ci down -v --remove-orphans >/dev/null 2>&1
  done
  docker network rm "$NET" >/dev/null 2>&1
  rm -rf "$BASE"
  exit "$rc"
}
trap cleanup EXIT

printf 'clean-clone %s: checkout %s\n' "$NAME" "$SRC"

# --- 1. the clean clone -------------------------------------------------------------------------

mkdir -p "$CO"
if tar -C "$SRC" --exclude=./target --exclude=./dist --exclude=./.git --exclude=./.worktrees \
        --exclude=.env --exclude='*.env' -cf - . | tar -C "$CO" -xf -; then
  ok "clean copy of the checkout in $CO"
else
  bad "clean copy of the checkout"
fi
[ -f "$CO/.env.example" ] && ok ".env.example came along" || bad ".env.example is missing from the copy"
[ ! -e "$CO/.env" ] && ok "the copy carries no env file" || bad "the copy carries an env file"

# --- 2. the throwaway network and the .env written through the template --------------------------

docker network create --label org.codehaven.throwaway=true "$NET" >/dev/null || ch_die "cannot create the docker network $NET"
subnet="$(docker network inspect "$NET" --format '{{(index .IPAM.Config 0).Subnet}}')"
[ -n "$subnet" ] || ch_die "cannot read the subnet of $NET"

cp "$CO/.env.example" "$ENV_MAIN"
misses=0
for k in CODEHAVEN_PROJECT CODEHAVEN_TAG CODEHAVEN_EXTERNAL_URL CODEHAVEN_DATA_DIR \
         CODEHAVEN_SECRETS_DIR CODEHAVEN_BACKUP_DIR CODEHAVEN_BUILD_ROOT CODEHAVEN_HTTP_BIND \
         CODEHAVEN_SSH_BIND CODEHAVEN_SSH_EXTERNAL CODEHAVEN_PROXY CODEHAVEN_PROXY_NETWORK \
         CODEHAVEN_PROXY_ALIAS CODEHAVEN_TRUSTED_PROXIES CODEHAVEN_RUNNER_NAME; do
  grep -qE "^(# *)?$k=" "$CO/.env.example" && continue
  bad "$k is not named in .env.example"; misses=$((misses + 1))
done
[ "$misses" -eq 0 ] && ok "every key the scripts need is named in .env.example"

set_env "$ENV_MAIN" CODEHAVEN_PROJECT "$NAME"
set_env "$ENV_MAIN" CODEHAVEN_TAG "$NAME"
set_env "$ENV_MAIN" CODEHAVEN_EXTERNAL_URL "http://127.0.0.1:$HTTP1"
set_env "$ENV_MAIN" CODEHAVEN_DATA_DIR "$BASE/data"
set_env "$ENV_MAIN" CODEHAVEN_SECRETS_DIR "$BASE/secrets"
set_env "$ENV_MAIN" CODEHAVEN_BACKUP_DIR "$BASE/backups"
set_env "$ENV_MAIN" CODEHAVEN_BUILD_ROOT "$BASE"
set_env "$ENV_MAIN" CODEHAVEN_HTTP_BIND "127.0.0.1:$HTTP1"
set_env "$ENV_MAIN" CODEHAVEN_SSH_BIND "127.0.0.1:$SSH1"
set_env "$ENV_MAIN" CODEHAVEN_SSH_EXTERNAL "127.0.0.1:$SSH1"
set_env "$ENV_MAIN" CODEHAVEN_PROXY network
set_env "$ENV_MAIN" CODEHAVEN_PROXY_NETWORK "$NET"
set_env "$ENV_MAIN" CODEHAVEN_PROXY_ALIAS ch-cc-web
set_env "$ENV_MAIN" CODEHAVEN_TRUSTED_PROXIES "'[\"$subnet\"]'"
set_env "$ENV_MAIN" CODEHAVEN_RUNNER_NAME runner-1
grep -q "^CODEHAVEN_PROJECT=$NAME$" "$ENV_MAIN" && ok ".env written for project $NAME" || bad ".env was not written"

if [ -n "${CH_CLEAN_CLONE_SUBNETS:-}" ]; then
  # shellcheck disable=SC2086
  set -- $CH_CLEAN_CLONE_SUBNETS
  if [ "$#" -eq 4 ]; then
    write_pools "$BASE/pools-main.yml" "$1" "$2"
    write_pools "$BASE/pools-restore.yml" "$3" "$4"
    set_env "$ENV_MAIN" CODEHAVEN_COMPOSE_EXTRA "$BASE/pools-main.yml"
    ok "fixed subnets for the first instance's networks (CH_CLEAN_CLONE_SUBNETS)"
  else
    ch_die "CH_CLEAN_CLONE_SUBNETS needs four ranges (main backend, main runners, restore backend, restore runners)"
  fi
fi

# --- 3. setup: secrets once, never replaced, never printed ---------------------------------------

PW="$BASE/secrets/pg_password"; URL="$BASE/secrets/database_url"
run_out "$SCRATCH/setup1.out" "$ENV_MAIN" bash "$CO/deploy/prod/setup.sh"
want0 "$RC" "the first setup.sh exits 0"
[ -s "$PW" ] && [ -s "$URL" ] && ok "secrets dir holds pg_password and database_url" || bad "secrets are missing"
[ "$(stat -c %a "$PW" 2>/dev/null)" = 440 ] && [ "$(stat -c %a "$URL" 2>/dev/null)" = 440 ] \
  && ok "both secret files are mode 440" || bad "a secret file is not mode 440"
sha_first="$(sha256sum "$PW" | awk '{print $1}')"
run_out "$SCRATCH/setup2.out" "$ENV_MAIN" bash "$CO/deploy/prod/setup.sh"
want0 "$RC" "the second setup.sh exits 0"
[ "$(sha256sum "$PW" | awk '{print $1}')" = "$sha_first" ] && ok "pg_password is unchanged by the second run" || bad "pg_password was replaced"
pw="$(cat "$PW")"
if [ -n "$pw" ]; then
  for f in setup1.out setup2.out; do
    grep -qF "$pw" "$SCRATCH/$f" && bad "setup output ($f) contains the password" || ok "setup output ($f) keeps the password out"
  done
else
  bad "pg_password is empty, so the leak check could not run"
fi

# --- 4. up and reachability ----------------------------------------------------------------------

run_out "$SCRATCH/up1.out" "$ENV_MAIN" bash "$CO/deploy/prod/up.sh"
want0 "$RC" "up.sh exits 0 (the images exist, so it builds nothing)"
# up.sh output holds no secret, so a failure shows the end of it.
[ "$RC" -eq 0 ] || tail -n 25 "$SCRATCH/up1.out" | sed 's/^/      up.sh: /'

check "the server becomes healthy" wait_healthy "$ENV_MAIN"
check "public smoke against 127.0.0.1:$HTTP1" bash "$CO/deploy/dev/public-smoke.sh" "http://127.0.0.1:$HTTP1"
[ -s "$SCRATCH/last.out" ] && grep -q "^FAIL" "$SCRATCH/last.out" && grep "^FAIL" "$SCRATCH/last.out" | sed 's/^/      smoke: /'
aliasout="$(docker run --rm --network "$NET" --label org.codehaven.throwaway=true \
  postgres:17-alpine wget -qO- http://ch-cc-web:3200/healthz 2>/dev/null)"
[ "$aliasout" = ok ] && ok "a container on the proxy network reaches the server by alias" || bad "the alias is not reachable on the proxy network"
for p in "$HTTP1" "$SSH1"; do
  ss -tln 2>/dev/null | grep -q "127\.0\.0\.1:$p " && ok "port $p listens on 127.0.0.1" || bad "port $p is not on 127.0.0.1"
  ss -tln 2>/dev/null | grep -Eq "(0\.0\.0\.0|\*|\[::\]):$p " && bad "port $p listens beyond loopback" || ok "port $p listens nowhere beyond loopback"
done

# --- 5. the first admin --------------------------------------------------------------------------

run_out "$SCRATCH/admin1.out" "$ENV_MAIN" bash "$CO/deploy/prod/first-admin.sh" alice alice@example.com
want0 "$RC" "first-admin.sh exits 0 (its one-time link stays in the scratch dir)"
[ "$(users "$ENV_MAIN")" = 1 ] && ok "admin users lists exactly 1 account" || bad "admin users does not list exactly 1 account"
run_out "$SCRATCH/admin2.out" "$ENV_MAIN" bash "$CO/deploy/prod/first-admin.sh" alice alice@example.com
wantfail "$RC" "a second first-admin.sh run is refused"
[ "$(users "$ENV_MAIN")" = 1 ] && ok "still exactly 1 account" || bad "the account count moved"

# --- 6. the runner --------------------------------------------------------------------------------

tok="$(ch_run "$ENV_MAIN" bash "$CO/deploy/compose.sh" exec -T server codehaven admin runner-token 2>/dev/null | tail -1)"
[ -n "$tok" ] && ok "a runner token was fetched (kept in a variable, never printed)" || bad "no runner token"
(
  export HAVEN_RUNNER_TOKEN="$tok"
  run_out "$SCRATCH/register.out" "$ENV_MAIN" bash "$CO/deploy/prod/ci-up.sh" register
  exit "$RC"
)
want0 "$?" "ci-up.sh register exits 0 (name from CODEHAVEN_RUNNER_NAME)"
run_out "$SCRATCH/ciup.out" "$ENV_MAIN" bash "$CO/deploy/prod/ci-up.sh"
want0 "$RC" "ci-up.sh exits 0"
sleep 15
svcs="$(ch_run "$ENV_MAIN" bash "$CO/deploy/compose.sh" --profile ci ps --status running --format '{{.Service}}' 2>/dev/null)"
printf '%s\n' "$svcs" | grep -qx runner && ok "the runner service is running" || bad "the runner service is not running"
printf '%s\n' "$svcs" | grep -qx dind && ok "the dind service is running" || bad "the dind service is not running"
check "dind answers docker info" ch_run "$ENV_MAIN" bash "$CO/deploy/compose.sh" --profile ci exec -T dind docker info

# --- 7. backup -------------------------------------------------------------------------------------

# stdout only: the path is the one thing the script prints there. Compose writes its progress to stderr.
ch_run "$ENV_MAIN" bash "$CO/deploy/prod/backup.sh" >"$SCRATCH/backup.out" 2>"$SCRATCH/backup.err"; RC=$?
want0 "$RC" "backup.sh exits 0"
archive="$(tail -n 1 "$SCRATCH/backup.out")"
[ "$(grep -c . "$SCRATCH/backup.out")" = 1 ] && ok "backup.sh prints one path" || bad "backup.sh did not print exactly one path"
[ -f "$archive" ] && ok "the archive exists" || bad "the archive does not exist"
[ "$(stat -c %a "$archive" 2>/dev/null)" = 600 ] && ok "the archive is mode 600" || bad "the archive is not mode 600"
case "$archive" in
  "$BASE/backups/"*) ok "the archive is under \$BASE/backups" ;;
  *) bad "the archive is not under \$BASE/backups ($archive)" ;;
esac
listing="$(tar -tzf "$archive" 2>/dev/null)"
for f in MANIFEST pg.dump data.tar compose-secrets.tar codehaven.env; do
  printf '%s\n' "$listing" | grep -qx "$f" && ok "the archive lists $f" || bad "the archive has no $f"
done
check "the server is healthy again after the backup" wait_healthy "$ENV_MAIN"

cp "$ENV_MAIN" "$SCRATCH/env-incheck.env"
set_env "$SCRATCH/env-incheck.env" CODEHAVEN_BACKUP_DIR "$CO/backup-inside"
run_out "$SCRATCH/backup-incheck.out" "$SCRATCH/env-incheck.env" bash "$CO/deploy/prod/backup.sh"
wantfail "$RC" "backup.sh refuses a backup dir inside the checkout"

# --- 8. restore into a second, empty project --------------------------------------------------------

cp "$ENV_MAIN" "$ENV_REST"
set_env "$ENV_REST" CODEHAVEN_PROJECT "${NAME}-r"
set_env "$ENV_REST" CODEHAVEN_DATA_DIR "$BASE/r/data"
set_env "$ENV_REST" CODEHAVEN_SECRETS_DIR "$BASE/r/secrets"
set_env "$ENV_REST" CODEHAVEN_BACKUP_DIR "$BASE/r/backups"
set_env "$ENV_REST" CODEHAVEN_HTTP_BIND "127.0.0.1:$HTTP2"
set_env "$ENV_REST" CODEHAVEN_SSH_BIND "127.0.0.1:$SSH2"
set_env "$ENV_REST" CODEHAVEN_SSH_EXTERNAL "127.0.0.1:$SSH2"
set_env "$ENV_REST" CODEHAVEN_EXTERNAL_URL "http://127.0.0.1:$HTTP2"
set_env "$ENV_REST" CODEHAVEN_PROXY none
set_env "$ENV_REST" CODEHAVEN_TRUSTED_PROXIES "'[]'"
[ -f "$BASE/pools-restore.yml" ] && set_env "$ENV_REST" CODEHAVEN_COMPOSE_EXTRA "$BASE/pools-restore.yml"

run_out "$SCRATCH/restore1.out" "$ENV_REST" bash "$CO/deploy/prod/restore.sh" "$archive"
want0 "$RC" "restore.sh exits 0 into the empty project ${NAME}-r"
run_out "$SCRATCH/up-r.out" "$ENV_REST" bash "$CO/deploy/prod/up.sh"
want0 "$RC" "up.sh exits 0 on the restored project"
check "the restored server is healthy" wait_healthy "$ENV_REST"
[ "$(users "$ENV_REST")" = 1 ] && ok "the restored instance lists 1 account" || bad "the restored instance does not list 1 account"
run_out "$SCRATCH/restore2.out" "$ENV_REST" bash "$CO/deploy/prod/restore.sh" "$archive"
wantfail "$RC" "a second restore into the same project is refused"
run_out "$SCRATCH/restore3.out" "$ENV_MAIN" bash "$CO/deploy/prod/restore.sh" "$archive"
wantfail "$RC" "a restore into the first project is refused"
[ "$(users "$ENV_MAIN")" = 1 ] && ok "the first instance still lists exactly 1 account" || bad "the first instance's account count moved"
[ "$(sha256sum "$BASE/r/secrets/pg_password" 2>/dev/null | awk '{print $1}')" = "$sha_first" ] \
  && ok "the restored credentials are the ones the backup carried" || bad "the restored pg_password differs from the first one's"

# --- 9. upgrade and rollback -------------------------------------------------------------------------

run_out "$SCRATCH/up2.out" "$ENV_MAIN" bash "$CO/deploy/prod/up.sh"
want0 "$RC" "up.sh with nothing to do exits 0"
check "the server stays healthy" wait_healthy "$ENV_MAIN"
[ "$(users "$ENV_MAIN")" = 1 ] && ok "still exactly 1 account after the upgrade pass" || bad "the account count moved after the upgrade pass"

run_out "$SCRATCH/up-nomigrate.out" "$ENV_MAIN" env CODEHAVEN_NO_MIGRATE=1 bash "$CO/deploy/prod/up.sh"
want0 "$RC" "CODEHAVEN_NO_MIGRATE=1 up.sh exits 0"
check "the server is healthy without --migrate" wait_healthy "$ENV_MAIN"
cid="$(ch_run "$ENV_MAIN" bash "$CO/deploy/compose.sh" ps -q server 2>/dev/null)"
if [ -n "$cid" ]; then
  docker inspect --format '{{json .Config.Cmd}}' "$cid" 2>/dev/null | grep -q '"--migrate"' \
    && bad "the serve command still carries --migrate" || ok "the serve command carries no --migrate"
else
  bad "no server container to inspect for --migrate"
fi

run_out "$SCRATCH/up3.out" "$ENV_MAIN" bash "$CO/deploy/prod/up.sh"
want0 "$RC" "up.sh puts --migrate back"
cid="$(ch_run "$ENV_MAIN" bash "$CO/deploy/compose.sh" ps -q server 2>/dev/null)"
if [ -n "$cid" ]; then
  docker inspect --format '{{json .Config.Cmd}}' "$cid" 2>/dev/null | grep -q '"--migrate"' \
    && ok "the serve command carries --migrate again" || bad "the serve command lost --migrate"
else
  bad "no server container to inspect for --migrate"
fi

# --- 10. config errors name the missing key ------------------------------------------------------------

cp "$ENV_MAIN" "$SCRATCH/env-nourl.env"
sed -i '/^CODEHAVEN_EXTERNAL_URL=/d' "$SCRATCH/env-nourl.env"
ch_run "$SCRATCH/env-nourl.env" bash "$CO/deploy/compose.sh" config >"$SCRATCH/cfg1.out" 2>&1
if [ "$?" -ne 0 ] && grep -q CODEHAVEN_EXTERNAL_URL "$SCRATCH/cfg1.out"; then
  ok "a missing CODEHAVEN_EXTERNAL_URL is named by compose.sh config"
else
  bad "a missing CODEHAVEN_EXTERNAL_URL was not named"
fi

cp "$ENV_MAIN" "$SCRATCH/env-nodata.env"
sed -i '/^CODEHAVEN_DATA_DIR=/d' "$SCRATCH/env-nodata.env"
ch_run "$SCRATCH/env-nodata.env" bash "$CO/deploy/compose.sh" config >"$SCRATCH/cfg2.out" 2>&1
if [ "$?" -ne 0 ] && grep -q CODEHAVEN_DATA_DIR "$SCRATCH/cfg2.out"; then
  ok "a missing CODEHAVEN_DATA_DIR is named by compose.sh config"
else
  bad "a missing CODEHAVEN_DATA_DIR was not named"
fi

# --- the count -----------------------------------------------------------------------------------------

printf 'clean-clone: %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
