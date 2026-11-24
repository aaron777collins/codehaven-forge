#!/usr/bin/env bash
# Proves the git hooks guard against a real noexec mount, on a throwaway stack. Branch protection
# is enforced by git hooks, and git silently skips a hook it cannot execute, so the server must
# not run (or accept pushes) on a data directory that cannot execute files. Runs on the build
# host, as root, from a synced checkout:
#   bash deploy/dev/host.sh <stream> 'bash deploy/tests/noexec.sh'
# The images are not built here:
#   bash deploy/dev/host.sh <stream> 'CH_STREAM=<stream> bash deploy/dev/release.sh proof'
#
# 1. The data volume is a tmpfs mounted `noexec`. The server must exit non-zero at start with
#    its "refusing to start" message, and must not come up.
# 2. The same image on an exec tmpfs starts, and a push works (the control).
# 3. The running server's /data is then remounted `noexec` from the host (nsenter into the
#    container's mount namespace), so the probe fails after start. A push must answer 503 with
#    the "Pushes are disabled" message, and a read (clone) must still work.
# Everything it makes is named chproof-nx*, and the trap removes all of it. It never touches the
# live `codehaven` project, port 3200 on the host or the live data directory. Nothing prints the
# password or the token.
#
#   CHPROOF_KEEP=1   leave the stack up (remove it with `docker compose -p chproof-nx down -v`)
#   CODEHAVEN_TAG    server image tag (default proof)
#   CODEHAVEN_PROOF_SUBNET  a free /24 for the throwaway network (required)
set -euo pipefail

tag="${CODEHAVEN_TAG:-proof}"
: "${CODEHAVEN_PROOF_SUBNET:?set CODEHAVEN_PROOF_SUBNET to a free /24 on this host, for example 10.20.30.0/24}"
export CODEHAVEN_PROOF_SUBNET
proj=chproof-nx
user=chproof
work="$(mktemp -d)"

docker image inspect "codehaven-server:$tag" >/dev/null 2>&1 || {
  echo "noexec: codehaven-server:$tag is missing. Build it first (see the header)." >&2
  exit 1
}
command -v nsenter >/dev/null || { echo "noexec: nsenter is needed to remount inside the container" >&2; exit 1; }

PORT=$(python3 - <<'PY'
import socket
s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()
PY
)
[ "$PORT" != 3200 ] || { echo "noexec: picked a reserved port" >&2; exit 1; }
export PORT CODEHAVEN_TAG="$tag"
base="http://127.0.0.1:$PORT"

cat > "$work/compose.yml" <<'YML'
name: chproof-nx
x-server: &server
  image: codehaven-server:${CODEHAVEN_TAG}
  depends_on:
    postgres: {condition: service_healthy}
  command: ["codehaven", "serve", "--migrate"]
  restart: "no"
  environment:
    CODEHAVEN__SERVER__EXTERNAL_URL: http://127.0.0.1:${PORT}
    CODEHAVEN__DATABASE__URL: postgres://codehaven:proof-only-not-a-secret@postgres:5432/codehaven
    CODEHAVEN__STORAGE__DATA_DIR: /data
    CODEHAVEN__LOG__FORMAT: pretty
  mem_limit: 2g
services:
  postgres:
    image: postgres:17-alpine
    environment:
      POSTGRES_USER: codehaven
      POSTGRES_PASSWORD: proof-only-not-a-secret
      POSTGRES_DB: codehaven
    command: ["postgres", "-c", "fsync=off", "-c", "synchronous_commit=off", "-c", "full_page_writes=off"]
    tmpfs: [/var/lib/postgresql/data]
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U codehaven -d codehaven"]
      interval: 2s
      timeout: 3s
      retries: 30
    mem_limit: 1g
  # The data directory is a tmpfs mounted noexec. The server must refuse to start on it.
  server-noexec:
    <<: *server
    tmpfs: ["/data:noexec,nosuid,uid=10001,gid=10001,mode=0755"]
  # The same image on a tmpfs that can run programs.
  server:
    <<: *server
    ports: ["127.0.0.1:${PORT}:3200"]
    tmpfs: ["/data:exec,uid=10001,gid=10001,mode=0755"]
    healthcheck:
      test: ["CMD", "codehaven", "healthcheck"]
      interval: 2s
      timeout: 5s
      retries: 30
      start_period: 5s
