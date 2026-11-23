#!/usr/bin/env bash
# deploy/tests/clean-clone.sh NAME: the stranger test. From a clean copy of the checkout to a first
# admin, a runner, a backup (checked against its SHA256SUMS), a restore into a second empty project
# (every table's row count and git fsck of every repository compared), an upgrade, a rollback, and
# a failed deploy that rolls the server back, on one Linux host with Docker, using images that
# already exist. Run as root.
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

# table_counts <envfile>: "table count" for every table in the public schema, sorted by name.
table_counts() {
  ch_run "$1" bash "$CO/deploy/compose.sh" exec -T postgres psql -U codehaven -d codehaven -At -c \
    "SELECT format('SELECT %L || '' '' || count(*) FROM public.%I;', table_name, table_name) FROM information_schema.tables WHERE table_schema = 'public' AND table_type = 'BASE TABLE' ORDER BY table_name" \
    | ch_run "$1" bash "$CO/deploy/compose.sh" exec -T postgres psql -U codehaven -d codehaven -At
}
# repos_in <data-dir>: every bare repository under <data-dir>/git, one path per line, sorted.
repos_in() { find "$1/git" -type d -name objects -prune 2>/dev/null | sed 's,/objects$,,' | while IFS= read -r d; do [ -f "$d/HEAD" ] && printf '%s\n' "$d"; done | sort; }
# refs_of <repo>: its refs and commit ids.
refs_of() { git -c safe.directory='*' --git-dir="$1" for-each-ref --format='%(refname) %(objectname)' 2>/dev/null; }

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
  # The image tags this script's own checks leave behind. Removing a tag never removes an image
  # that another tag still names.
  docker rmi "codehaven-server:$NAME-broken" "codehaven-runner:$NAME-broken" "codehaven-server:$NAME-previous" "codehaven-server:$NAME-r-previous" >/dev/null 2>&1
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

# --- 5b. data to back up ----------------------------------------------------------------------------

bobpw="$(head -c 18 /dev/urandom | base64 | tr -d '/+=')"
printf '%s\n' "$bobpw" | ch_run "$ENV_MAIN" bash "$CO/deploy/compose.sh" exec -T server codehaven admin create-user --username bob --email bob@example.com --password-stdin >"$SCRATCH/bob.out" 2>&1
want0 "$?" "a second account, bob, is created with a password"
auth="Authorization: Basic $(printf 'bob:%s' "$bobpw" | base64 | tr -d '\n')"
code="$(curl -s -o "$SCRATCH/repo.out" -w '%{http_code}' -H "$auth" -H 'content-type: application/json' -d '{"name":"demo","private":true}' "http://127.0.0.1:$HTTP1/api/v3/user/repos")"
[ "$code" = 201 ] && ok "bob creates the repository bob/demo over the API" || bad "creating bob/demo answered $code"
work="$SCRATCH/demo"; mkdir -p "$work"
( cd "$work" && git init -q -b main && printf 'hello\n' > README.md && git add README.md \
  && git -c user.name=Bob -c user.email=bob@example.com commit -q -m first \
  && git -c credential.helper= -c http.extraHeader="$auth" push -q "http://127.0.0.1:$HTTP1/bob/demo.git" main ) >"$SCRATCH/push.out" 2>&1
want0 "$?" "a commit is pushed to bob/demo over HTTP"
[ "$(users "$ENV_MAIN")" = 2 ] && ok "admin users lists 2 accounts" || bad "admin users does not list 2 accounts"
ACCOUNTS=2

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

# The server is stopped first, so the table counts are taken with nothing writing. backup.sh
# leaves a server it found stopped, stopped, so this script starts it again itself.
ch_run "$ENV_MAIN" bash "$CO/deploy/compose.sh" stop server >/dev/null 2>&1
table_counts "$ENV_MAIN" >"$SCRATCH/counts-main.txt" 2>"$SCRATCH/counts-main.err"
[ "$(grep -c . "$SCRATCH/counts-main.txt")" -gt 10 ] && ok "the main database lists $(grep -c . "$SCRATCH/counts-main.txt") tables" || bad "could not count the main database's tables"
# stdout only: the path is the one thing the script prints there. Compose writes its progress to stderr.
ch_run "$ENV_MAIN" bash "$CO/deploy/prod/backup.sh" >"$SCRATCH/backup.out" 2>"$SCRATCH/backup.err"; RC=$?
want0 "$RC" "backup.sh exits 0"
table_counts "$ENV_MAIN" >"$SCRATCH/counts-main2.txt" 2>/dev/null
cmp -s "$SCRATCH/counts-main.txt" "$SCRATCH/counts-main2.txt" && ok "nothing changed in the database while the backup ran" || bad "the database changed while the backup ran"
ch_run "$ENV_MAIN" bash "$CO/deploy/compose.sh" start server >/dev/null 2>&1
archive="$(tail -n 1 "$SCRATCH/backup.out")"
[ "$(grep -c . "$SCRATCH/backup.out")" = 1 ] && ok "backup.sh prints one path" || bad "backup.sh did not print exactly one path"
[ -f "$archive" ] && ok "the archive exists" || bad "the archive does not exist"
[ "$(stat -c %a "$archive" 2>/dev/null)" = 600 ] && ok "the archive is mode 600" || bad "the archive is not mode 600"
case "$archive" in
  "$BASE/backups/"*) ok "the archive is under \$BASE/backups" ;;
  *) bad "the archive is not under \$BASE/backups ($archive)" ;;
