#!/usr/bin/env bash
# Proves the OCI registry against real clients, on a throwaway stack. Runs on the build host, as
# root, from a synced checkout:
#   bash deploy/dev/host.sh <stream> 'bash deploy/tests/registry-conformance.sh'
# The images are not built here:
#   bash deploy/dev/host.sh <stream> 'CH_STREAM=<stream> bash deploy/dev/release.sh proof'
#
# What it does, in order:
#   1. starts its own project (chproof-reg: a postgres, a server, tmpfs data, a free loopback
#      port) with a made-up account;
#   2. `docker login`, `docker push`, `docker pull` with that account's personal access token,
#      and checks the pulled image is the pushed one;
#   3. checks a range request on a pushed layer and the catalog over HTTP;
#   4. runs the official OCI distribution-spec conformance suite (a pinned release, by digest)
#      for push, pull, content discovery and content management.
# Everything it makes is named chproof-reg*, and the trap removes all of it, whatever happened.
# It never touches the live `codehaven` project, port 3200 on the host or the live data directory,
# and it uses its own docker config directory, so the host's saved logins are not read or written.
# Nothing prints the password or the token.
#
#   CHPROOF_KEEP=1   leave the stack up for poking at (remove it with `docker compose -p chproof-reg down -v`)
#   CODEHAVEN_TAG    server image tag (default proof)
#   CODEHAVEN_PROOF_SUBNET  a free /24 for the throwaway network (required)
set -euo pipefail

tag="${CODEHAVEN_TAG:-proof}"
: "${CODEHAVEN_PROOF_SUBNET:?set CODEHAVEN_PROOF_SUBNET to a free /24 on this host, for example 10.20.30.0/24}"
export CODEHAVEN_PROOF_SUBNET
proj=chproof-reg
# The release of github.com/opencontainers/distribution-spec this runs, pinned by digest.
suite_tag=v1.1.1
suite="ghcr.io/opencontainers/distribution-spec/conformance:$suite_tag@sha256:609201aab0905b1e90ded490e5f0dbaadc9a4bef98aca4cd38ff308f588ed27a"
user=chproof
work="$(mktemp -d)"
export DOCKER_CONFIG="$work/dockercfg"
mkdir -p "$DOCKER_CONFIG"

docker image inspect "codehaven-server:$tag" >/dev/null 2>&1 || {
  echo "registry-conformance: codehaven-server:$tag is missing. Build it first (see the header)." >&2
  exit 1
}

# A free loopback port. Never 3200, which is the live instance's.
free_port() {
  python3 - <<'PY'
import socket
s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()
PY
}
PORT=$(free_port)
SSH_PORT=$(free_port)
[ "$PORT" != 3200 ] && [ "$SSH_PORT" != 2222 ] || { echo "registry-conformance: picked a reserved port" >&2; exit 1; }
export PORT SSH_PORT CODEHAVEN_TAG="$tag"
base="http://127.0.0.1:$PORT"

cat > "$work/compose.yml" <<'YML'
name: chproof-reg
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
  server:
    image: codehaven-server:${CODEHAVEN_TAG}
    depends_on:
      postgres: {condition: service_healthy}
    command: ["codehaven", "serve", "--migrate"]
    environment:
      CODEHAVEN__SERVER__EXTERNAL_URL: http://127.0.0.1:${PORT}
      CODEHAVEN__DATABASE__URL: postgres://codehaven:proof-only-not-a-secret@postgres:5432/codehaven
      CODEHAVEN__STORAGE__DATA_DIR: /data
      CODEHAVEN__SSH__EXTERNAL: 127.0.0.1:${SSH_PORT}
      CODEHAVEN__LOG__FORMAT: pretty
    ports:
      - 127.0.0.1:${PORT}:3200
      - 127.0.0.1:${SSH_PORT}:2222
    # exec: docker mounts tmpfs noexec by default and the server then refuses to start.
    tmpfs: ["/data:exec,uid=10001,gid=10001,mode=0755"]
    healthcheck:
      test: ["CMD", "codehaven", "healthcheck"]
      interval: 2s
      timeout: 5s
      retries: 30
      start_period: 5s
    mem_limit: 2g