# The network's subnet is fixed, because a busy host's default address pools can run out. It
# comes from CODEHAVEN_PROOF_SUBNET: a free /24 on this host.
networks:
  default:
    ipam:
      config:
        - subnet: ${CODEHAVEN_PROOF_SUBNET}
YML
compose=(docker compose -p "$proj" -f "$work/compose.yml")

cleanup() {
  set +e
  if [ -n "${CHPROOF_KEEP:-}" ]; then
    echo "noexec: CHPROOF_KEEP is set, leaving $proj up on 127.0.0.1:$PORT, work dir $work" >&2
    return
  fi
  "${compose[@]}" down -v --remove-orphans >/dev/null 2>&1
  rm -rf "$work"
}
trap cleanup EXIT

step() { echo "noexec: $*"; }
die() { echo "noexec: FAIL: $*" >&2; exit 1; }

"${compose[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
up() { "${compose[@]}" up -d "$@" >"$work/up.log" 2>&1 || { tail -15 "$work/up.log" >&2; die "docker compose up $* failed"; }; }
up postgres
for _ in $(seq 1 30); do
  [ "$("${compose[@]}" ps --format '{{.Service}} {{.Health}}' | grep '^postgres ' | cut -d' ' -f2)" = healthy ] && break
  sleep 2
done

# ---- 1. a server whose data directory is noexec refuses to start -------------------------------
up server-noexec
cid=$("${compose[@]}" ps -a -q server-noexec)
[ -n "$cid" ] || die "the noexec server container was not created"
code=$(timeout 90 docker wait "$cid") || die "the server on a noexec data directory is still running after 90 seconds"
logs=$(docker logs "$cid" 2>&1)
[ "$code" != 0 ] || die "the server on a noexec data directory exited 0"
grep -q "refusing to start" <<< "$logs" || die "no 'refusing to start' message on a noexec data directory:
$(tail -5 <<< "$logs")"
grep -q "noexec" <<< "$logs" || die "the refusal does not mention noexec"
mounts=$(docker inspect "$cid" --format '{{json .HostConfig.Tmpfs}}')
grep -q noexec <<< "$mounts" || die "the data directory was not mounted noexec"
step "a noexec data directory: the server exits $code and says it is refusing to start: ok"
"${compose[@]}" rm -f -s server-noexec >/dev/null 2>&1

# ---- 2. the control: an exec data directory starts and takes a push ---------------------------
up server
st=""
for _ in $(seq 1 40); do
  st=$("${compose[@]}" ps --format '{{.Service}} {{.Health}}' | grep '^server ' | cut -d' ' -f2 || true)
  [ "$st" = healthy ] && break
  sleep 3
done
[ "$st" = healthy ] || { "${compose[@]}" logs --no-color server 2>&1 | tail -15; die "the server on an exec data directory did not become healthy"; }

python3 -c 'import secrets; print(secrets.token_urlsafe(24))' > "$work/password"
"${compose[@]}" exec -T server codehaven admin create-user --username "$user" --email chproof@example.test --can-run-ci --password-stdin \
  < "$work/password" >/dev/null 2>&1 || die "could not create the account"
cat > "$work/mktoken.py" <<'PY'
import http.client, re, sys, urllib.parse
host, port, user = sys.argv[1], int(sys.argv[2]), sys.argv[3]
password = open(sys.argv[4]).read().strip()
origin = f"http://{host}:{port}"
jar = {}
def req(method, path, form=None, js=None, auth=None):
    h = {"Host": f"{host}:{port}", "User-Agent": "chproof/1"}
    body = None
    if method != "GET":
        h["Origin"] = origin
    if jar:
        h["Cookie"] = "; ".join(f"{k}={v}" for k, v in jar.items())
    if form is not None:
        body = urllib.parse.urlencode(form, doseq=True).encode()
        h["Content-Type"] = "application/x-www-form-urlencoded"
    c = http.client.HTTPConnection(host, port, timeout=60)
    c.request(method, path, body=body, headers=h)
    r = c.getresponse(); data = r.read().decode("utf-8", "replace")
    for k, v in r.getheaders():
        if k.lower() == "set-cookie":
            n, _, val = v.split(";", 1)[0].partition("=")
            if val == "" or "Max-Age=0" in v: jar.pop(n, None)
            else: jar[n] = val
    c.close()
    return r.status, data