esac
listing="$(tar -tzf "$archive" 2>/dev/null)"
for f in MANIFEST pg.dump data.tar compose-secrets.tar codehaven.env SHA256SUMS; do
  printf '%s\n' "$listing" | grep -qx "$f" && ok "the archive lists $f" || bad "the archive has no $f"
done
rm -rf "$SCRATCH/unpacked"; mkdir -p "$SCRATCH/unpacked"
tar -xzf "$archive" -C "$SCRATCH/unpacked" && (cd "$SCRATCH/unpacked" && sha256sum --quiet -c SHA256SUMS) \
  && ok "every member of the archive matches SHA256SUMS" || bad "the archive does not match SHA256SUMS"
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

# A tampered archive must be refused before anything is written.
tdir="$SCRATCH/unpacked"   # already unpacked in section 7
printf '#tampered\n' >> "$tdir/codehaven.env"
tar -C "$tdir" -czf "$SCRATCH/tampered.tar.gz" MANIFEST pg.dump data.tar compose-secrets.tar codehaven.env SHA256SUMS
run_out "$SCRATCH/restore-tampered.out" "$ENV_REST" bash "$CO/deploy/prod/restore.sh" "$SCRATCH/tampered.tar.gz"
wantfail "$RC" "a tampered archive is refused"
grep -q checksums "$SCRATCH/restore-tampered.out" && ok "the refusal names the checksums" || bad "the refusal does not mention the checksums"
[ ! -e "$BASE/r/data/postgres" ] && ok "the refused restore wrote nothing" || bad "the refused restore wrote $BASE/r/data/postgres"

run_out "$SCRATCH/restore1.out" "$ENV_REST" bash "$CO/deploy/prod/restore.sh" "$archive"
want0 "$RC" "restore.sh exits 0 into the empty project ${NAME}-r"
# restore.sh stopped postgres when it finished, so it comes up again to compare the restored
# database and repositories with what was backed up. The up.sh run below starts the rest.
ch_run "$ENV_REST" bash "$CO/deploy/compose.sh" up -d postgres >/dev/null 2>&1
for i in $(seq 1 40); do ch_run "$ENV_REST" bash "$CO/deploy/compose.sh" exec -T postgres pg_isready -U codehaven -d codehaven >/dev/null 2>&1 && break; sleep 2; done
table_counts "$ENV_REST" >"$SCRATCH/counts-restored.txt" 2>"$SCRATCH/counts-restored.err"
if cmp -s "$SCRATCH/counts-main.txt" "$SCRATCH/counts-restored.txt"; then
  ok "every table has the same row count after the restore ($(grep -c . "$SCRATCH/counts-restored.txt") tables)"
else
  bad "row counts differ after the restore"; diff "$SCRATCH/counts-main.txt" "$SCRATCH/counts-restored.txt" | head -n 20 | sed 's/^/      counts: /'
fi
repos_in "$BASE/data" >"$SCRATCH/repos-main.txt"; repos_in "$BASE/r/data" >"$SCRATCH/repos-restored.txt"
n_repos="$(grep -c . "$SCRATCH/repos-restored.txt")"
[ "$n_repos" -ge 1 ] && [ "$n_repos" = "$(grep -c . "$SCRATCH/repos-main.txt")" ] && ok "the restore has the same $n_repos repositories" || bad "the restored repository count ($n_repos) is wrong"
fsck_bad=0; refs_bad=0
while IFS= read -r r; do
  git -c safe.directory='*' --git-dir="$r" fsck --full --strict >"$SCRATCH/fsck.out" 2>&1 || fsck_bad=$((fsck_bad + 1))
  rel="${r#"$BASE/r/data/"}"
  [ "$(refs_of "$r")" = "$(refs_of "$BASE/data/$rel")" ] || refs_bad=$((refs_bad + 1))
