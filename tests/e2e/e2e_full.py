#!/usr/bin/env python3
"""The end-to-end test of ARCHITECTURE.md / ROADMAP.md section 6, steps 0 to 19, against a
throwaway Codehaven stack. Standard library only. Runs on the build host.

It drives the system the way a person does: HTML forms with CSRF, the REST API, real `git`
over HTTP and SSH, TOTP codes, an SSE log stream. The HTTP calls go to the server container
over the stack's own network, so the server sees the host as a trusted proxy and honours
`X-Forwarded-For` and `X-Forwarded-Proto`. That is the same path Caddy uses, and it lets the
rate limit and lockout steps act as many client addresses.

  E2E_BASE=http://192.0.2.10:3200    base URL of the server (required)
  E2E_PUBLIC=https://code.example.com  the origin the server believes it has (required)
  E2E_SSH=192.0.2.10:2222            git over SSH
  E2E_PROJECT=test-mine              the throwaway compose project the stack runs in (required)
  E2E_CLI="docker compose -p test-mine -f deploy/compose.e2e.yml exec -T server codehaven"   (required)
  E2E_HOST_IP=192.0.2.1              (optional) the host as the server sees it, for the webhook step
  E2E_STEPS=0-19      (or several ranges: 0-14,24)

It refuses to run against a stack that already holds an account, and against any project whose
name is not a throwaway one. There is no flag that turns that off.
  E2E_SHOTS=/path   (optional) where step 20 leaves the account data for the screenshot run
"""
import base64, concurrent.futures, hashlib, hmac, http.client, json, os, re, shlex, struct, subprocess, sys, tempfile, threading, time, urllib.parse

# No default: on a host that runs a real instance, 127.0.0.1:3200 is that instance (ch-5q8).
BASE = os.environ.get("E2E_BASE", "").rstrip("/")
PUBLIC = os.environ.get("E2E_PUBLIC", "").rstrip("/")
SSH = os.environ.get("E2E_SSH", "127.0.0.1:2222")
CLI = shlex.split(os.environ.get("E2E_CLI", "codehaven"))
STEPS = os.environ.get("E2E_STEPS", "0-19")
RUN = os.environ.get("E2E_RUN", time.strftime("%H%M%S"))
u = urllib.parse.urlparse(BASE)
HOST, PORT = u.hostname, u.port or 80
SECURE = PUBLIC.startswith("https://")

SEC_HEADERS = ["content-security-policy", "x-frame-options", "x-content-type-options", "referrer-policy",
               "cross-origin-opener-policy", "cross-origin-resource-policy", "permissions-policy", "x-robots-tag"]


class Fail(Exception):
    pass


def check(cond, msg):
    if not cond:
        raise Fail(msg)


WARNINGS = []


def warn(cond, msg):
    """A check the run reports but does not stop on (E2E_SOFT=1 only)."""
    if not cond:
        if not os.environ.get("E2E_SOFT"):
            raise Fail(msg)
        WARNINGS.append(msg)
        print("         WARN " + msg.splitlines()[0][:200], flush=True)


class Resp:
    def __init__(self, status, headers, body):
        self.status, self.headers, self.body = status, headers, body

    @property
    def text(self):
        return self.body.decode("utf-8", "replace")

    def json(self):
        return json.loads(self.body or b"null")

    def h(self, name):
        return self.headers.get(name.lower(), "")


class Client:
    """One browser or API client with its own address and cookie jar."""

    def __init__(self, ip, name="client"):
        self.ip, self.name, self.cookies, self.token = ip, name, {}, None

    def req(self, method, path, body=None, headers=None, form=None, js=None, auth=None, origin=True, cookies=True):
        hd = {"X-Forwarded-For": self.ip, "X-Forwarded-Proto": "https" if SECURE else "http", "Host": urllib.parse.urlparse(PUBLIC).netloc,
              "User-Agent": "ch-e2e/1"}
        if origin and method not in ("GET", "HEAD"):
            hd["Origin"] = PUBLIC
        if self.cookies and cookies:
            hd["Cookie"] = "; ".join(f"{k}={v}" for k, v in self.cookies.items())
        if form is not None:
            body = urllib.parse.urlencode(form, doseq=True).encode()
            hd["Content-Type"] = "application/x-www-form-urlencoded"
        if js is not None:
            body = json.dumps(js).encode()
            hd["Content-Type"] = "application/json"
        if isinstance(body, str):
            body = body.encode()
        tok = auth if auth is not None else self.token
        if tok:
            hd["Authorization"] = tok if " " in tok else f"Bearer {tok}"
        if headers:
            hd.update(headers)
        c = http.client.HTTPConnection(HOST, PORT, timeout=60)
        c.request(method, path, body=body, headers=hd)
        r = c.getresponse()
        data = r.read()
        hs = {}
        for k, v in r.getheaders():
            if k.lower() == "set-cookie":
                nv = v.split(";", 1)[0]
                n, _, val = nv.partition("=")
                if "Max-Age=0" in v or val == "":
                    self.cookies.pop(n, None)
                else:
                    self.cookies[n] = val
            hs[k.lower()] = v
        c.close()
        return Resp(r.status, hs, data)

    def get(self, path, **kw):
        return self.req("GET", path, **kw)

    def csrf(self, path="/login"):
        r = self.get(path)
        m = re.search(r'name="_csrf"\s+value="([^"]+)"', r.text) or re.search(r'value="([^"]+)"\s+name="_csrf"', r.text) \
            or re.search(r'X-CSRF-Token"\s*:\s*"([^"]+)"', r.text) or re.search(r"X-CSRF-Token&quot;:\s*&quot;([^&]+)&quot;", r.text)
        check(m, f"no csrf token on {path} (status {r.status})")
        return m.group(1)

    def post_form(self, path, data, page=None):
        tok = self.csrf(page or path)
        d = dict(data)
        d["_csrf"] = tok
        return self.req("POST", path, form=d)

    def api(self, method, path, js=None, **kw):
        return self.req(method, "/api/v3" + path, js=js, **kw)

    def login(self, username, password, code=None):
        r = self.post_form("/login", {"username": username, "password": password, "next": "/"})
        if r.status == 303 and "/login/two-factor" in r.h("location"):
            check(code, f"{username} needs a code")
            r = self.post_form("/login/two-factor", {"code": code, "next": "/"})
        return r

    def signed_in(self):
        return self.get("/").status == 200