def csrf(path):
    st, text = req("GET", path)
    m = re.search(r'name="_csrf"\s+value="([^"]+)"', text) or re.search(r'value="([^"]+)"\s+name="_csrf"', text)
    if not m: sys.exit(f"no csrf token on {path} ({st})")
    return m.group(1)
st, _ = req("POST", "/login", {"_csrf": csrf("/login"), "username": user, "password": password, "next": "/"})
if st != 303: sys.exit(f"sign-in answered {st}")
st, text = req("POST", "/settings/tokens", {"_csrf": csrf("/settings/tokens"), "name": "chproof", "scopes": ["repo", "user"], "expires_in_days": "1"})
m = re.search(r"chp_[A-Za-z0-9]{40}", text)
if st != 200 or not m: sys.exit(f"token create answered {st}")
open(sys.argv[5], "w").write(m.group(0))
PY
python3 "$work/mktoken.py" 127.0.0.1 "$PORT" "$user" "$work/password" "$work/token" || die "could not sign in and make a token"
chmod 600 "$work/token" "$work/password"
tok=$(cat "$work/token")
code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $tok" -H 'Content-Type: application/json' \
  -d '{"name":"demo","private":true}' "$base/api/v3/user/repos")
[ "$code" = 201 ] || die "creating the repository answered $code"

# git reads the credential from this helper, so the token is never on a command line or in a URL.
export HOME="$work/home" GIT_TERMINAL_PROMPT=0 GIT_CONFIG_NOSYSTEM=1
mkdir -p "$HOME"
cd "$work"   # not the synced checkout: a worktree has a .git file that points nowhere here
git config --global credential.helper "!f() { echo username=$user; echo password=\$(cat $work/token); }; f"
git config --global user.name chproof
git config --global user.email chproof@example.test
git config --global init.defaultBranch main
remote="$base/$user/demo.git"
mkdir -p "$work/wd" && cd "$work/wd"
git init -q . && echo one > a.txt && git add a.txt && git commit -q -m one
git push -q "$remote" main 2>"$work/push1.err" || die "the control push failed: $(tail -3 "$work/push1.err")"
step "an exec data directory: the server is healthy and a push works: ok"

# ---- 3. remount noexec under the running server: pushes are refused, reads are not ------------
pid=$(docker inspect "$("${compose[@]}" ps -q server)" --format '{{.State.Pid}}')
[ "$pid" -gt 1 ] || die "no pid for the server"
nsenter -t "$pid" -m mount -o remount,noexec /data || die "could not remount /data noexec"
grep ' /data ' "/proc/$pid/mountinfo" | grep -qw noexec || die "/data is not noexec after the remount"
step "the running server's /data is now a real noexec mount"
echo two > a.txt && git commit -q -am two
if git push "$remote" main >"$work/push2.out" 2>&1; then die "a push to a server that cannot run its hooks succeeded"; fi
grep -q "503" "$work/push2.out" || die "the refused push did not answer 503: $(tail -3 "$work/push2.out")"
grep -q "Pushes are disabled" "$work/push2.out" || die "the refused push does not say why: $(tail -3 "$work/push2.out")"
code=$(curl -sS -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $tok" "$base/$user/demo.git/info/refs?service=git-receive-pack")
[ "$code" = 503 ] || die "the receive-pack advertisement answered $code, not 503"
git clone -q "$remote" "$work/clone" 2>"$work/clone.err" || die "a clone failed while pushes were disabled: $(tail -3 "$work/clone.err")"
[ "$(cat "$work/clone/a.txt")" = one ] || die "the clone does not hold the earlier push"
step "a push answers 503 with its message, and a clone still works: ok"
"${compose[@]}" logs --no-color server 2>&1 | grep -q "refusing a push" || die "the server did not log the refused push"
step "all checks passed"