done <"$SCRATCH/repos-restored.txt"
[ "$fsck_bad" = 0 ] && ok "git fsck --full --strict passes on every restored repository" || bad "git fsck failed on $fsck_bad restored repositories"
[ "$refs_bad" = 0 ] && ok "every restored repository has the same refs as the original" || bad "$refs_bad restored repositories have different refs"
run_out "$SCRATCH/up-r.out" "$ENV_REST" bash "$CO/deploy/prod/up.sh"
want0 "$RC" "up.sh exits 0 on the restored project"
[ "$RC" -eq 0 ] || tail -n 25 "$SCRATCH/up-r.out" | sed 's/^/      up.sh: /'
[ "$RC" -eq 0 ] || cat "$BASE/run/up-smoke.log" 2>/dev/null | grep '^FAIL' | sed 's/^/      smoke: /'
check "the restored server is healthy" wait_healthy "$ENV_REST"
[ "$(users "$ENV_REST")" = "$ACCOUNTS" ] && ok "the restored instance lists $ACCOUNTS accounts" || bad "the restored instance does not list $ACCOUNTS accounts"
run_out "$SCRATCH/restore2.out" "$ENV_REST" bash "$CO/deploy/prod/restore.sh" "$archive"
wantfail "$RC" "a second restore into the same project is refused"
run_out "$SCRATCH/restore3.out" "$ENV_MAIN" bash "$CO/deploy/prod/restore.sh" "$archive"
wantfail "$RC" "a restore into the first project is refused"
[ "$(users "$ENV_MAIN")" = "$ACCOUNTS" ] && ok "the first instance still lists exactly $ACCOUNTS accounts" || bad "the first instance's account count moved"
[ "$(sha256sum "$BASE/r/secrets/pg_password" 2>/dev/null | awk '{print $1}')" = "$sha_first" ] \
  && ok "the restored credentials are the ones the backup carried" || bad "the restored pg_password differs from the first one's"

# --- 9. upgrade and rollback -------------------------------------------------------------------------

run_out "$SCRATCH/up2.out" "$ENV_MAIN" bash "$CO/deploy/prod/up.sh"
want0 "$RC" "up.sh with nothing to do exits 0"
check "the server stays healthy" wait_healthy "$ENV_MAIN"
[ "$(users "$ENV_MAIN")" = "$ACCOUNTS" ] && ok "still exactly $ACCOUNTS accounts after the upgrade pass" || bad "the account count moved after the upgrade pass"

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

# --- 9b. a failed deploy rolls the server back ----------------------------------------------------

docker tag "codehaven-runner:$NAME" "codehaven-runner:$NAME-broken"
printf 'FROM codehaven-server:%s\nENTRYPOINT ["/nonexistent"]\n' "$NAME" | docker build -q -t "codehaven-server:$NAME-broken" - >"$SCRATCH/broken-build.out" 2>&1
want0 "$?" "a broken server image is built for the test"
good_id="$(docker image inspect --format '{{.Id}}' "codehaven-server:$NAME")"
newest_before="$(ls -1t "$BASE/backups"/pre-deploy-"$NAME"-*.dump 2>/dev/null | head -n 1)"
# A short wait: the broken server never becomes healthy, so there is nothing to wait for.
run_out "$SCRATCH/up-broken.out" "$ENV_MAIN" env CODEHAVEN_DEPLOY_WAIT=60 bash "$CO/deploy/prod/up.sh" "$NAME-broken"
wantfail "$RC" "up.sh with a server that never becomes healthy exits non-zero"
grep -q "rolled back" "$SCRATCH/up-broken.out" && ok "up.sh says it rolled back" || { bad "up.sh does not say it rolled back"; tail -n 15 "$SCRATCH/up-broken.out" | sed 's/^/      up.sh: /'; }
check "the rolled back server is healthy" wait_healthy "$ENV_MAIN"
cid="$(ch_run "$ENV_MAIN" bash "$CO/deploy/compose.sh" ps -q server 2>/dev/null)"
[ -n "$cid" ] && [ "$(docker inspect --format '{{.Image}}' "$cid")" = "$good_id" ] && ok "the server runs the previous image again" || bad "the server does not run the previous image"
docker image inspect "codehaven-server:$NAME-previous" >/dev/null 2>&1 && ok "the previous image is kept as codehaven-server:$NAME-previous" || bad "codehaven-server:$NAME-previous is missing"
newest="$(ls -1t "$BASE/backups"/pre-deploy-"$NAME"-*.dump 2>/dev/null | head -n 1)"
case "$newest" in
  *-"$NAME-broken".dump) [ "$newest" != "$newest_before" ] && ok "a pre-deploy dump was taken before the broken deploy" || bad "no new pre-deploy dump" ;;
  *) bad "no pre-deploy dump was taken before the broken deploy" ;;
esac
[ -n "$newest" ] && [ "$(stat -c %a "$newest")" = 600 ] && ok "the pre-deploy dump is mode 600" || bad "the pre-deploy dump is not mode 600"
[ "$(users "$ENV_MAIN")" = "$ACCOUNTS" ] && ok "still exactly $ACCOUNTS accounts after the rollback" || bad "the account count moved after the rollback"
run_out "$SCRATCH/up-after.out" "$ENV_MAIN" bash "$CO/deploy/prod/up.sh"
want0 "$RC" "a normal up.sh after the rollback exits 0"
check "the server is healthy on its own tag again" wait_healthy "$ENV_MAIN"

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