# The network's subnet is fixed, because a busy host's default address pools can run out. It
# comes from CODEHAVEN_PROOF_SUBNET: a free /24 on this host.
networks:
  default:
    ipam:
      config:
        - subnet: ${CODEHAVEN_PROOF_SUBNET}
YML
compose=(docker compose -p "$proj" -f "$work/compose.yml")

images=()
cleanup() {
  set +e
  if [ -n "${CHPROOF_KEEP:-}" ]; then
    echo "registry-conformance: CHPROOF_KEEP is set, leaving $proj up on 127.0.0.1:$PORT (ssh $SSH_PORT), work dir $work" >&2
    return
  fi
  "${compose[@]}" down -v --remove-orphans >/dev/null 2>&1
  [ ${#images[@]} -eq 0 ] || docker rmi -f "${images[@]}" >/dev/null 2>&1
  rm -rf "$work"
}
trap cleanup EXIT

step() { echo "registry-conformance: $*"; }
die() { echo "registry-conformance: FAIL: $*" >&2; exit 1; }

# A leftover project from an earlier run would hold the names.
"${compose[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
up() { "${compose[@]}" up -d "$@" >"$work/up.log" 2>&1 || { tail -15 "$work/up.log" >&2; die "docker compose up $* failed"; }; }
up postgres server
st=""
for _ in $(seq 1 40); do
  st=$("${compose[@]}" ps --format '{{.Service}} {{.Health}}' | grep '^server ' | cut -d' ' -f2 || true)
  [ "$st" = healthy ] && break
  sleep 3
done
[ "$st" = healthy ] || { "${compose[@]}" logs --no-color server 2>&1 | tail -15; die "the server did not become healthy"; }
step "stack up on 127.0.0.1:$PORT"

# A made-up account with a password from /dev/urandom, then a token with package access. The
# password goes to the CLI on stdin and into files under $work only; the token is written to a
# file and never printed.
python3 -c 'import secrets; print(secrets.token_urlsafe(24))' > "$work/password"
"${compose[@]}" exec -T server codehaven admin create-user --username "$user" --email chproof@example.test --can-run-ci --password-stdin \
  < "$work/password" >/dev/null 2>&1 || die "could not create the account"

cat > "$work/mktoken.py" <<'PY'
import http.client, re, sys, urllib.parse
host, port, user = sys.argv[1], int(sys.argv[2]), sys.argv[3]
password = open(sys.argv[4]).read().strip()
origin = f"http://{host}:{port}"
jar = {}
def req(method, path, form=None):
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
scopes = sys.argv[5].split(",")
st, text = req("POST", "/settings/tokens", {"_csrf": csrf("/settings/tokens"), "name": "chproof", "scopes": scopes, "expires_in_days": "1"})
m = re.search(r"chp_[A-Za-z0-9]{40}", text)
if st != 200 or not m: sys.exit(f"token create answered {st}")
open(sys.argv[6], "w").write(m.group(0))
PY
python3 "$work/mktoken.py" 127.0.0.1 "$PORT" "$user" "$work/password" "repo,user,read:packages,write:packages" "$work/token" \
  || die "could not sign in and make a token"
chmod 600 "$work/token" "$work/password"
python3 "$work/mktoken.py" 127.0.0.1 "$PORT" "$user" "$work/password" "repo" "$work/token-repo" || die "could not make a repo-only token"
chmod 600 "$work/token-repo"
reg="127.0.0.1:$PORT"

# ---- 1. docker login, push, pull -------------------------------------------------------------
# A token without the packages scope gets through the token exchange but can do nothing.
docker login "$reg" -u "$user" --password-stdin < "$work/token-repo" >/dev/null 2>&1 || die "docker login with a repo-only token failed"
mkdir -p "$work/img"
echo x > "$work/img/x"
printf 'FROM scratch\nCOPY x /x\n' > "$work/img/Dockerfile"
scoped="$reg/$user/scoped:t"
docker build -q -t "$scoped" "$work/img" >/dev/null || die "docker build failed"
images+=("$scoped")
if docker push -q "$scoped" >/dev/null 2>&1; then die "a token without write:packages pushed an image"; fi
step "a repo-only token cannot push: ok"
if docker login "$reg" -u "$user" --password-stdin <<< "not-the-token" >/dev/null 2>&1; then die "docker login accepted a wrong token"; fi
step "docker login refuses a wrong token: ok"
docker login "$reg" -u "$user" --password-stdin < "$work/token" >/dev/null 2>&1 || die "docker login failed"
step "docker login: ok"

# Two layers: a 3 MiB random file (so a range request has something to cut) and a small one.
mkdir -p "$work/app"
head -c 3145728 /dev/urandom > "$work/app/big.bin"
echo "second layer" > "$work/app/small.txt"
printf 'FROM scratch\nCOPY big.bin /big.bin\nCOPY small.txt /small.txt\n' > "$work/app/Dockerfile"
img="$reg/$user/app:v1"
docker build -q -t "$img" "$work/app" >/dev/null || die "docker build failed"
images+=("$img")
sum_before=$(sha256sum < "$work/app/big.bin" | cut -d' ' -f1)
docker push -q "$img" >/dev/null 2>&1 || die "docker push failed"
step "docker push: ok"
docker rmi -f "$img" >/dev/null
docker pull -q "$img" >/dev/null 2>&1 || die "docker pull failed"
cid=$(docker create "$img" /big.bin)
docker cp "$cid:/big.bin" "$work/big.pulled" >/dev/null
docker cp "$cid:/small.txt" "$work/small.pulled" >/dev/null
docker rm "$cid" >/dev/null
[ "$(sha256sum < "$work/big.pulled" | cut -d' ' -f1)" = "$sum_before" ] || die "the pulled layer differs from the pushed one"
[ "$(cat "$work/small.pulled")" = "second layer" ] || die "the pulled small layer differs"
step "docker pull returns the pushed bytes: ok"

# A token with only package scopes pushes and pulls an image linked to a private repository: the
# repository's permissions are the person's, and the token adds package access only.
code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $(cat "$work/token")" -H 'Content-Type: application/json' \
  -d '{"name":"linked","private":true}' "$base/api/v3/user/repos")
[ "$code" = 201 ] || die "creating the repository answered $code"
python3 "$work/mktoken.py" 127.0.0.1 "$PORT" "$user" "$work/password" "read:packages,write:packages" "$work/token-pkg" || die "could not make a package-only token"
chmod 600 "$work/token-pkg"
docker login "$reg" -u "$user" --password-stdin < "$work/token-pkg" >/dev/null 2>&1 || die "docker login with a package-only token failed"
linked="$reg/$user/linked:t"
docker tag "$scoped" "$linked" && images+=("$linked")
docker push -q "$linked" >/dev/null 2>&1 || die "a package-only token could not push an image linked to its owner's private repository"
docker rmi -f "$linked" >/dev/null
docker pull -q "$linked" >/dev/null 2>&1 || die "a package-only token could not pull the linked private image"
step "a package-only token pushes and pulls an image linked to a private repository: ok"
docker login "$reg" -u "$user" --password-stdin < "$work/token" >/dev/null 2>&1 || die "docker login failed"

# ---- 2. range and catalog over plain HTTP ----------------------------------------------------
tokv=$(cat "$work/token")
auth=(-H "Authorization: Bearer $tokv")
accept='application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json'
manifest=$(curl -fsS "${auth[@]}" -H "Accept: $accept" "$base/v2/$user/app/manifests/v1")
# An index (buildx can add attestations) points at the image manifest: follow it.
child=$(python3 -c 'import json,sys; m=json.loads(sys.argv[1]); print(next((x["digest"] for x in m.get("manifests", []) if x.get("platform", {}).get("os") != "unknown"), ""))' "$manifest")
if [ -n "$child" ]; then manifest=$(curl -fsS "${auth[@]}" -H "Accept: $accept" "$base/v2/$user/app/manifests/$child"); fi
layer=$(python3 -c 'import json,sys; m=json.loads(sys.argv[1]); print(max(m["layers"], key=lambda l: l["size"])["digest"])' "$manifest")
# The layer is a tar of the file, so compare a slice with the same bytes of a whole download.
curl -fsS "${auth[@]}" -o "$work/blob.full" "$base/v2/$user/app/blobs/$layer"
total=$(wc -c < "$work/blob.full" | tr -d ' ')
code=$(curl -sS "${auth[@]}" -H 'Range: bytes=1000-1999' -o "$work/slice" -w '%{http_code} %header{content-range}' "$base/v2/$user/app/blobs/$layer")
[ "$code" = "206 bytes 1000-1999/$total" ] || die "range request answered: $code"
dd if="$work/blob.full" of="$work/slice.want" bs=1 skip=1000 count=1000 2>/dev/null
cmp -s "$work/slice" "$work/slice.want" || die "the range body is not bytes 1000-1999 of the layer"
past=$(curl -sS "${auth[@]}" -H 'Range: bytes=9999999-' -o /dev/null -w '%{http_code}' "$base/v2/$user/app/blobs/$layer")
[ "$past" = 416 ] || die "a range past the end answered $past"
step "range request: 206 and 416: ok"
cat="$(curl -fsS "${auth[@]}" "$base/v2/_catalog")"
[ "$cat" = "{\"repositories\":[\"$user/app\",\"$user/linked\"]}" ] || die "catalog answered: $cat"
[ "$(curl -sS -o /dev/null -w '%{http_code}' "$base/v2/_catalog")" = 401 ] || die "the catalog answered a signed-out request"
step "catalog lists the pushed image and refuses signed-out requests: ok"

# ---- 3. the OCI conformance suite ------------------------------------------------------------
docker pull -q "$suite" >/dev/null 2>&1 || die "could not fetch the conformance suite $suite_tag (refused or unreachable); no other source is used"
images+=("$suite")
mkdir -p "$work/report"
chmod 0777 "$work/report"
rc=0
# --network host: the registry is published on the host's loopback only.
docker run --rm --network host -v "$work/report:/report" \
  -e OCI_ROOT_URL="$base" -e OCI_NAMESPACE="$user/conformance" -e OCI_CROSSMOUNT_NAMESPACE="$user/crossmount" \
  -e OCI_USERNAME="$user" -e OCI_PASSWORD="$tokv" \
  -e OCI_TEST_PUSH=1 -e OCI_TEST_PULL=1 -e OCI_TEST_CONTENT_DISCOVERY=1 -e OCI_TEST_CONTENT_MANAGEMENT=1 \
  -e OCI_HIDE_SKIPPED_WORKFLOWS=0 -e OCI_DELETE_MANIFEST_BEFORE_BLOBS=0 -e OCI_REPORT_DIR=/report \
  "$suite" > "$work/conformance.out" 2>&1 || rc=$?
# Scrub the token from anything shown.
sed "s/$tokv/<token>/g" "$work/conformance.out" | tail -25
# On a failure, what each failing spec expected, so the cause is visible without the report.
[ "$rc" = 0 ] || sed "s/$tokv/<token>/g; s/\x1b\[[0-9;]*m//g" "$work/conformance.out" | grep -A14 '\[FAILED\]' | head -150
[ "$rc" = 0 ] || die "the conformance suite $suite_tag failed (exit $rc)"
step "OCI distribution-spec conformance $suite_tag: passed"
step "all checks passed"