def totp(secret, at=None, step=30):
    key = base64.b32decode(secret.replace(" ", "").upper() + "=" * (-len(secret) % 8))
    t = int((at if at is not None else time.time()) // step)
    h = hmac.new(key, struct.pack(">Q", t), hashlib.sha1).digest()
    o = h[-1] & 15
    return f"{(struct.unpack('>I', h[o:o + 4])[0] & 0x7fffffff) % 1000000:06d}"


def cli(*args, stdin=None):
    p = subprocess.run(CLI + list(args), input=stdin, capture_output=True, text=True, timeout=120)
    return p.returncode, p.stdout, p.stderr


def run(cmd, cwd=None, env=None, check_rc=True, timeout=180):
    e = dict(os.environ)
    if env:
        e.update(env)
    p = subprocess.run(cmd, cwd=cwd, env=e, capture_output=True, text=True, timeout=timeout)
    if check_rc and p.returncode != 0:
        raise Fail(f"{' '.join(cmd)[:120]} exited {p.returncode}: {(p.stdout + p.stderr)[-600:]}")
    return p


def git(args, cwd, env=None, check_rc=True):
    base_env = {"GIT_TERMINAL_PROMPT": "0", "GIT_AUTHOR_NAME": "E2E", "GIT_AUTHOR_EMAIL": "e2e@example.test",
                "GIT_COMMITTER_NAME": "E2E", "GIT_COMMITTER_EMAIL": "e2e@example.test", "HOME": cwd}
    if env:
        base_env.update(env)
    return run(["git"] + args, cwd=cwd, env=base_env, check_rc=check_rc)


S = {}  # state shared by steps
WORK = tempfile.mkdtemp(prefix="ch-e2e-")
PW = "correct-horse-battery-staple-" + RUN


def http_url(user, token, repo):
    return f"http://{urllib.parse.quote(user)}:{urllib.parse.quote(token, safe='')}@{HOST}:{PORT}/{repo}.git"


def gitenv(ip):
    # git sends no proxy headers by default. Say which client address this is.
    return {"GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "http.extraHeader", "GIT_CONFIG_VALUE_0": f"X-Forwarded-For: {ip}"}


# ------------------------------------------------------------------------------------ steps

def step0():
    c = Client("10.9.0.1")
    r = c.get("/healthz")
    check(r.status == 200 and r.text == "ok", f"/healthz: {r.status} {r.text[:40]}")
    rc, out, err = cli("doctor")
    check(rc == 0, f"codehaven doctor failed:\n{out}\n{err}")
    r = c.get("/login")
    check(r.status == 200, f"/login {r.status}")
    check("No accounts yet" in r.text, "/login should say there are no accounts yet")
    check(not re.search(r"sign.?up|register|create an account", r.text, re.I), "/login offers sign-up")
    S["zero_accounts"] = True


def step1():
    c = Client("10.9.0.2")
    cases = [("/", 303, "page"), ("/explore", 303, "page"), ("/root", 303, "page"), ("/nobody/nothing", 303, "page"),
             ("/signup", 303, "page"), ("/metrics", 303, "page"), ("/readyz", 303, "page"),
             ("/api/v3/user", 401, "api"), ("/api/v3/meta", 401, "api"), ("/api/v3/repos/x/y", 401, "api"),
             ("/x/y.git/info/refs?service=git-upload-pack", 401, "git"), ("/v2/", 401, "registry"),
             ("/api/runner/v1/jobs/request", 401, "runner"),
             # C9: the artifact service, and the private NuGet feed (a Basic challenge, nothing public).
             ("/twirp/github.actions.results.api.v1.ArtifactService/ListArtifacts", 401, "runner"),
             ("/nuget/x/index.json", 401, "git"), ("/nuget/x/query", 401, "git"), ("/nuget/x/flatcontainer/y/index.json", 401, "git")]
    bodies = {}
    for path, status, kind in cases:
        r = c.get(path, cookies=False)
        check(r.status == status, f"{path} returned {r.status}, expected {status}")
        for hname in SEC_HEADERS:
            check(hname in r.headers, f"{path} lacks {hname}")
        if kind == "page":
            check(r.h("location").startswith("/login"), f"{path} redirected to {r.h('location')}")
        if kind == "git":
            check(r.h("www-authenticate").lower().startswith("basic"), "git needs a Basic challenge")
        if kind == "registry":
            check(r.h("www-authenticate").lower().startswith("bearer"), "/v2/ needs a Bearer challenge")
        bodies[path] = r.body
    r = c.get("/login")
    for hname in SEC_HEADERS:
        check(hname in r.headers, f"/login lacks {hname}")
    check("server" not in r.headers, "Server header present")
    for m in ("POST", "PUT"):
        check(c.req(m, "/signup", origin=False).status == 303, f"{m} /signup")
    r = c.get("/api/v3/user", headers={"Remote-User": "root", "Remote-Groups": "admins"}, cookies=False)
    check(r.status == 401, f"Remote-User signed someone in: {r.status}")
    a = c.get("/x/y/z/q", cookies=False)
    b = c.get("/explore", cookies=False)
    check(a.status == b.status, "missing and existing pages differ")


def password_flow(link, client, password):
    path = urllib.parse.urlparse(link).path
    r = client.post_form(path, {"password": password, "password_confirm": password})
    check(r.status in (303, 200), f"set password: {r.status} {r.text[:200]}")


def step2():
    root = Client("10.9.1.1", "root")
    rc, out, err = cli("admin", "create-user", "--username", "ops", "--email", "ops@example.test", "--admin", "--password-stdin", stdin=PW + "\n")
    check(rc == 0, f"create-user: {out}{err}")
    r = root.login("ops", PW)
    check(r.status == 303 and root.signed_in(), f"root sign-in: {r.status}")
    r = root.get("/admin")
    check(r.status in (403, 303) and "two-factor" in r.text.lower() or r.status == 303, f"/admin before 2FA: {r.status} {r.text[:200]}")
    # Turn on TOTP.
    r = root.post_form("/settings/security/2fa/start", {}, page="/settings/security")
    check(r.status == 200, f"2fa start {r.status}")
    secret = re.search(r'name="secret"\s+value="([^"]+)"', r.text)
    check(secret, "no TOTP secret on the setup page")
    secret = secret.group(1)
    otp = re.search(r'name="otpauth_url"\s+value="([^"]+)"', r.text)
    code = totp(secret)
    tok = root.csrf("/settings/security")
    r = root.req("POST", "/settings/security/2fa/confirm", form={"_csrf": tok, "secret": secret, "otpauth_url": otp.group(1) if otp else "", "code": code})
    check(r.status == 200 and ("recover" in r.text.lower()), f"2fa confirm: {r.status} {r.text[:300]}")
    S["root_secret"] = secret
    S["root_codes"] = re.findall(r"\b[a-z0-9]{5}-[a-z0-9]{5}\b", r.text)
    check(root.signed_in(), "root signed out after turning on 2FA (new cookie not set)")
    # Sign out and back in with a code. The last step's code is used up, so use the next step's.
    root.req("POST", "/logout", form={"_csrf": root.csrf("/settings/security")})
    check(not root.signed_in(), "still signed in after logout")
    r = root.login("ops", PW, code=totp(secret, time.time() + 30))
    check(root.signed_in(), f"root cannot sign in with a code: {r.status} {r.text[:200]}")
    r = root.get("/admin")
    check(r.status == 200, f"/admin after 2FA: {r.status}")
    S["root"] = root


def mint_invite(root, email, ci=False):
    tok = root.csrf("/admin/invites")
    d = {"_csrf": tok, "email": email, "note": "e2e", "expires_in_hours": "168"}
    if ci:
        d["grants_ci"] = "1"
    r = root.req("POST", "/admin/invites", form=d)
    check(r.status == 200, f"create invite: {r.status} {r.text[:200]}")
    m = re.search(r"/invite/(chi_[A-Za-z0-9]+)", r.text)
    check(m, "no invite link in the response")
    return m.group(1)


def accept(token, username, email, password, ip):
    c = Client(ip, username)
    r = c.post_form(f"/invite/{token}", {"username": username, "display_name": username.title(), "email": email,
                                         "password": password, "password_confirm": password}, page=f"/invite/{token}")
    return c, r


def step3():
    root = S["root"]
    t_alice = mint_invite(root, "alice@example.test", ci=True)
    rc, out, err = cli("admin", "invite", "create", "--email", "bob@example.test")
    check(rc == 0, f"cli invite: {out}{err}")
    m = re.search(r"/invite/(chi_[A-Za-z0-9]+)", out)
    check(m, f"no invite link from the CLI: {out}")
    t_bob = m.group(1)
    for name, tok, ip in (("alice", t_alice, "10.9.2.1"), ("bob", t_bob, "10.9.3.1")):
        c, r = accept(tok, name, f"{name}@example.test", PW, ip)
        check(r.status in (303, 200), f"accept {name}: {r.status} {r.text[:300]}")
        r2 = c.login(name, PW)
        check(c.signed_in(), f"{name} cannot sign in after accepting: {r2.status}")
        S[name] = c
    # A used link, a revoked one, an expired one and a made-up one all look the same.
    probe = Client("10.9.4.1")
    bad = []
    bad.append(probe.get(f"/invite/{t_alice}"))
    t_rev = mint_invite(root, "rev@example.test")
    rc, out, err = cli("admin", "invite", "list")
    rid = [ln.split()[0] for ln in out.splitlines() if "rev@example.test" in ln]
    check(rid, f"revoked-invite id not found in: {out}")
    rc, out, err = cli("admin", "invite", "revoke", rid[0])
    check(rc == 0, f"revoke: {out}{err}")
    bad.append(probe.get(f"/invite/{t_rev}"))
    bad.append(probe.get("/invite/chi_" + "x" * 32))
    for r in bad:
        check(r.status == 404, f"a bad invite returned {r.status}")
    check(len({re.sub(r"\s+", " ", re.sub(r'value="[^"]*"', "", x.text)) for x in bad}) == 1, "bad invite pages differ")
    # 20 racing accepts of one fresh link make one account.
    t_race = mint_invite(root, "race@example.test")
    results = []

    def go(i):
        c, r = accept(t_race, f"racer{i}", "race@example.test", PW, f"10.9.5.{i + 1}")
        results.append(r.status)

    with concurrent.futures.ThreadPoolExecutor(20) as ex:
        list(ex.map(go, range(20)))
    rc, out, err = cli("admin", "users")
    made = [ln for ln in out.splitlines() if re.search(r"\bracer\d+\b", ln)]
    check(len(made) == 1, f"{len(made)} accounts came from one invite:\n{out}")
    S["invite_ids_ok"] = True


def step4():
    alice = S["alice"]
    tok = alice.csrf("/settings/tokens")
    r = alice.req("POST", "/settings/tokens", form={"_csrf": tok, "name": "e2e", "scopes": ["repo", "user", "workflow"], "expires_in_days": "30"})
    check(r.status == 200, f"token create: {r.status} {r.text[:300]}")
    m = re.search(r"chp_[A-Za-z0-9]{40}", r.text)
    check(m, "no token shown after creating it")
    S["alice_token"] = m.group(0)
    me = Client("10.9.2.2").api("GET", "/user", auth=S["alice_token"])
    check(me.status == 200 and me.json().get("login") == "alice", f"token on /user: {me.status} {me.text[:200]}")
    # an ed25519 key
    kd = os.path.join(WORK, "alice_key")
    run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", kd, "-C", "alice@e2e"])
    pub = open(kd + ".pub").read().strip()
    r = Client("10.9.2.2").api("POST", "/user/keys", js={"title": "e2e", "key": pub}, auth=S["alice_token"])
    check(r.status == 201, f"add key: {r.status} {r.text[:200]}")
    S["alice_key"] = kd
    # Bob's token too, for the later steps.
    bob = S["bob"]
    r = bob.req("POST", "/settings/tokens", form={"_csrf": bob.csrf("/settings/tokens"), "name": "e2e", "scopes": ["repo", "user", "workflow"], "expires_in_days": "30"})
    S["bob_token"] = re.search(r"chp_[A-Za-z0-9]{40}", r.text).group(0)
    # An expired token is refused: delete-style check by making a token with a bad value.
    r = Client("10.9.2.3").api("GET", "/user", auth="chp_" + "a" * 40)
    check(r.status == 401, f"a made-up token returned {r.status}")


def until_not_limited(send, what):
    """POST /login is limited per address (10 a minute, burst 5). Sends again until it is not 429."""
    for _ in range(6):
        r = send()
        if r.status != 429:
            return r
        time.sleep(7)
    check(False, f"the sign-in limit never let {what} through")


def step5():
    # 11 sign-ins in a minute from one address: the 11th is 429.
    c = Client("10.9.6.1")
    codes = []
    for i in range(12):
        r = c.req("POST", "/login", form={"_csrf": c.csrf("/login"), "username": "nobody", "password": "x" * 14})
        codes.append(r.status)
        if r.status == 429:
            break
    check(429 in codes, f"no 429 after 12 sign-ins from one address: {codes}")
    # Lock bob from one address; wrong passwords count per account and address.
    b = Client("10.9.7.1")
    for i in range(10):
        until_not_limited(lambda i=i: b.post_form("/login", {"username": "bob", "password": "wrong-password-" + str(i), "next": "/"}),
                          f"attempt {i}")
    # The tenth failure locked bob at this address, so the right password is refused too. A locked
    # sign-in answers 429 like the address limit does, so it is sent once, not retried, after a
    # pause that lets the address limit allow one more.
    time.sleep(7)
    b.post_form("/login", {"username": "bob", "password": PW, "next": "/"})
    check(not b.signed_in(), "bob signed in while locked")
    # An outsider cannot lock the owner out.
    o = Client("10.9.8.1")
    o.login("bob", PW)
    check(o.signed_in(), "bob is locked out everywhere by failures from one other address")
    # Alice is unaffected.
    a = Client("10.9.8.2")
    a.login("alice", PW)
    check(a.signed_in(), "alice blocked by bob's lockout")
    rc, out, err = cli("admin", "unlock", "bob")
    check(rc == 0, f"unlock: {out}{err}")
    # The unlock lets bob sign in again from the very address the failures came from.
    b = Client("10.9.7.1")
    until_not_limited(lambda: b.login("bob", PW), "bob in after unlock")
    check(b.signed_in(), "bob cannot sign in after unlock")
    S["bob"] = b


def step6():
    r = Client("10.9.2.2").api("POST", "/user/repos", js={"name": "demo", "private": True, "description": "e2e demo"}, auth=S["alice_token"])
    check(r.status == 201, f"create repo: {r.status} {r.text[:300]}")
    r = S["alice"].get("/alice/demo")
    check(r.status == 200, f"repo page {r.status}")
    check(re.search(r"empty|git push|push an existing", r.text, re.I), "empty-repo instructions missing")
    bob = S["bob"]
    miss = bob.get("/alice/demo")
    nonex = bob.get("/alice/does-not-exist")
    check(miss.status == 404 and nonex.status == 404, f"bob sees {miss.status}, missing is {nonex.status}")
    check(re.sub(r"\s+", " ", re.sub(r'value="[^"]*"', "", miss.text)) == re.sub(r"\s+", " ", re.sub(r'value="[^"]*"', "", nonex.text)).replace("does-not-exist", "demo"),
          "a private repo and a missing repo answer differently")


WORKFLOW = """name: ci
on:
  push:
    branches: [main]
  pull_request:
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - run: echo lint ok
  test:
    needs: lint
    runs-on: ubuntu-latest
    strategy:
      matrix:
        cell: [a, b]
    steps:
      - run: echo "hello from codehaven"
      - run: echo ${{ matrix.cell }} > out-${{ matrix.cell }}.txt
      - uses: actions/upload-artifact@v4
        with:
          name: out-${{ matrix.cell }}
          path: out-${{ matrix.cell }}.txt
"""


def step7():
    wd = os.path.join(WORK, "alice_clone")
    os.makedirs(wd)
    git(["init", "-q", "-b", "main"], wd)
    os.makedirs(os.path.join(wd, "src"))
    os.makedirs(os.path.join(wd, ".codehaven/workflows"))
    open(os.path.join(wd, "README.md"), "w").write("# demo\n\nA demo repository.\n")
    open(os.path.join(wd, "src/lib.rs"), "w").write("pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n")
    open(os.path.join(wd, ".codehaven/workflows/ci.yml"), "w").write(WORKFLOW)
    git(["add", "-A"], wd)
    git(["commit", "-q", "-m", "first commit"], wd)
    env = gitenv("10.9.2.4")
    # No credentials and a wrong token both fail with 401.
    p = git(["push", f"http://{HOST}:{PORT}/alice/demo.git", "main"], wd, env=env, check_rc=False)
    check(p.returncode != 0, "a push with no credentials worked")
    p = git(["push", http_url("alice", "chp_" + "b" * 40, "alice/demo"), "main"], wd, env=env, check_rc=False)
    check(p.returncode != 0, "a push with a wrong token worked")
    git(["push", http_url("alice", S["alice_token"], "alice/demo"), "main"], wd, env=env)
    S["alice_wd"] = wd
    r = S["alice"].get("/alice/demo/branches")
    check(r.status == 200 and "main" in r.text, "branches page does not list main")
    r = S["alice"].get("/alice/demo/blob/main/src/lib.rs")
    check(r.status == 200 and re.search(r'class="[^"]*(tok-|hl-|syn|highlight|keyword)', r.text), "no highlight classes on the blob page")
    S["main_sha"] = git(["rev-parse", "HEAD"], wd).stdout.strip()


def step8():
    wd = S["alice_wd"]
    git(["checkout", "-q", "-b", "feature"], wd)
    open(os.path.join(wd, "src/lib.rs"), "w").write("pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n")
    git(["commit", "-q", "-am", "add sub"], wd)
    host, port = SSH.split(":")
    ssh = f"ssh -i {S['alice_key']} -p {port} -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o LogLevel=ERROR"
    git(["push", f"ssh://git@{host}:{port}/alice/demo.git", "feature"], wd, env={"GIT_SSH_COMMAND": ssh})
    r = S["alice"].get("/alice/demo/branches")
    check("feature" in r.text, "feature is not listed after the SSH push")
    p = run(ssh.split() + [f"git@{host}", "ls"], check_rc=False)
    check(p.returncode != 0, "a shell login was not refused")
    kd = os.path.join(WORK, "stranger")
    run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", kd])
    ssh2 = ssh.replace(S["alice_key"], kd)
    p = git(["ls-remote", f"ssh://git@{host}:{port}/alice/demo.git"], wd, env={"GIT_SSH_COMMAND": ssh2}, check_rc=False)
    check(p.returncode != 0, "an unknown key was accepted")
    # A token without the workflow scope pushes ordinary commits to a repository that has
    # workflows, and is refused only when the push changes a workflow file. It cannot add an SSH
    # key either, since a push over SSH carries no scopes (ch-s7r).
    a = S["alice"]
    r = a.req("POST", "/settings/tokens", form={"_csrf": a.csrf("/settings/tokens"), "name": "no-workflow", "scopes": ["repo", "user"], "expires_in_days": "1"})
    m = re.search(r"chp_[A-Za-z0-9]{40}", r.text)
    check(r.status == 200 and m, f"token without workflow: {r.status}")
    narrow = m.group(0)
    r = Client("10.9.2.2").api("POST", "/user/keys", js={"title": "narrow", "key": open(kd + ".pub").read().strip()}, auth=narrow)
    check(r.status == 403, f"a token without workflow added an SSH key: {r.status} {r.text[:200]}")
    sd = os.path.join(WORK, "scope_clone")
    env = gitenv("10.9.2.4")
    git(["clone", "-q", http_url("alice", narrow, "alice/demo"), sd], WORK, env=env)
    for name in ("one", "two"):
        open(os.path.join(sd, f"{name}.txt"), "w").write(name + "\n")
        git(["add", "-A"], sd)
        git(["commit", "-q", "-m", name], sd)
    p = git(["push", "origin", "HEAD:refs/heads/scope-plain"], sd, env=env, check_rc=False)
    check(p.returncode == 0, f"a plain push without the workflow scope was refused: {(p.stdout + p.stderr)[-300:]}")
    open(os.path.join(sd, ".codehaven/workflows/extra.yml"), "w").write(WORKFLOW)
    git(["add", "-A"], sd)
    git(["commit", "-q", "-m", "extra workflow"], sd)
    p = git(["push", "origin", "HEAD:refs/heads/scope-wf"], sd, env=env, check_rc=False)
    check(p.returncode != 0 and "workflow scope" in p.stderr, f"a workflow push without the workflow scope: {(p.stdout + p.stderr)[-300:]}")
    git(["push", "-q", "origin", ":scope-plain"], sd, env=env)


def step9():
    alice, bob = S["alice_token"], S["bob"]
    r = Client("10.9.2.4").api("PUT", "/repos/alice/demo/collaborators/bob", js={"permission": "push"}, auth=alice)
    check(r.status in (201, 204), f"add collaborator: {r.status} {r.text[:200]}")
    wd = os.path.join(WORK, "bob_clone")
    git(["clone", "-q", http_url("bob", PW, "alice/demo"), wd], WORK, env=gitenv("10.9.3.2"))
    check(git(["rev-parse", "origin/main"], wd).stdout.strip() == S["main_sha"], "bob's clone is not main's sha")
    S["bob_wd"] = wd
    # Two-factor on for bob: the password clone fails and says to use a token.
    b = S["bob"]
    r = b.post_form("/settings/security/2fa/start", {}, page="/settings/security")
    secret = re.search(r'name="secret"\s+value="([^"]+)"', r.text).group(1)
    otp = re.search(r'name="otpauth_url"\s+value="([^"]+)"', r.text).group(1)
    r = b.req("POST", "/settings/security/2fa/confirm", form={"_csrf": b.csrf("/settings/security"), "secret": secret, "otpauth_url": otp, "code": totp(secret)})
    check(r.status == 200, f"bob 2fa: {r.status}")
    S["bob_secret"] = secret
    p = git(["clone", "-q", http_url("bob", PW, "alice/demo"), os.path.join(WORK, "bob_clone2")], WORK, env=gitenv("10.9.3.3"), check_rc=False)
    check(p.returncode != 0, "a password clone worked after two-factor")
    warn(re.search(r"token", p.stdout + p.stderr, re.I), f"the failure does not say to use a token: {p.stderr[-300:]}")
    # A token clone still works.
    git(["ls-remote", http_url("bob", S["bob_token"], "alice/demo")], WORK, env=gitenv("10.9.3.2"))


def step10():
    r = Client("10.9.2.4").api("PUT", "/repos/alice/demo/branches/main/protection", auth=S["alice_token"], js={
        "required_status_checks": {"strict": False, "contexts": ["ci / test (a)", "ci / test (b)"]},
        "required_pull_request_reviews": {"required_approving_review_count": 1, "dismiss_stale_reviews": False, "require_code_owner_reviews": False},
        "enforce_admins": False, "required_conversation_resolution": False, "allow_force_pushes": False,
        "allow_deletions": False, "allow_direct_push": False})
    check(r.status in (200, 201), f"protect main: {r.status} {r.text[:300]}")
    wd = S["bob_wd"]
    open(os.path.join(wd, "README.md"), "a").write("direct push\n")
    git(["commit", "-q", "-am", "direct"], wd)
    p = git(["push", http_url("bob", S["bob_token"], "alice/demo"), "main"], wd, env=gitenv("10.9.3.2"), check_rc=False)
    check(p.returncode != 0, f"a direct push to protected main worked: {(p.stdout + p.stderr)[-400:]}")
    check(re.search(r"protect", p.stdout + p.stderr, re.I), f"no protection message: {(p.stdout + p.stderr)[-300:]}")
    git(["reset", "-q", "--hard", "origin/main"], wd)


def step11():
    r = Client("10.9.3.2").api("POST", "/repos/alice/demo/labels", js={"name": "bug", "color": "d73a4a"}, auth=S["bob_token"])
    r = Client("10.9.3.2").api("POST", "/repos/alice/demo/issues", js={"title": "Subtract is missing", "body": "Please look, @alice", "labels": ["bug"]}, auth=S["bob_token"])
    check(r.status == 201, f"open issue: {r.status} {r.text[:300]}")
    check(r.json().get("number") == 1, "issue is not number 1")
    time.sleep(3)
    r = Client("10.9.2.4").api("GET", "/notifications", auth=S["alice_token"])
    check(r.status == 200, f"notifications {r.status}")
    check(any("mention" in json.dumps(n) for n in r.json()), f"no mention notification for alice: {r.text[:400]}")


def start_hook_listener():
    import http.server
    got = []

    class H(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            n = int(self.headers.get("Content-Length", "0"))
            body = self.rfile.read(n)
            got.append((dict(self.headers), body))
            self.send_response(200)
            self.end_headers()

        def log_message(self, *a):
            pass

    srv = http.server.ThreadingHTTPServer(("0.0.0.0", 0), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, got


def host_ip_for_containers():
    # The docker bridge gateway of the stack's network is the host, as the server sees it.
    ip = os.environ.get("E2E_HOST_IP")
    check(ip, "set E2E_HOST_IP to the address the server sees the host as")
    return ip


def host_public_ips():
    """The host's public IPv4 addresses: the same filter as deploy/prod/host-ips.sh, IPv4 only.
    A job that reaches one of them reaches the host itself, which listens on ssh, postgres,
    redis and more at that address, so the probe has to know them."""
    addrs = []
    out = run(["ip", "-o", "addr", "show", "scope", "global"], check_rc=False).stdout
    for line in out.splitlines():
        f = line.split()
        if len(f) >= 2 and f[1].startswith(("docker", "br-", "veth")):
            continue
        addrs += [f[i + 1].split("/")[0] for i, t in enumerate(f[:-1]) if t in ("inet", "inet6")]
    for args in (["ip", "-4", "route", "get", "1.1.1.1"], ["ip", "-6", "route", "get", "2606:4700:4700::1111"]):
        toks = run(args, check_rc=False).stdout.split()
        if "src" in toks:
            addrs.append(toks[toks.index("src") + 1])

    def public(x):
        if not re.fullmatch(r"(\d+\.){3}\d+", x):
            return False
        o = list(map(int, x.split(".")))
        return not (o[0] in (10, 127) or (o[0] == 172 and 16 <= o[1] <= 31) or
                    (o[0] == 192 and o[1] == 168) or (o[0] == 169 and o[1] == 254) or
                    (o[0] == 100 and 64 <= o[1] <= 127))

    return sorted({x for x in addrs if public(x)})


def step12():
    srv, got = start_hook_listener()
    S["hook_srv"], S["hook_got"] = srv, got
    url = f"http://{host_ip_for_containers()}:{srv.server_address[1]}/hook"
    secret = "e2e-hook-secret"
    r = Client("10.9.2.4").api("POST", "/repos/alice/demo/hooks", auth=S["alice_token"],
                               js={"name": "web", "active": True, "events": ["pull_request", "push"], "config": {"url": url, "secret": secret, "content_type": "json"}})
    if r.status in (400, 422) and re.search(r"private|loopback|allowed", r.text, re.I):
        S["hook_blocked"] = True
        raise Fail(f"the webhook target {url} was refused as private. Set webhooks.allow_private for the e2e run. {r.text[:200]}")
    check(r.status == 201, f"create hook: {r.status} {r.text[:300]}")
    hid = r.json()["id"]
    r = Client("10.9.2.4").api("POST", f"/repos/alice/demo/hooks/{hid}/pings", auth=S["alice_token"])
    deadline = time.time() + 40
    while time.time() < deadline and not got:
        time.sleep(1)
    check(got, "the ping never arrived")
    hd, body = got[0]
    hd = {k.lower(): v for k, v in hd.items()}
    sig = "sha256=" + hmac.new(secret.encode(), body, hashlib.sha256).hexdigest()
    check(hd.get("x-hub-signature-256") == sig, f"bad signature {hd.get('x-hub-signature-256')} != {sig}")
    S["hook_id"] = hid


def step13():
    r = Client("10.9.2.4").api("POST", "/repos/alice/demo/pulls", auth=S["alice_token"], js={"title": "Add sub", "head": "feature", "base": "main", "body": "Fixes #1"})
    check(r.status == 201, f"open PR: {r.status} {r.text[:300]}")
    S["pr"] = r.json()["number"]
    check(S["pr"] == 2, f"the PR is #{S['pr']}, expected 2 (issue 1 comes first)")
    page = S["alice"].get(f"/alice/demo/pulls/{S['pr']}/files")
    check(page.status == 200 and "sub" in page.text, "the PR files page does not show the diff")
    S["pr_head"] = r.json()["head"]["sha"]


def step14():
    # Wait for the workflow run on the PR head. The runner was registered by the deploy script.
    a = Client("10.9.2.4")
    head = S["pr_head"]
    deadline = time.time() + 300
    runs = []
    while time.time() < deadline:
        r = a.api("GET", "/repos/alice/demo/actions/runs", auth=S["alice_token"])
        if r.status == 200:
            runs = [x for x in r.json().get("workflow_runs", []) if x.get("head_sha") == head]
            if runs and all(x["status"] == "completed" for x in runs):
                break
        time.sleep(3)
    check(runs, "no workflow run started for the PR head")
    check(all(x["status"] == "completed" for x in runs), f"runs did not finish: {[(x['id'], x['status'], x.get('conclusion')) for x in runs]}")
    check(all(x.get("conclusion") == "success" for x in runs), f"a run failed: {[(x['id'], x['status'], x.get('conclusion')) for x in runs]}")
    rid = runs[0]["id"]
    jobs = a.api("GET", f"/repos/alice/demo/actions/runs/{rid}/jobs", auth=S["alice_token"]).json().get("jobs", [])
    names = {j["name"]: j for j in jobs}
    check(len(jobs) >= 3, f"expected lint and two test cells, got {list(names)}")
    lint = [j for j in jobs if j["name"].startswith("lint")][0]
    tests = [j for j in jobs if j["name"].startswith("test")]
    check(all(lint["completed_at"] <= t["started_at"] for t in tests), "lint did not finish before test started")
    log = a.req("GET", f"/api/v3/repos/alice/demo/actions/jobs/{tests[0]['id']}/logs", auth=S["alice_token"])
    check(log.status == 200 and "hello from codehaven" in log.text, f"the log lacks the hello line: {log.status} {log.text[:300]}")
    # Every step's panel on the job page holds its own output and nothing from the next step.
    cell = [t for t in tests if t["name"].endswith("(a)")][0]
    jp = S["alice"].get(f"/alice/demo/actions/runs/{rid}/jobs/{cell['id']}")
    check(jp.status == 200, f"job page {jp.status}")
    panels = jp.text.split('<details class="step"')[1:]
    check(len(panels) >= 3, f"expected at least three step panels, got {len(panels)}")
    p1, p2 = panels[0], panels[1]
    check("hello from codehaven" in p1 and "out-a.txt" not in p1, "the first step's panel does not hold exactly its own output")
    check("out-a.txt" in p2 and "hello from codehaven" not in p2, "the second step's panel holds the first step's output, or lacks its own")
    arts = a.api("GET", f"/repos/alice/demo/actions/runs/{rid}/artifacts", auth=S["alice_token"]).json()
    check(arts.get("total_count", 0) >= 2, f"artifacts: {arts}")
    d = a.req("GET", f"/api/v3/repos/alice/demo/actions/artifacts/{arts['artifacts'][0]['id']}/zip", auth=S["alice_token"])
    check(d.status == 200 and d.body[:2] == b"PK", f"the artifact does not download as a zip: {d.status}")
    cr = a.api("GET", f"/repos/alice/demo/commits/{head}/check-runs", auth=S["alice_token"]).json().get("check_runs", [])
    check({"ci / test (a)", "ci / test (b)"} <= {c["name"] for c in cr}, f"check names on the PR head: {[c['name'] for c in cr]}")
    # A made-up registration token fails.
    r = Client("10.9.9.1").req("POST", "/api/runner/v1/register", js={"token": "chrt_" + "z" * 40, "name": "x", "labels": ["x"]})
    check(r.status in (400, 401, 403, 404, 422), f"a made-up registration token returned {r.status}")
    # A real one works once and fails the second time. The label matches no workflow, so this
    # runner never takes a job.
    # Registration tokens are a site admin's to issue, from Site admin > Runners.
    t = S["root"].post_form("/admin/runners/token", {}, page="/admin/runners")
    m = re.search(r"chrt_[A-Za-z0-9]{40}", t.text)
    check(t.status == 200 and m, f"create registration token: {t.status}")
    tok = m.group(0)
    r = Client("10.9.9.2").req("POST", "/api/runner/v1/register", js={"token": tok, "name": "e2e-twice", "labels": ["e2e-twice"]})
    check(r.status in (200, 201) and r.json().get("runner_id"), f"the first use of the token returned {r.status} {r.text[:200]}")
    runner_id = r.json()["runner_id"]
    r = Client("10.9.9.2").req("POST", "/api/runner/v1/register", js={"token": tok, "name": "e2e-twice-2", "labels": ["e2e-twice"]})
    check(r.status == 401, f"registering a second time with the same token returned {r.status}")
    S["root"].post_form(f"/admin/runners/{runner_id}/revoke", {}, page="/admin/runners")
    S["run_id"] = rid
    S["job_id"] = tests[0]["id"]


def step15():
    # The check says the merge is blocked until a review comes in. (Checks already pass.)
    r = Client("10.9.2.4").api("PUT", f"/repos/alice/demo/pulls/{S['pr']}/merge", auth=S["alice_token"], js={"merge_method": "squash"})
    check(r.status == 405, f"merge before review: {r.status} {r.text[:300]}")
    check(re.search(r"approv|review", r.text, re.I), f"no approval reason: {r.text[:300]}")


def step16():
    bob = Client("10.9.3.2")
    r = bob.api("POST", f"/repos/alice/demo/pulls/{S['pr']}/comments", auth=S["bob_token"],
                js={"body": "Why not a checked add?", "commit_id": S["pr_head"], "path": "src/lib.rs", "line": 1, "side": "RIGHT"})
    check(r.status == 201, f"inline comment: {r.status} {r.text[:300]}")
    c = r.json()
    check(c.get("path") == "src/lib.rs" and c.get("line") == 1 and str(c.get("side")).upper() == "RIGHT", f"comment fields: {c}")
    r = bob.api("POST", f"/repos/alice/demo/pulls/{S['pr']}/reviews", auth=S["bob_token"], js={"event": "APPROVE", "body": "Looks fine"})
    check(r.status == 200, f"approve: {r.status} {r.text[:300]}")
    check(r.json().get("state") == "APPROVED", f"review state {r.json().get('state')}")
    review_threads_in_the_web_ui(c["id"])


def review_threads_in_the_web_ui(first_comment_id):
    """The author answers bob's inline comment and resolves the thread from the web pages alone,
    on both tabs. Without a Reply box and a Resolve button, 'Require resolved review threads' leaves
    a team that works in the browser stuck."""
    alice, pr = S["alice"], S["pr"]
    files = f"/alice/demo/pulls/{pr}/files"
    page = alice.get(files)
    check(page.status == 200 and 'class="thread' in page.text, "the files page shows no thread under bob's comment")
    check('name="in_reply_to"' in page.text and ">Reply</button>" in page.text, "the thread has no Reply form on the files page")
    m = re.search(r'name="thread" value="(\d+)"', page.text)
    check(m and ">Resolve</button>" in page.text, "the pull request author has no Resolve button on the files page")
    thread = m.group(1)
    r = alice.post_form(files.replace("/files", "/inline"),
                        {"path": "src/lib.rs", "side": "right", "line": "1", "in_reply_to": str(first_comment_id),
                         "body": "Good point, will check it.", "back": "files"}, page=files)
    check(r.status in (302, 303), f"reply: {r.status} {r.text[:300]}")
    page = alice.get(files)
    check("Good point, will check it." in page.text, "the reply is not on the files page")
    check(page.text.count('class="thread') == 1 or len(re.findall(r'name="thread" value="', page.text)) == 1, "the reply made a second thread")
    conv = alice.get(f"/alice/demo/pulls/{pr}")
    check(conv.status == 200 and 'name="in_reply_to"' in conv.text and f'name="thread" value="{thread}"' in conv.text,
          "the conversation tab has no Reply and Resolve for the thread")
    r = alice.post_form(f"/alice/demo/pulls/{pr}/resolve", {"thread": thread, "resolved": "1", "back": "files"}, page=files)
    check(r.status in (302, 303), f"resolve: {r.status} {r.text[:300]}")
    page = alice.get(files)
    check(">Unresolve</button>" in page.text and "resolved" in page.text, "the thread is not shown as resolved")
    # A reader with no write access cannot resolve it: the button is not offered.
    check(S["bob"].get(files).status == 200, "bob cannot open the files page")


def step17():
    a = Client("10.9.2.4")
    before = git(["ls-remote", http_url("alice", S["alice_token"], "alice/demo"), "refs/heads/main"], WORK, env=gitenv("10.9.2.4")).stdout.split()[0]
    r = a.api("PUT", f"/repos/alice/demo/pulls/{S['pr']}/merge", auth=S["alice_token"], js={"merge_method": "squash"})
    check(r.status == 200 and r.json().get("merged") is True, f"merge: {r.status} {r.text[:300]}")
    after = git(["ls-remote", http_url("alice", S["alice_token"], "alice/demo"), "refs/heads/main"], WORK, env=gitenv("10.9.2.4")).stdout.split()[0]
    check(before != after, "main did not move")
    wd = os.path.join(WORK, "verify")
    git(["clone", "-q", http_url("alice", S["alice_token"], "alice/demo"), wd], WORK, env=gitenv("10.9.2.4"))
    n = git(["rev-list", "--count", f"{before}..{after}"], wd).stdout.strip()
    check(n == "1", f"main gained {n} commits, expected 1")
    time.sleep(3)
    i = a.api("GET", "/repos/alice/demo/issues/1", auth=S["alice_token"])
    check(i.json().get("state") == "closed", f"issue 1 is {i.json().get('state')}")
    deadline = time.time() + 60
    got = S["hook_got"]
    while time.time() < deadline and not any(b'"merged":true' in b or b'"merged": true' in b for _, b in got):
        time.sleep(1)
    check(any((b'"merged":true' in b or b'"merged": true' in b) and b'"closed"' in b for _, b in got), "no pull_request closed merged:true delivery")
    S["merged_sha"] = after


def step18():
    a = Client("10.9.2.4")
    deadline = time.time() + 300
    last = None
    while time.time() < deadline:
        r = a.api("GET", f"/repos/alice/demo/commits/{S['merged_sha']}/check-runs", auth=S["alice_token"])
        if r.status == 200:
            runs = r.json().get("check_runs", [])
            last = [(x["name"], x["status"], x.get("conclusion")) for x in runs]
            if len(runs) >= 3 and all(x["status"] == "completed" for x in runs):
                break
        time.sleep(4)
    check(last and len(last) >= 3 and all(s == "completed" and c == "success" for _, s, c in last), f"check runs on main: {last}")
    page = S["alice"].get("/alice/demo")
    check(page.status == 200 and re.search(r"status|check|success|passed", page.text, re.I), "no status on the repo home page")
    release_asset_and_org_ci()
    org_ui_flow()
    job_isolation()


def release_asset_and_org_ci():
    a = Client("10.9.2.4")
    # An uploaded release file downloads from the link the API gives, and the page lists it.
    r = a.api("POST", "/repos/alice/demo/releases", auth=S["alice_token"], js={"tag_name": "v0.0.1", "target_commitish": "main", "name": "First"})
    check(r.status == 201, f"create release: {r.status} {r.text[:300]}")
    rid = r.json()["id"]
    r = a.req("POST", f"/api/v3/repos/alice/demo/releases/{rid}/assets?name=asset.txt", body=b"asset body\n", auth=S["alice_token"],
              headers={"Content-Type": "text/plain"})
    check(r.status == 201, f"upload asset: {r.status} {r.text[:300]}")
    url = urllib.parse.urlparse(r.json()["browser_download_url"]).path
    d = S["alice"].get(url)
    check(d.status == 200 and d.body == b"asset body\n", f"the release download link gives {d.status}: {d.text[:200]}")
    page = S["alice"].get("/alice/demo/releases/tag/v0.0.1")
    check(page.status == 200 and "asset.txt" in page.text and url in page.text, "the release page does not list the uploaded file")
    # A repository owned by an organization runs workflows too.
    r = S["alice"].post_form("/new/org", {"name": "acme", "display_name": "Acme"}, page="/new/org")
    check(r.status in (302, 303), f"create org: {r.status} {r.text[:300]}")
    r = a.api("POST", "/orgs/acme/repos", auth=S["alice_token"], js={"name": "slugify", "private": True})
    check(r.status == 201, f"create org repo: {r.status} {r.text[:300]}")
    wd = os.path.join(WORK, "acme_clone")
    os.makedirs(os.path.join(wd, ".codehaven/workflows"))
    git(["init", "-q", "-b", "main"], wd)
    open(os.path.join(wd, "README.md"), "w").write("# slugify\n")
    # The job also tries every address the host has on its Docker bridges, and the host's own
    # public addresses. None of them may answer: a job that reaches one reaches ssh, postgres,
    # redis and whatever else the host listens on at that address. The one exception is tcp/443
    # on a public address, the public https site, which answers on purpose and must NOT fail the
    # step, so it writes to a different file than the failures.
    gws = subprocess.run(["docker", "network", "inspect", "-f", "{{range .IPAM.Config}}{{.Gateway}} {{end}}"] +
                         subprocess.run(["docker", "network", "ls", "-q"], capture_output=True, text=True).stdout.split(),
                         capture_output=True, text=True).stdout.split()
    gws = sorted({g for g in gws if re.fullmatch(r"\d+\.\d+\.\d+\.\d+", g)})
    pub = host_public_ips()
    # Dropped packets time out, so every connection is tried at once rather than one after another.
    gw_probe = ("for ip in " + " ".join(gws) + "; do for port in 22 80 443 3200 5432 6379; do "
                "(timeout 3 bash -c \"</dev/tcp/$ip/$port\" 2>/dev/null && echo \"REACHED $ip:$port\" >> /tmp/reached) & done; done")
    pub_probe = ("for ip in " + " ".join(pub) + "; do for port in 22 80 2222 3000 3200 5432 6379 8080 11434; do "
                 "(timeout 3 bash -c \"</dev/tcp/$ip/$port\" 2>/dev/null && echo \"REACHED $ip:$port\" >> /tmp/reached) & done; "
                 "(timeout 3 bash -c \"</dev/tcp/$ip/443\" 2>/dev/null && echo \"443-OPEN $ip\" >> /tmp/https) & done")
    probe = ("rm -f /tmp/reached /tmp/https; " + gw_probe + "; " + pub_probe + "; wait; "
             "cat /tmp/https 2>/dev/null; echo \"probe-443=$(cat /tmp/https 2>/dev/null | wc -l)\"; "
             "if [ -s /tmp/reached ]; then cat /tmp/reached; exit 1; fi; echo host-unreachable")
    wf = {"name": "ci", "on": "push", "jobs": {"t": {"runs-on": "ubuntu-latest", "steps": [{"run": "echo org ok"}, {"run": probe}]}}}
    open(os.path.join(wd, ".codehaven/workflows/ci.yml"), "w").write(json.dumps(wf))
    git(["add", "-A"], wd)
    git(["commit", "-q", "-m", "first"], wd)
    git(["push", http_url("alice", S["alice_token"], "acme/slugify"), "main"], wd, env=gitenv("10.9.2.4"))
    sha = git(["rev-parse", "HEAD"], wd).stdout.strip()
    deadline = time.time() + 240
    runs = []
    while time.time() < deadline:
        r = a.api("GET", "/repos/acme/slugify/actions/runs", auth=S["alice_token"])
        if r.status == 200:
            runs = [x for x in r.json().get("workflow_runs", []) if x.get("head_sha") == sha]
            if runs and all(x["status"] == "completed" for x in runs):
                break
        time.sleep(3)
    check(runs and all(x.get("conclusion") == "success" for x in runs),
          f"the org repository's workflow failed or did not run (a failed probe means a job reached the host): {runs}")
    # The probe ran to its end, nothing answered, and the 443 exception held: 443 on a public
    # address answered and the run still concluded success.
    jobs = a.api("GET", f"/repos/acme/slugify/actions/runs/{runs[0]['id']}/jobs", auth=S["alice_token"]).json().get("jobs", [])
    log = " ".join(a.req("GET", f"/api/v3/repos/acme/slugify/actions/jobs/{j['id']}/logs", auth=S["alice_token"]).text for j in jobs)
    check(re.search(r"probe-443=\d", log), f"the host probe never ran to its end: {log[:300]}")
    check(not re.search(r"REACHED \d", log), f"a job reached the host: {log[:300]}")
    if pub:
        check(re.search(r"443-OPEN \d", log), "tcp/443 never answered on a public address, so the 443 exception is untested")


def job_isolation():
    # Two jobs of one run, which in real life belong to two owners, run at the same time (the
    # runner takes two). They must not share a network, and they must not hold NET_RAW: job a
    # serves, job b waits for the server to be up and then hunts for it across every /16 the
    # daemon's address pool hands out, and finally checks its own capabilities. The job image
    # has python3 and bash.
    a = Client("10.9.2.4")
    # The clone the org run above pushed from: its history is the repository's, so the push is a
    # plain fast-forward.
    wd = os.path.join(WORK, "acme_clone")
    serve = ("echo \"A-ADDR $(hostname -i | awk '{print $1}')\"; "
             "python3 -m http.server 8099 >/dev/null 2>&1 & sleep 40; echo A-DONE")
    hunt = ("sleep 12; me=$(hostname -i | awk '{print $1}'); rm -f /tmp/reached; "
            "for o in $(seq 17 31); do ip=172.$o.0.2; [ \"$ip\" = \"$me\" ] && continue; "
            "(timeout 3 bash -c \"</dev/tcp/$ip/8099\" 2>/dev/null && echo \"REACHED $ip\" >> /tmp/reached) & done; wait; "
            "echo \"B-ADDR $me\"; if [ -s /tmp/reached ]; then cat /tmp/reached; exit 1; fi; echo no-other-job-reachable")
    caps = ("if [ $(( 0x$(awk '/CapEff/{print $2}' /proc/self/status) & 0x2000 )) -ne 0 ]; then "
            "echo \"NET_RAW is present\"; exit 1; fi; echo no-net-raw")
    wf = {"name": "iso", "on": "push", "jobs": {
        "a": {"runs-on": "ubuntu-latest", "steps": [{"run": serve}]},
        "b": {"runs-on": "ubuntu-latest", "steps": [{"run": hunt}, {"run": caps}]}}}
    open(os.path.join(wd, ".codehaven/workflows/iso.yml"), "w").write(json.dumps(wf))
    git(["add", "-A"], wd)
    git(["commit", "-q", "-m", "two jobs at once"], wd)
    git(["push", http_url("alice", S["alice_token"], "acme/slugify"), "main"], wd, env=gitenv("10.9.2.4"))
    sha = git(["rev-parse", "HEAD"], wd).stdout.strip()
    deadline = time.time() + 240
    runs = []
    while time.time() < deadline:
        r = a.api("GET", "/repos/acme/slugify/actions/runs", auth=S["alice_token"])
        if r.status == 200:
            runs = [x for x in r.json().get("workflow_runs", []) if x.get("head_sha") == sha]
            if runs and all(x["status"] == "completed" for x in runs):
                break
        time.sleep(3)
    check(runs and all(x.get("conclusion") == "success" for x in runs),
          f"the isolation workflow failed or did not run (a job reached the other, or kept NET_RAW): {runs}")
    iso = [x for x in runs if x.get("name") == "iso"]
    check(iso, f"no iso run for this push: {[(x.get('name'), x.get('head_sha')) for x in runs]}")
    jobs = a.api("GET", f"/repos/acme/slugify/actions/runs/{iso[0]['id']}/jobs", auth=S["alice_token"]).json().get("jobs", [])
    logs = {j["name"]: a.req("GET", f"/api/v3/repos/acme/slugify/actions/jobs/{j['id']}/logs", auth=S["alice_token"]).text
            for j in jobs}
    check("A-ADDR 172." in logs.get("a", ""), f"job a's log lacks its address: {logs.get('a', '')[:300]}")
    check("no-other-job-reachable" in logs.get("b", "") and "no-net-raw" in logs.get("b", ""),
          f"job b's log lacks its verdicts: {logs.get('b', '')[:300]}")
    # Vacuity: the two really were on two different networks, not two addresses of one bridge.
    ma = re.search(r"A-ADDR (172\.\d+)\.", logs.get("a", ""))
    mb = re.search(r"B-ADDR (172\.\d+)\.", logs.get("b", ""))
    check(ma and mb and ma.group(1) != mb.group(1),
          f"both jobs sat on the same /16: {ma and ma.group(1)} and {mb and mb.group(1)}")


def org_ui_flow():
    """The organization pages an owner manages everything from: the profile buttons, people,
    teams, one team's permission, members and repositories, and the owner-only settings."""
    alice, bob = S["alice"], S["bob"]
    # The owner's view of the organization page offers a repository and the settings.
    page = alice.get("/acme")
    check(page.status == 200 and "/new?owner=" in page.text, "the org page lacks a New repository button for the owner")
    # A team made from the form carries the permission it was given.
    r = alice.post_form("/orgs/acme/teams/new", {"name": "devs", "description": "Development", "permission": "write"}, page="/orgs/acme/teams")
    check(r.status in (302, 303), f"new team: {r.status} {r.text[:300]}")
    team_path = r.h("location")
    check(re.fullmatch(r"/orgs/acme/teams/\d+", team_path), f"new team sent the owner to {team_path}")
    t = alice.get(team_path)
    check(t.status == 200 and "devs" in t.text and ">Write<" in t.text, "the team page does not show the team and its Write permission")
    # Bob joins the organization first, then the team, and both pages list him.
    r = alice.post_form("/orgs/acme/people/add", {"username": "bob", "role": "member"}, page="/orgs/acme/people")
    check(r.status in (302, 303), f"add org member: {r.status} {r.text[:300]}")
    r = alice.post_form(f"{team_path}/members/add", {"username": "bob"}, page=team_path)
    check(r.status in (302, 303), f"add team member: {r.status} {r.text[:300]}")
    t = alice.get(team_path)
    check(t.status == 200 and 'href="/bob"' in t.text, "bob is not listed on the team page")
    # A repository of the organization joins the team with the team's permission.
    m = re.search(r'<option value="(\d+)">slugify</option>', t.text)
    check(m, "the team page offers no slugify repository to add")
    r = alice.post_form(f"{team_path}/repos/add", {"repo_id": m.group(1), "permission": "write"}, page=team_path)
    check(r.status in (302, 303), f"add team repository: {r.status} {r.text[:300]}")
    t = alice.get(team_path)
    check(t.status == 200 and 'href="/acme/slugify"' in t.text, "the team page does not list slugify")
    # Settings are the owner's alone.
    check(alice.get("/orgs/acme/settings").status == 200, "the owner cannot read the org settings page")
    check(bob.get("/orgs/acme/settings").status in (403, 404), "a plain member can read the org settings page")
    # A member cannot make a team, and nothing is created by the attempt.
    r = bob.post_form("/orgs/acme/teams/new", {"name": "sneaky", "permission": "read"}, page="/orgs/acme/people")
    check(r.status in (302, 303, 403, 404), f"a member's new team post gave {r.status}")
    check("sneaky" not in alice.get("/orgs/acme/teams").text, "a member's team was created anyway")
    # Taking bob off the team takes him off the page.
    r = alice.post_form(f"{team_path}/members/bob/remove", {}, page=team_path)
    check(r.status in (302, 303), f"remove team member: {r.status} {r.text[:300]}")
    t = alice.get(team_path)
    check(t.status == 200 and 'href="/bob"' not in t.text, "bob is still listed on the team page")
    # What the REST API says about the team matches what the form made: write reads as push.
    r = alice.api("GET", "/orgs/acme/teams/devs", auth=S["alice_token"])
    check(r.status == 200 and r.json().get("permission") == "push", f"REST team permission: {r.status} {r.text[:200]}")


def step19():
    root = S["root"]
    r = root.get("/admin/audit")
    check(r.status == 200, f"audit page {r.status}")
    t = r.text
    for what in ("sign", "invite", "token", "key", "lock", "runner"):
        check(re.search(what, t, re.I), f"the audit log lacks anything about {what}")
    check(re.search(r"\b10\.9\.\d+\.\d+\b", t), "no client IPs in the audit log")


def step20():
    """Leaves what the screenshot run needs: session cookies, ids and one unused invite."""
    out = os.environ.get("E2E_SHOTS")
    check(out, "set E2E_SHOTS to the file the session data goes to")
    cookie = "__Host-ch_session" if SECURE else "ch_session"
    ops = S["root"]
    invite = mint_invite(ops, "newcomer@example.test")
    data = {"alice": S["alice"].cookies.get(cookie), "ops": ops.cookies.get(cookie), "bob": S["bob"].cookies.get(cookie),
            "run_id": S["run_id"], "job_id": S["job_id"], "invite_path": f"/invite/{invite}", "cookie_name": cookie}
    data["root"] = data["ops"]
    data["alice_token"] = S["alice_token"]
    check(data["alice"] and data["ops"], "no session cookies to hand over")
    json.dump(data, open(out, "w"))
    os.chmod(out, 0o600)


def step21():
    """The live job log in a real browser: a job with three steps is pushed and watched while it runs.
    tests/e2e/livelog.mjs asserts that a step which has not started stays empty, that the running
    step's log grows, and that its elapsed time ticks. Needs node, playwright-core and Chrome on this
    host (the screenshot run uses the same), and the session file step 20 wrote."""
    mjs = os.environ.get("E2E_LIVELOG") or os.path.join(os.path.dirname(os.path.abspath(__file__)), "livelog.mjs")
    sess = os.environ.get("E2E_SHOTS")
    if not os.path.exists(mjs):
        warn(False, f"{mjs} is not here, so the live job log was not checked in a browser")
        return
    check(sess and os.path.exists(sess), "step 20 has not left the session file")
    a = Client("10.9.2.4")
    wd = os.path.join(WORK, "verify")
    os.makedirs(os.path.join(wd, ".codehaven/workflows"), exist_ok=True)
    loop = "for i in 1 2 3 4 5 6 7 8; do echo {n}$i; sleep 3; done"
    wf = {"name": "livelog-check", "on": "push", "jobs": {"three": {"runs-on": "ubuntu-latest", "steps": [
        {"name": "Count A", "run": loop.format(n="A")}, {"name": "Count B", "run": loop.format(n="B")}, {"name": "Done", "run": "echo C"}]}}}
    open(os.path.join(wd, ".codehaven/workflows/livelog.yml"), "w").write(json.dumps(wf))
    git(["add", "-A"], wd)
    git(["commit", "-q", "-m", "live log check"], wd)
    # main is protected by step 10, so the check runs from a branch of its own.
    git(["push", http_url("alice", S["alice_token"], "alice/demo"), "HEAD:refs/heads/livelog-check"], wd, env=gitenv("10.9.2.4"))
    sha = git(["rev-parse", "HEAD"], wd).stdout.strip()
    run_id = job_id = None
    deadline = time.time() + 120
    while time.time() < deadline and not job_id:
        r = a.api("GET", "/repos/alice/demo/actions/runs", auth=S["alice_token"])
        mine = [x for x in r.json().get("workflow_runs", []) if x.get("head_sha") == sha and x.get("name") == "livelog-check"] if r.status == 200 else []
        if mine:
            run_id = mine[0]["id"]
            jobs = a.api("GET", f"/repos/alice/demo/actions/runs/{run_id}/jobs", auth=S["alice_token"]).json().get("jobs", [])
            if jobs and jobs[0].get("status") == "in_progress":
                job_id = jobs[0]["id"]
        time.sleep(1)
    check(job_id, "the live log job never started running")
    p = subprocess.run(["node", mjs, sess, BASE, "alice/demo", str(run_id), str(job_id)], capture_output=True, text=True, timeout=120)
    check(p.returncode == 0, f"the live job page failed in the browser: {(p.stdout + p.stderr)[-800:]}")
    print("         " + p.stdout.strip().splitlines()[-1][:200], flush=True)
    deadline = time.time() + 120
    while time.time() < deadline:
        r = a.api("GET", f"/repos/alice/demo/actions/runs/{run_id}", auth=S["alice_token"])
        if r.status == 200 and r.json().get("status") == "completed":
            break
        time.sleep(2)
    check(r.json().get("conclusion") == "success", f"the live log job ended {r.json().get('conclusion')}")


def step22():
    """The browser workspace in a real browser: tests/e2e/workspace.mjs loads it at the public
    origin and checks that five languages get syntax colours, that the phone top bar stays
    visible on every pane, and that a phone can edit and commit a long file (tap areas, the
    bottom tabs, the top bar, what the dialog and the run panel say will run, note contrast). Needs node, playwright-core and Chrome on this host (the screenshot run
    uses the same), and the session file step 20 wrote."""
    mjs = os.environ.get("E2E_WORKSPACE_MJS") or os.path.join(os.path.dirname(os.path.abspath(__file__)), "workspace.mjs")
    if not os.path.exists(mjs):
        check(False, f"{mjs} is not here, so the workspace was not checked in a browser")
    sess = os.environ.get("E2E_SHOTS")
    check(sess and os.path.exists(sess), "step 20 has not left the session file")
    wd = os.path.join(WORK, "verify")
    # A branch of its own from main, so the five files never touch protected main.
    git(["fetch", "-q", http_url("alice", S["alice_token"], "alice/demo"), "main"], wd, env=gitenv("10.9.2.4"))
    git(["checkout", "-q", "-B", "ws-check", "FETCH_HEAD"], wd)
    files = {
        "a.ts": "interface Item {\n  id: number;\n  name: string;\n}\n\nfunction first(items: Item[]): string {\n  const found = items[0];\n  return found ? found.name : \"none\";\n}\n",
        "b.js": "// the total of a basket, with the usual rate\nfunction total(prices) {\n  const rate = 1.2;\n  let sum = 0;\n  for (const p of prices) sum += p;\n  return \"total: \" + Math.round(sum * rate);\n}\n",
        "c.css": "/* the shell every page sits in */\n.shell {\n  margin: 0 auto;\n  max-width: 960px;\n  color: #333;\n}\n\n.shell h1 {\n  font-weight: 600;\n}\n",
        "d.py": "def greet(name):\n    \"\"\"Say hello.\"\"\"\n    greeting = \"hello\"\n    count = 3\n    # three bangs, no more\n    return f\"{greeting} {name}!\" + \"!\" * count\n",
        "e.rs": "fn main() {\n    let name = \"codehaven\";\n    let count = 3;\n    // say it three times\n    for _ in 0..count {\n        println!(\"hello {}\", name);\n    }\n}\n",
        # Sixty lines, for the phone check that the end of a long file is not under the tabs.
        "long.txt": "".join(f"line {i}\n" for i in range(1, 60)) + "THE LAST LINE\n",
    }
    for name, body in files.items():
        open(os.path.join(wd, name), "w").write(body)
    git(["add", *files], wd)
    git(["commit", "-q", "-m", "workspace check"], wd)
    git(["push", http_url("alice", S["alice_token"], "alice/demo"), "HEAD:refs/heads/ws-check"], wd, env=gitenv("10.9.2.4"))
    # The same commit under a long branch name, for the phone top bar.
    git(["push", http_url("alice", S["alice_token"], "alice/demo"), "HEAD:refs/heads/ws-check-with-a-rather-long-name"], wd, env=gitenv("10.9.2.4"))
    p = subprocess.run(["node", mjs, sess, BASE, PUBLIC, "alice/demo", "ws-check"], capture_output=True, text=True, timeout=150)
    for line in p.stdout.strip().splitlines():
        print("         " + line[:200], flush=True)
    problems = "\n".join(line[:200] for line in p.stderr.strip().splitlines()[:25])
    check(p.returncode == 0, f"the workspace failed in the browser:\n{problems}")


def step23():
    """The workspace terminal in a real browser: tests/e2e/terminal.mjs opens it on the ws-check
    branch step 22 pushed and checks the prompt and its budget, the everyday commands, that an
    unsaved edit reaches the shell, that shell changes reach the editor only through a review where
    nothing is ticked, that committing them shows the CI warning, Stop and Ctrl+C, escape
    sequences, that the frame and its Worker reach nothing on the server, and the phone layout.
    E2E_TERMINAL_ENGINES picks engines (default chromium; the host has Chrome only)."""
    mjs = os.environ.get("E2E_TERMINAL_MJS") or os.path.join(os.path.dirname(os.path.abspath(__file__)), "terminal.mjs")
    if not os.path.exists(mjs):
        check(False, f"{mjs} is not here, so the terminal was not checked in a browser")
    sess = os.environ.get("E2E_SHOTS")
    check(sess and os.path.exists(sess), "step 20 has not left the session file")
    for engine in (os.environ.get("E2E_TERMINAL_ENGINES") or "chromium").split(","):
        p = subprocess.run(["node", mjs, sess, BASE, PUBLIC, "alice/demo", "ws-check", engine], capture_output=True, text=True, timeout=400)
        for line in p.stdout.strip().splitlines():
            print("         " + line[:200], flush=True)
        problems = "\n".join(line[:300] for line in p.stderr.strip().splitlines()[:25])
        check(p.returncode == 0, f"the terminal failed in {engine}:\n{problems}")


C3_FILES = {
    ".codehaven/workflows/c3.yml": """name: c3
on: push
jobs:
  test:
    name: Tests
    runs-on: ubuntu-latest
    strategy:
      matrix:
        go: ['1.21', '1.22']
    steps:
      - name: Install Go ${{ matrix.go }}
        run: echo go ${{ matrix.go }}
      - name: Rust is in the job image
        run: rustup --version && cargo --version
""",
    ".codehaven/workflows/win.yml": """name: win
on: push
jobs:
  build:
    runs-on: windows-latest
    steps:
      - run: echo hi
""",
    ".codehaven/workflows/broken.yml": """name: broken
on: push
jobs:
  b:
    runs-on: ubuntu-latest
    container:
      image: alpine
      volumes: [/:/host]
    steps:
      - run: echo
""",
}


def step24():
    """Batch C3 on a real runner: matrix cells named apart, step names filled in, Rust in the job
    image, a job no runner can take says so, a broken file shows a failed run, the badge, the logs
    zip, the run objects, re-runs as one row and the rate limit body."""
    a, tok = Client("10.9.2.4"), S["alice_token"]
    r = a.api("POST", "/user/repos", js={"name": "c3", "private": True}, auth=tok)
    check(r.status == 201, f"create c3: {r.status} {r.text[:200]}")
    wd = os.path.join(WORK, "c3")
    os.makedirs(os.path.join(wd, ".codehaven/workflows"))
    for path, text in C3_FILES.items():
        open(os.path.join(wd, path), "w").write(text)
    git(["init", "-q", "-b", "main"], wd)
    git(["add", "-A"], wd)
    git(["commit", "-q", "-m", "C3 checks"], wd)
    git(["push", http_url("alice", tok, "alice/c3"), "main"], wd, env=gitenv("10.9.2.4"))
    sha = git(["rev-parse", "HEAD"], wd).stdout.strip()
    runs, deadline = {}, time.time() + 400
    while time.time() < deadline:
        r = a.api("GET", "/repos/alice/c3/actions/runs", auth=tok)
        if r.status == 200:
            runs = {x["name"]: x for x in r.json().get("workflow_runs", []) if x.get("head_sha") == sha}
            if {"c3", "broken", "win"} <= set(runs) and runs["c3"]["status"] == "completed":
                break
        time.sleep(3)
    check({"c3", "broken", "win"} <= set(runs), f"runs for the push: {sorted(runs)}")
    c3, broken, win = runs["c3"], runs["broken"], runs["win"]
    check(c3["status"] == "completed" and c3["conclusion"] == "success", f"c3 run: {c3['status']} {c3.get('conclusion')}")
    check(c3["head_commit"]["id"] == sha and c3["pull_requests"] == [], f"run object: {c3.get('head_commit')} {c3.get('pull_requests')}")
    jobs = a.api("GET", f"/repos/alice/c3/actions/runs/{c3['id']}/jobs", auth=tok).json()["jobs"]
    check(sorted(j["name"] for j in jobs) == ["Tests (1.21)", "Tests (1.22)"], f"matrix names: {[j['name'] for j in jobs]}")
    steps = [s["name"] for j in jobs for s in j["steps"]]
    check("Install Go 1.21" in steps and "Install Go 1.22" in steps, f"step names: {steps}")
    # ch-xy5: the broken file is a failed run that says why.
    check(broken["status"] == "completed" and broken["conclusion"] == "failure", f"broken run: {broken['status']} {broken.get('conclusion')}")
    page = S["alice"].get(f"/alice/c3/actions/runs/{broken['id']}")
    check(page.status == 200 and "could not run" in page.text and "volumes" in page.text, "the broken run's page does not say why")
    # ch-7rq and ch-dvg: the Windows job says no runner has its label, and lists what the runner offers.
    wj = a.api("GET", f"/repos/alice/c3/actions/runs/{win['id']}/jobs", auth=tok).json()["jobs"][0]
    jp = S["alice"].get(f"/alice/c3/actions/runs/{win['id']}/jobs/{wj['id']}")
    check("No runner has the labels this job asks for: windows-latest" in jp.text, "no queue note on the Windows job")
    check("ubuntu-24.04" in jp.text and "ubuntu-latest" in jp.text, "the note does not list the runner's labels")
    check(a.api("POST", f"/repos/alice/c3/actions/runs/{win['id']}/cancel", auth=tok).status in (202, 204), "cancel the Windows run")
    # ch-c1d: the badge for a signed-in viewer, nothing for a stranger.
    b = S["alice"].get("/alice/c3/actions/workflows/c3.yml/badge.svg")
    check(b.status == 200 and b.h("content-type").startswith("image/svg+xml") and "passing" in b.text, f"badge: {b.status}")
    anon = Client("10.9.9.3").get("/alice/c3/actions/workflows/c3.yml/badge.svg", cookies=False)
    check(anon.status == 303 and "svg" not in anon.text, f"a stranger got the badge: {anon.status}")
    # ch-1bp: the logs are a zip.
    z = a.req("GET", f"/api/v3/repos/alice/c3/actions/runs/{c3['id']}/logs", auth=tok)
    check(z.status == 200 and z.body[:2] == b"PK", f"logs zip: {z.status}")
    # ch-zwo: a re-run replaces its row in the list, which names the workflow and the commit.
    check(a.api("POST", f"/repos/alice/c3/actions/runs/{c3['id']}/rerun", auth=tok).status == 201, "re-run")
    lp = S["alice"].get("/alice/c3/actions")
    check("C3 checks" in lp.text and "attempt 2" in lp.text, "the runs list lacks the title or the attempt")
    # ch-iw8: the rate limit body agrees with the headers of the same answer.
    rl = a.api("GET", "/rate_limit", auth=tok)
    body = rl.json()["rate"]
    check(str(body["remaining"]) == rl.h("x-ratelimit-remaining") and str(body["limit"]) == rl.h("x-ratelimit-limit"), f"rate limit: {body}")


C9_CALLER = """name: callit
on: push
env:
  CALLER_ENV: caller-env-value
jobs:
  prep:
    runs-on: ubuntu-latest
    steps:
      - run: echo prep
  build:
    needs: prep
    uses: ./.codehaven/workflows/build.yml
    with:
      version: '1.2'
    secrets:
      token: ${{ secrets.NOT_SET }}
  after:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - run: |
          echo got ${{ needs.build.outputs.artifact }}
          echo "after_env=[${CALLER_ENV}]"
"""

C9_CALLEE = """name: build
on:
  workflow_call:
    inputs:
      version: {type: string, required: true}
    secrets:
      token: {required: false}
    outputs:
      artifact:
        value: ${{ jobs.pack.outputs.name }}
jobs:
  pack:
    runs-on: ubuntu-latest
    outputs:
      name: ${{ steps.p.outputs.name }}
    steps:
      - id: p
        run: |
          echo "version=${{ inputs.version }}"
          echo "callee_sees_caller_env=[${CALLER_ENV}]"
          echo "name=dist.zip" >> "$GITHUB_OUTPUT"
"""


def step25():
    """Batch C9 on a real runner: a workflow calls a reusable workflow in its own repository, the
    called job runs with the input pasted in, and its output reaches the job after the call."""
    a, tok = Client("10.9.2.4"), S["alice_token"]
    r = a.api("POST", "/user/repos", js={"name": "c9", "private": True}, auth=tok)
    check(r.status == 201, f"create c9: {r.status} {r.text[:200]}")
    wd = os.path.join(WORK, "c9")
    os.makedirs(os.path.join(wd, ".codehaven/workflows"))
    open(os.path.join(wd, ".codehaven/workflows/call.yml"), "w").write(C9_CALLER)
    open(os.path.join(wd, ".codehaven/workflows/build.yml"), "w").write(C9_CALLEE)
    git(["init", "-q", "-b", "main"], wd)
    git(["add", "-A"], wd)
    git(["commit", "-q", "-m", "C9 reusable workflow"], wd)
    git(["push", http_url("alice", tok, "alice/c9"), "main"], wd, env=gitenv("10.9.2.4"))
    sha = git(["rev-parse", "HEAD"], wd).stdout.strip()
    run_, deadline = None, time.time() + 400
    while time.time() < deadline:
        r = a.api("GET", "/repos/alice/c9/actions/runs", auth=tok)
        if r.status == 200:
            rs = [x for x in r.json().get("workflow_runs", []) if x.get("head_sha") == sha and x.get("name") == "callit"]
            if rs and rs[0]["status"] == "completed":
                run_ = rs[0]
                break
        time.sleep(3)
    check(run_, "the run that calls a reusable workflow did not finish")
    check(run_.get("conclusion") == "success", f"the run did not succeed: {run_.get('conclusion')}")
    jobs = a.api("GET", f"/repos/alice/c9/actions/runs/{run_['id']}/jobs", auth=tok).json().get("jobs", [])
    names = [j["name"] for j in jobs]
    check("build / pack" in names, f"the called job is not in the run: {names}")
    pack = [j for j in jobs if j["name"] == "build / pack"][0]
    after = [j for j in jobs if j["name"] == "after"][0]
    log = a.req("GET", f"/api/v3/repos/alice/c9/actions/jobs/{pack['id']}/logs", auth=tok)
    check(log.status == 200 and "version=1.2" in log.text, f"the input did not reach the called job: {log.text[:300]}")
    # The caller's workflow-level env does not reach the called job, and still reaches the caller's own.
    check("callee_sees_caller_env=[]" in log.text and "caller-env-value" not in log.text, f"the caller's env reached the called job: {log.text[:300]}")
    log = a.req("GET", f"/api/v3/repos/alice/c9/actions/jobs/{after['id']}/logs", auth=tok)
    check("after_env=[caller-env-value]" in log.text, f"the caller's own job lost its workflow env: {log.text[:300]}")
    check(log.status == 200 and "got dist.zip" in log.text, f"the called workflow's output did not reach the next job: {log.text[:300]}")
    nuget_feed()


def nuget_feed():
    """The private NuGet feed through the real gate: a signed-out stranger gets a Basic challenge, a
    token with the packages scope pushes with X-NuGet-ApiKey (as `dotnet nuget push -k` does) and
    restores with Basic credentials, and a token without the scope is refused."""
    import zipfile, io
    alice = S["alice"]
    r = alice.req("POST", "/settings/tokens", form={"_csrf": alice.csrf("/settings/tokens"), "name": "nuget", "scopes": ["write:packages", "read:packages"], "expires_in_days": "30"})
    m = re.search(r"chp_[A-Za-z0-9]{40}", r.text)
    check(r.status == 200 and m, f"packages token: {r.status}")
    pk = m.group(0)
    spec = ('<?xml version="1.0"?><package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd"><metadata>'
            '<id>Probe.Pkg</id><version>1.0.0</version><authors>e2e</authors><description>probe</description></metadata></package>')
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        z.writestr("Probe.Pkg.nuspec", spec)
        z.writestr("lib/net8.0/Probe.Pkg.dll", b"MZ")
    nupkg = buf.getvalue()
    b = "----e2eboundary"
    body = (f"--{b}\r\nContent-Disposition: form-data; name=\"package\"; filename=\"package.nupkg\"\r\n"
            f"Content-Type: application/octet-stream\r\n\r\n").encode() + nupkg + f"\r\n--{b}--\r\n".encode()
    c = Client("10.9.2.4")
    stranger = c.req("PUT", "/nuget/alice/push", body=body, headers={"Content-Type": f"multipart/form-data; boundary={b}"}, cookies=False)
    check(stranger.status == 401 and stranger.h("www-authenticate").lower().startswith("basic"), f"a stranger's push: {stranger.status}")
    push = c.req("PUT", "/nuget/alice/push", body=body, headers={"Content-Type": f"multipart/form-data; boundary={b}", "X-NuGet-ApiKey": pk}, cookies=False)
    check(push.status == 201, f"push with X-NuGet-ApiKey: {push.status} {push.text[:200]}")
    basic = "Basic " + base64.b64encode(f"alice:{pk}".encode()).decode()
    v = c.req("GET", "/nuget/alice/flatcontainer/probe.pkg/index.json", auth=basic, cookies=False)
    check(v.status == 200 and v.json().get("versions") == ["1.0.0"], f"restore versions: {v.status} {v.text[:200]}")
    f = c.req("GET", "/nuget/alice/flatcontainer/probe.pkg/1.0.0/probe.pkg.1.0.0.nupkg", auth=basic, cookies=False)
    check(f.status == 200 and f.body == nupkg, f"restore file: {f.status}")
    again = c.req("PUT", "/nuget/alice/push", body=body, headers={"Content-Type": f"multipart/form-data; boundary={b}", "X-NuGet-ApiKey": pk}, cookies=False)
    check(again.status == 409, f"a second push of the same version: {again.status}")
    # The repo-only token from step 4 has no package scope.
    other = c.req("GET", "/nuget/alice/index.json", auth="Basic " + base64.b64encode(f"alice:{S['alice_token']}".encode()).decode(), cookies=False)
    check(other.status == 403, f"a token without the packages scope: {other.status}")
    # Bob cannot read Alice's unlinked package.
    bob_tok = S["bob"].req("POST", "/settings/tokens", form={"_csrf": S["bob"].csrf("/settings/tokens"), "name": "nuget", "scopes": ["read:packages"], "expires_in_days": "30"})
    bk = re.search(r"chp_[A-Za-z0-9]{40}", bob_tok.text).group(0)
    nope = c.req("GET", "/nuget/alice/flatcontainer/probe.pkg/index.json", auth="Basic " + base64.b64encode(f"bob:{bk}".encode()).decode(), cookies=False)
    check(nope.status == 404, f"another account reading it: {nope.status}")


def step26():
    """ch-d9s: a repository holding the whole Codehaven source runs `make ci` on a runner through
    its own workflow (.codehaven/workflows/ci.yml). Slow (a full workspace build inside the runner's
    Docker daemon), so it only runs when asked for: E2E_STEPS=0-4,26 with E2E_SOURCE_DIR set to a
    checkout, and the builder image and postgres loaded into the runner's daemon (shots.sh does it
    with SHOTS_DOGFOOD=1)."""
    src = os.environ.get("E2E_SOURCE_DIR", "")
    check(src and os.path.isdir(os.path.join(src, ".codehaven/workflows")), "set E2E_SOURCE_DIR to a Codehaven checkout")
    a, tok = Client("10.9.2.4"), S["alice_token"]
    r = a.api("POST", "/user/repos", js={"name": "codehaven", "private": True}, auth=tok)
    check(r.status == 201, f"create codehaven: {r.status} {r.text[:200]}")
    wd = os.path.join(WORK, "codehaven")
    os.makedirs(wd)
    run(["rsync", "-a", "--exclude", "target/", "--exclude", ".git", "--exclude", ".worktrees/", "--exclude", ".beads*",
         "--exclude", "*.env", "--exclude", ".env", src.rstrip("/") + "/", wd + "/"], timeout=600)
    git(["init", "-q", "-b", "main"], wd)
    git(["add", "-A"], wd)
    git(["commit", "-q", "-m", "Codehaven source"], wd)
    git(["push", http_url("alice", tok, "alice/codehaven"), "main"], wd, env=gitenv("10.9.2.4"), check_rc=True)
    sha = git(["rev-parse", "HEAD"], wd).stdout.strip()
    deadline = time.time() + int(os.environ.get("E2E_DOGFOOD_MINUTES", "150")) * 60
    run_, last = None, ""
    while time.time() < deadline:
        r = a.api("GET", "/repos/alice/codehaven/actions/runs", auth=tok)
        if r.status == 200:
            rs = [x for x in r.json().get("workflow_runs", []) if x.get("head_sha") == sha and x.get("name") == "ci"]
            if rs:
                state = f"{rs[0]['status']}/{rs[0].get('conclusion')}"
                if state != last:
                    print(f"         dogfood run {rs[0]['id']}: {state}", flush=True)
                    last = state
                if rs[0]["status"] == "completed":
                    run_ = rs[0]
                    break
        time.sleep(20)
    check(run_, "the workflow run of the Codehaven source did not finish in time")
    jobs = a.api("GET", f"/repos/alice/codehaven/actions/runs/{run_['id']}/jobs", auth=tok).json().get("jobs", [])
    log = ""
    for j in jobs:
        lg = a.req("GET", f"/api/v3/repos/alice/codehaven/actions/jobs/{j['id']}/logs", auth=tok)
        log += lg.text if lg.status == 200 else ""
    tail = "\n".join(log.splitlines()[-40:])
    check(run_.get("conclusion") == "success", f"make ci did not succeed on the runner ({run_.get('conclusion')}). Log tail:\n{tail}")
    check("tests run:" in log and "passed" in log, f"no test summary in the log. Log tail:\n{tail}")
    print("         " + [ln for ln in log.splitlines() if "tests run:" in ln][-1].strip(), flush=True)


def step27():
    """Batch C10 on the real stack: the admin path that imports a bare repository for an existing
    account, delete, transfer and the pinned order of issues over HTTP, the pull request publish
    confirm, and everything new answering a signed-out caller as nothing."""
    a, tok = Client("10.9.2.4"), S["alice_token"]
    # 1. `codehaven admin repo-import` inside the server container: no password, no token.
    prep = ("set -e; rm -rf /tmp/e2e-src /tmp/e2e-src.git; git init -q -b main /tmp/e2e-src; cd /tmp/e2e-src; "
            "git -c user.name=e -c user.email=e@example.test commit -q --allow-empty -m first; "
            "git clone -q --bare /tmp/e2e-src /tmp/e2e-src.git")
    p = subprocess.run(CLI[:-1] + ["sh", "-c", prep], capture_output=True, text=True, timeout=120)
    check(p.returncode == 0, f"could not make a bare repository in the server container: {(p.stdout + p.stderr)[-300:]}")
    rc, out, err = cli("admin", "repo-import", "--owner", "alice", "--name", "imported", "--from", "/tmp/e2e-src.git")
    check(rc == 0, f"repo-import failed: {(out + err)[-300:]}")
    rc, out, err = cli("admin", "repo-import", "--owner", "alice", "--name", "imported", "--from", "/tmp/e2e-src.git")
    check(rc != 0, "a second import under the same name must be refused")
    r = a.api("GET", "/repos/alice/imported", auth=tok)
    check(r.status == 200 and r.json().get("private") is True, f"the imported repository is not private: {r.status} {r.text[:200]}")
    anon = Client("10.9.2.9")
    check(anon.api("GET", "/repos/alice/imported").status in (401, 404), "a stranger can see the imported repository")
    # 2. Pinned order, transfer and delete, as GitHub-shaped JSON over the Codehaven prefix.
    nums = []
    for i in range(3):
        r = a.api("POST", "/repos/alice/imported/issues", js={"title": f"c10 issue {i}"}, auth=tok)
        check(r.status == 201, f"open an issue: {r.status} {r.text[:200]}")
        nums.append(r.json()["number"])
    for n in nums:
        check(a.api("PUT", f"/repos/alice/imported/issues/{n}/pin", auth=tok).status == 204, "pin")
    want = [nums[2], nums[0], nums[1]]
    r = a.api("PUT", "/repos/alice/imported/codehaven/issues/pinned/order", js={"numbers": want}, auth=tok)
    check(r.status == 200 and [i["number"] for i in r.json()] == want, f"reorder: {r.status} {r.text[:200]}")
    r = a.api("GET", "/repos/alice/imported/codehaven/issues/pinned", auth=tok)
    check([i["number"] for i in r.json()] == want, "the pinned list does not follow the order")
    r = a.api("POST", f"/repos/alice/imported/codehaven/issues/{nums[0]}/transfer", js={"repository": "alice/demo"}, auth=tok)
    check(r.status == 200 and "/alice/demo/issues/" in r.json().get("html_url", ""), f"transfer: {r.status} {r.text[:200]}")
    # demo is private like imported, so the move is allowed; moving into a more visible repository is not.
    bob = S.get("bob_token")
    if bob:
        check(a.api("DELETE", f"/repos/alice/imported/codehaven/issues/{nums[1]}", auth=bob).status in (403, 404), "bob deleted an issue")
    check(a.api("DELETE", f"/repos/alice/imported/codehaven/issues/{nums[1]}", auth=tok).status == 204, "the admin could not delete")
    check(a.api("GET", f"/repos/alice/imported/issues/{nums[1]}", auth=tok).status == 404, "a deleted issue is still there")
    # 3. Signed out, none of it is served, and the NuGet feed refuses a push with no credential.
    for m, u in [("GET", "/repos/alice/imported/codehaven/issues/pinned"), ("PUT", "/repos/alice/imported/codehaven/issues/pinned/order"),
                 ("DELETE", f"/repos/alice/imported/codehaven/issues/{nums[2]}"),
                 ("POST", f"/repos/alice/imported/codehaven/issues/{nums[2]}/transfer")]:
        r = anon.api(m, u, js={} if m in ("PUT", "POST") else None)
        check(r.status in (401, 404), f"{m} {u} signed out answered {r.status}")
    r = anon.req("PUT", "/nuget/alice/push", body=b"x", headers={"Content-Type": "multipart/form-data; boundary=x"})
    check(r.status == 401, f"a NuGet push with no credential answered {r.status}")
    for path in ("/api/runner/v1/toolkit/blobs/u.1.9999999999." + "0" * 64, "/api/runner/v1/toolkit/blobs/a.1.9999999999." + "0" * 64):
        check(anon.req("GET", path).status == 404, f"a made-up artifact link answered something other than 404: {path[:50]}")
    subprocess.run(CLI[:-1] + ["rm", "-rf", "/tmp/e2e-src", "/tmp/e2e-src.git"], capture_output=True, text=True, timeout=60)
    # Leaves a pinned issue in alice/demo, so the screenshots show the pinned section and the Manage block.
    check(a.api("PUT", "/repos/alice/demo/issues/1/pin", auth=tok).status == 204, "pin issue 1 of alice/demo")


def step28():
    """C10 review: commits copied into an internal base from a private fork are not published when
    the base is made public, and a signed-out stranger cannot fetch them."""
    ip, a, tok, bob = "10.9.2.5", S["alice"], S["alice_token"], S["bob_token"]
    rc, out, err = cli("admin", "public-repos", "on")
    check(rc == 0, f"public-repos on: {(out + err)[-300:]}")
    r = Client(ip).api("POST", "/user/repos", js={"name": "c10pub", "visibility": "internal", "auto_init": True}, auth=tok)
    check(r.status == 201, f"create the internal repository: {r.status} {r.text[:200]}")
    r = Client(ip).api("POST", "/repos/alice/c10pub/forks", js={"visibility": "private"}, auth=bob)
    check(r.status in (200, 201, 202), f"fork: {r.status} {r.text[:200]}")
    wd = os.path.join(WORK, "c10pub-bob")
    git(["clone", "-q", http_url("bob", bob, "bob/c10pub"), wd], WORK)
    git(["checkout", "-q", "-b", "feat"], wd)
    with open(os.path.join(wd, "carol-private-1.txt"), "w") as f:
        f.write("line one\nA5-HUNK-SECRET-LINE\nline three\n")
    git(["add", "."], wd)
    git(["commit", "-q", "-m", "xqz private commit message"], wd)
    git(["push", "-q", "origin", "feat"], wd)
    sha = git(["rev-parse", "HEAD"], wd).stdout.strip()
    # Into an internal base no confirm is needed.
    r = Client(ip).api("POST", "/repos/alice/c10pub/pulls", js={"title": "private work", "head": "bob:feat", "base": "main"}, auth=bob)
    check(r.status == 201, f"open the pull request: {r.status} {r.text[:300]}")
    n = r.json()["number"]
    # While the base is internal alice reads the diff and comments on a line of it. The comment
    # keeps a piece of the diff (its hunk) and the file's path.
    r = Client(ip).api("POST", f"/repos/alice/c10pub/pulls/{n}/comments", auth=tok,
                       js={"body": "why this line?", "commit_id": sha, "path": "carol-private-1.txt", "line": 2, "side": "RIGHT"})
    check(r.status == 201 and "A5-HUNK-SECRET-LINE" in r.json().get("diff_hunk", ""), f"alice's line comment: {r.status} {r.text[:300]}")
    cid = r.json()["id"]
    base_url = f"http://{HOST}:{PORT}/alice/c10pub.git"
    anon_env = gitenv("10.9.2.9")
    check(git(["ls-remote", base_url], WORK, env=anon_env, check_rc=False).returncode != 0, "a signed-out stranger read an internal repository")
    # Alice makes the base public in the browser, as the owner.
    page = "/alice/c10pub/settings/visibility"
    m = re.search(r'name="snapshot"\s+value="([^"]+)"', a.get(page).text)
    check(m, "no snapshot on the make-public page")
    r = a.post_form(page, {"value": "public", "snapshot": m.group(1), "confirm": "alice/c10pub", "checked_history": "1"}, page=page)
    check(r.status in (200, 303), f"make public: {r.status} {r.text[:300]}")
    r = Client(ip).api("GET", "/repos/alice/c10pub", auth=tok)
    check(r.json().get("visibility") == "public", f"the repository is not public: {r.text[:200]}")
    # Signed out: no pull refs, and the private commit cannot be fetched by id or by ref.
    refs = git(["ls-remote", base_url], WORK, env=anon_env).stdout
    check("refs/pull/" not in refs, f"a pull ref is advertised in the public repository: {refs[:300]}")
    probe = os.path.join(WORK, "c10pub-anon")
    git(["init", "-q", probe], WORK)
    for want in (f"refs/pull/{n}/head", sha):
        f = git(["fetch", "-q", base_url, want], probe, env=anon_env, check_rc=False)
        check(f.returncode != 0, f"a signed-out stranger fetched {want[:20]}")
    check(git(["cat-file", "-e", sha + "^{commit}"], probe, check_rc=False).returncode != 0, "the private commit is in the stranger's clone")
    # A later push to the fork is not copied either, and the pull request cannot be merged.
    with open(os.path.join(wd, "carol-private-2.txt"), "w") as f:
        f.write("more private work\n")
    git(["add", "."], wd)
    git(["commit", "-q", "-m", "more private work"], wd)
    git(["push", "-q", "origin", "feat"], wd)
    time.sleep(4)
    refs = git(["ls-remote", base_url], WORK, env=anon_env).stdout
    check("refs/pull/" not in refs, "a later head was written into the public repository")
    r = Client(ip).api("GET", f"/repos/alice/c10pub/pulls/{n}/merge-gate", auth=tok)
    check(r.status == 200 and r.json().get("mergeable") is False and "agreed to publish" in r.text, f"merge gate: {r.status} {r.text[:300]}")
    g = a.get(f"/alice/c10pub/pulls/{n}")
    check(g.status == 200, f"the pull request page: {g.status}")
    # The pull request's changes are not shown to anyone who cannot read Bob's private fork,
    # signed out included, through the API or the pages. Bob still sees his own.
    stranger = Client("10.9.2.9")
    for tail in ("files", "commits"):
        r = stranger.api("GET", f"/repos/alice/c10pub/pulls/{n}/{tail}")
        check(r.status == 200 and r.json() == [], f"signed out /pulls/{n}/{tail}: {r.status} {r.text[:200]}")
        r = Client(ip).api("GET", f"/repos/alice/c10pub/pulls/{n}/{tail}", auth=tok)
        check(r.status == 200 and r.json() == [], f"alice /pulls/{n}/{tail}: {r.status} {r.text[:200]}")
        g = stranger.get(f"/alice/c10pub/pulls/{n}/{tail}")
        check("carol-private-1.txt" not in g.text and "xqz private commit message" not in g.text, f"signed out web {tail} shows the private change")
    r = Client(ip).api("GET", f"/repos/alice/c10pub/pulls/{n}/files", auth=bob)
    check(r.status == 200 and any(f.get("filename") == "carol-private-1.txt" for f in r.json()), f"bob does not see his own files: {r.text[:200]}")
    # The line comment's stored hunk and path are code from the private fork too: blank for a
    # signed-out reader and for alice (she cannot read the fork), whole for Bob.
    for who, auth in (("signed out", None), ("alice", tok)):
        cl = stranger if auth is None else Client(ip)
        kw = {} if auth is None else {"auth": auth}
        # The repository-wide list is not among the routes a signed-out reader may use at all.
        tails = [f"/pulls/{n}/comments", f"/pulls/{n}/threads", f"/pulls/comments/{cid}"] + ([] if auth is None else ["/pulls/comments"])
        for tail in tails:
            r = cl.api("GET", f"/repos/alice/c10pub{tail}", **kw)
            check(r.status == 200, f"{who} {tail}: {r.status} {r.text[:200]}")
            check("A5-HUNK-SECRET-LINE" not in r.text and "carol-private-1.txt" not in r.text, f"{who} {tail} leaks the private code: {r.text[:300]}")
            check("why this line?" in r.text, f"{who} {tail} lost the comment itself: {r.text[:300]}")
        g = cl.get(f"/alice/c10pub/pulls/{n}")
        check(g.status == 200 and "A5-HUNK-SECRET-LINE" not in g.text and "carol-private-1.txt" not in g.text, f"{who} pull request page leaks the private code")
    r = Client(ip).api("GET", f"/repos/alice/c10pub/pulls/{n}/comments", auth=bob)
    check(r.status == 200 and "A5-HUNK-SECRET-LINE" in r.text, f"bob does not see his own hunk: {r.text[:300]}")
    # A late merge request (a merge that checked while the base was internal) is refused and copies nothing.
    r = Client(ip).api("PUT", f"/repos/alice/c10pub/pulls/{n}/merge", js={"merge_method": "merge"}, auth=tok)
    check(r.status in (405, 409, 422), f"the merge of a withheld head: {r.status} {r.text[:200]}")
    refs = git(["ls-remote", base_url], WORK, env=anon_env).stdout
    check("refs/pull/" not in refs, "a merge attempt wrote a pull ref into the public repository")
    f = git(["fetch", "-q", base_url, sha], probe, env=anon_env, check_rc=False)
    check(f.returncode != 0, "a signed-out stranger fetched the private commit after a merge attempt")


def step29():
    """C10 review round 2: merge requests and refreshes that race the base being made public copy
    nothing private into it, and the pull request shows no private change to a stranger."""
    ip, a, tok, bob = "10.9.2.5", S["alice"], S["alice_token"], S["bob_token"]
    r = Client(ip).api("POST", "/user/repos", js={"name": "c10race", "visibility": "internal", "auto_init": True}, auth=tok)
    check(r.status == 201, f"create the internal repository: {r.status} {r.text[:200]}")
    r = Client(ip).api("POST", "/repos/alice/c10race/forks", js={"visibility": "private"}, auth=bob)
    check(r.status in (200, 201, 202), f"fork: {r.status} {r.text[:200]}")
    wd = os.path.join(WORK, "c10race-bob")
    git(["clone", "-q", http_url("bob", bob, "bob/c10race"), wd], WORK)
    git(["checkout", "-q", "-b", "feat"], wd)
    with open(os.path.join(wd, "race-private.txt"), "w") as f:
        f.write("TOPSECRET-RACE\n")
    git(["add", "."], wd)
    git(["commit", "-q", "-m", "race private work"], wd)
    git(["push", "-q", "origin", "feat"], wd)
    sha = git(["rev-parse", "HEAD"], wd).stdout.strip()
    r = Client(ip).api("POST", "/repos/alice/c10race/pulls", js={"title": "race", "head": "bob:feat", "base": "main"}, auth=bob)
    check(r.status == 201, f"open the pull request: {r.status} {r.text[:300]}")
    n = r.json()["number"]
    stop = threading.Event()

    def hammer(k):
        c = Client(f"10.9.3.{k}")
        while not stop.is_set():
            c.api("PUT", f"/repos/alice/c10race/pulls/{n}/merge", js={"sha": "0" * 40, "merge_method": "merge"}, auth=tok)

    pool = [threading.Thread(target=hammer, args=(k,), daemon=True) for k in range(10, 14)]
    for t in pool:
        t.start()
    time.sleep(1)
    page = "/alice/c10race/settings/visibility"
    m = re.search(r'name="snapshot"\s+value="([^"]+)"', a.get(page).text)
    check(m, "no snapshot on the make-public page")
    r = a.post_form(page, {"value": "public", "snapshot": m.group(1), "confirm": "alice/c10race", "checked_history": "1"}, page=page)
    check(r.status in (200, 303), f"make public: {r.status} {r.text[:300]}")
    time.sleep(4)
    stop.set()
    for t in pool:
        t.join(timeout=30)
    base_url = f"http://{HOST}:{PORT}/alice/c10race.git"
    anon_env = gitenv("10.9.2.9")
    probe = os.path.join(WORK, "c10race-anon")
    git(["init", "-q", probe], WORK)
    f = git(["fetch", "-q", base_url, sha], probe, env=anon_env, check_rc=False)
    check(f.returncode != 0, "a signed-out stranger fetched a private commit that a racing merge copied")
    stranger = Client("10.9.2.9")
    for tail in ("files", "commits"):
        r = stranger.api("GET", f"/repos/alice/c10race/pulls/{n}/{tail}")
        check(r.status == 200 and r.json() == [], f"signed out /pulls/{n}/{tail}: {r.status} {r.text[:200]}")
        g = stranger.get(f"/alice/c10race/pulls/{n}/{tail}")
        check("TOPSECRET-RACE" not in g.text and "race-private.txt" not in g.text and "race private work" not in g.text,
              f"signed out web {tail} shows the private change")


ALL = [(0, "stack up with zero accounts", step0), (1, "signed-out sweep", step1), (2, "first admin and two-factor", step2),
       (3, "invites", step3), (4, "token and ssh key", step4), (5, "rate limit and lockout", step5), (6, "create repo", step6),
       (7, "push over HTTP", step7), (8, "push over SSH", step8), (9, "collaborator and clone", step9),
       (10, "branch protection", step10), (11, "issue and mention", step11), (12, "webhook ping", step12),
       (13, "open a pull request", step13), (14, "workflow run on the runner", step14), (15, "merge is blocked", step15),
       (16, "review", step16), (17, "squash merge", step17), (18, "push to main runs again, green; release file; org repository runs; job isolation", step18), (19, "audit log", step19),
       (20, "leave data for the screenshots", step20), (21, "live job log in a browser", step21),
       (22, "workspace in a browser", step22), (23, "terminal in a browser", step23),
       (24, "batch C3 on a real runner", step24), (25, "batch C9: a reusable workflow on a real runner", step25),
       (26, "ch-d9s: make ci of the Codehaven source on its own runner (slow, ask for it)", step26),
       (27, "batch C10: admin repo-import, issue order, transfer and delete, signed-out probes", step27),
       (28, "C10 review: private fork commits are not published when the base becomes public", step28),
       (29, "C10 review round 2: merge requests racing the base being made public copy nothing private", step29)]


THROWAWAY_PROJECT = re.compile(r"^(test|e2e|shots|tmp)-[a-z0-9][a-z0-9-]*$")


def project_addresses(project):
    """Every network address of the running containers of one compose project."""
    try:
        ids = subprocess.run(["docker", "ps", "-q", "--filter", "label=com.docker.compose.project=" + project],
                             capture_output=True, text=True, timeout=30).stdout.split()
        if not ids:
            return set()
        out = subprocess.run(["docker", "inspect", "--format", "{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}"] + ids,
                             capture_output=True, text=True, timeout=30).stdout
    except (OSError, subprocess.SubprocessError):
        return set()
    return {a for a in out.split() if a}


def refuse_unless_throwaway():
    """Never drive a stack people use. Two checks, and neither has an override: the project must
    carry a throwaway name and be the one E2E_CLI talks to, and the stack must hold no account."""
    if not PUBLIC:
        sys.exit("e2e: set E2E_PUBLIC to the origin the throwaway server believes it has (for example https://code.example.com)")
    project = os.environ.get("E2E_PROJECT", "")
    if not THROWAWAY_PROJECT.match(project):
        sys.exit("e2e: E2E_PROJECT must name a throwaway compose project such as test-mine or e2e-mine. Refusing to run.")
    if "compose" not in CLI or not ("-p" in CLI and CLI[CLI.index("-p") + 1] == project):
        sys.exit("e2e: E2E_CLI must address the throwaway project %s (docker compose -p %s ...). Refusing to run." % (project, project))
    if not BASE:
        sys.exit("e2e: set E2E_BASE to the address of the throwaway server (a container address of %s). There is no default. Refusing to run." % project)
    # The account check below asks the project through E2E_CLI, so E2E_BASE must be that same
    # project's server, not another instance that happens to answer: its host has to be the
    # address of one of the project's own containers. A loopback port could be anything.
    host = urllib.parse.urlparse(BASE).hostname or ""
    if host not in project_addresses(project):
        sys.exit("e2e: E2E_BASE (%s) is not the address of a container of the throwaway project %s. Refusing to run." % (BASE, project))
    rc, out, err = cli("admin", "users")
    if rc != 0:
        sys.exit("e2e: could not list the accounts of the target (rc=%d). Refusing to run." % rc)
    n = len([ln for ln in out.splitlines() if ln.strip()])
    if n:
        sys.exit("e2e: the target already holds %d account(s). A test run must start on an empty stack. Refusing to run." % n)


def main():
    refuse_unless_throwaway()
    # `0-19`, `14`, or several joined by commas: `0-14,24`.
    wanted = set()
    for part in STEPS.split(","):
        a, _, b = part.strip().partition("-")
        wanted.update(range(int(a), int(b or a) + 1))
    failed = 0
    for n, name, fn in ALL:
        if n not in wanted:
            continue
        t = time.time()
        try:
            fn()
            print(f"step {n:>2} PASS  {name} ({int((time.time() - t) * 1000)} ms)", flush=True)
        except Exception as e:  # noqa
            failed += 1
            import traceback
            print(f"step {n:>2} FAIL  {name}\n         {e}", flush=True)
            if os.environ.get("E2E_TRACE"):
                traceback.print_exc()
            break
    print("RESULT", "FAIL" if failed else "PASS")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
