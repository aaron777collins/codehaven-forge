#!/usr/bin/env python3
"""Fails when something that belongs to one deployment has crept into the tree.

Codehaven is meant to be cloned and run by anyone, so no file may carry the details of the server
one instance runs on: its domain, its host names, its addresses, its paths, the name of its runner.
A person's own name and email are not server details and are allowed (in a commit author, in a
LICENSE). Standard library only, and no git needed (the builder image has no .git). Run it from
anywhere:

    python3 deploy/check-portable.py                 check the checkout this file is in
    python3 deploy/check-portable.py --root DIR      check another directory (the export uses this)
    python3 deploy/check-portable.py --patterns F    also refuse each line of F (fixed text, any case)
    python3 deploy/check-portable.py --allow-emails  do not refuse email addresses (commit metadata)
    python3 deploy/check-portable.py --self-test     plant every kind of leak and expect a failure

What it checks, in file contents and in file names:

  server words  Words are split on punctuation and camel case, and each is compared as a SHA-256 of
                the lower-case word. The list below is hashes, so this file does not publish what
                it blocks: the domain, host names and paths of one deployment, never a person.
  addresses     Every IP literal must sit in a documentation range, be loopback, or be one of the
                well-known test values in KNOWN_TEST_ADDRESSES. Hashing an address would be pointless
                (there are only 2^32), so addresses are checked by rule instead.
  emails        The domain must be example.* , .test, .invalid or .localhost. LICENSE and
                THIRD_PARTY_NOTICES files are exempt: their copyright lines name the authors.
  paths         No home-directory path of a particular machine, and not the live install path.

A finding names the file, the line and the kind. It never prints the text it matched.
"""
import hashlib, ipaddress, os, re, sys, tempfile

# SHA-256 of lower-case words. Add one with:  python3 -c "import hashlib,sys;print(hashlib.sha256(sys.argv[1].lower().encode()).hexdigest())" word
OWNER_WORD_HASHES = frozenset("""
060d88f09920d939d933a915da0e4446696da43203d913cdcafc3b1083aa1d40
3e937169a0def6adeea9f4c72b80af73c893b003470615802970923bea778ec1
424f7b6802fde518ad47915879be5882c4bc54ebbe6c93411cebda2b1cdb60aa
913f8566936acaa4532730a7024fd5a3355b889daf86231d0dbf0dfadb9e085b
ab525b37864534f8217e377c166fc83ee8ef010b339d38a7d1a2c227eafdad04
f3d32e1e5b933df4c7a626e447ecd580c624c2412cbd7cb14c5a4ee99673ca02
fd61a03af4f77d870fc21e05e7e80678095c92d808cfb3b5c279ee04c74aca13
""".split())

DOC_V4 = [ipaddress.ip_network(n) for n in ("192.0.2.0/24", "198.51.100.0/24", "203.0.113.0/24")]
DOC_V6 = [ipaddress.ip_network("2001:db8::/32")]

# Ranges that are set aside by the standards for special uses. No deployment owns an address in
# them, and the address checks in ch-core and ch-notify are tested with them. The three ranges
# people actually number their machines from (10/8, 172.16/12, 192.168/16) and all public
# addresses are NOT here: those need an entry below.
SPECIAL_NETS = [ipaddress.ip_network(n) for n in (
    "0.0.0.0/8", "100.64.0.0/10", "127.0.0.0/8", "169.254.0.0/16", "192.0.0.0/24", "198.18.0.0/15", "224.0.0.0/4", "240.0.0.0/4",
    "::/128", "::1/128", "64:ff9b::/96", "100::/64", "2001::/32", "2002::/16", "fc00::/7", "fe80::/10", "fec0::/10", "ff00::/8")]

# Blocks used only as examples: the e2e harness invents client addresses in 10.9.0.0/16, and the
# docs, .env.example and tests use 10.20.0.0/16 for "a proxy network", "a free subnet" and the like.
TEST_NETS = [ipaddress.ip_network(n) for n in ("10.9.0.0/16", "10.20.0.0/16")]

# Well-known public test values and single addresses that the private-range checks are tested
# with. Nothing here belongs to a deployment.
KNOWN_TEST_ADDRESSES = frozenset("""
1.1.1.1 1.2.3.4 3.5.7.7 8.8.8.8 8.8.4.4 8.8.4.5 93.184.216.34
10.0.0.0 10.0.0.1 10.0.0.2 10.0.0.5 10.1.1.1 10.1.2.3 10.255.255.255
172.15.255.255 172.16.0.0 172.16.0.1 172.20.0.2 172.31.255.255 172.32.0.1
192.168.0.0 192.168.0.1 192.168.255.255
100.63.255.255 100.128.0.1 128.0.0.0
2606:4700:4700::1111 2001:4860:4860::8888
""".split())

SKIP_DIRS = {".git", "target", ".worktrees", "dist", "node_modules", "__pycache__"}
MAX_BYTES = 4_000_000

V4 = re.compile(r"(?<![\w.])(\d{1,3}(?:\.\d{1,3}){3})(?:/\d{1,2})?(?![\w])")
V6 = re.compile(r"(?<![\w:])((?:[0-9A-Fa-f]{0,4}:){2,7}(?:\d{1,3}(?:\.\d{1,3}){3}|[0-9A-Fa-f]{0,4}))(?:/\d{1,3})?(?![\w:.])")
EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@([A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+)")
# /home/user is the placeholder home of the browser terminal's shell, not anyone's machine.
PATHS = re.compile(r"(/Users/[A-Za-z0-9._-]+|/home/(?!codehaven\b|runner\b|user\b)[A-Za-z0-9._-]+|/opt/" r"codehaven\b)")
ALNUM = re.compile(r"[A-Za-z0-9]+")
CAMEL = re.compile(r"[A-Z]+(?![a-z])|[A-Z]?[a-z]+|[0-9]+")
OK_DOMAIN = re.compile(r"(^|\.)(example\.(com|org|net)|example|test|invalid|localhost)$", re.I)
# Short made-up domains used as examples of an address that should (or should not) validate, the
# one that ssh key comments carry, and the security contact inside the vendored just-bash.
OK_EMAIL_DOMAINS = frozenset("b.c b.co c.co d.co x.io b.com openssh.com anthropic.com vercel.com".split())


def sha(word):
    return hashlib.sha256(word.encode()).hexdigest()


def words_in(text):
    """Every lower-case word in text: whole alphanumeric runs and their camel-case parts. Words that
    are only neighbours are not joined: a person's two names side by side must not read as a domain."""
    out = set()
    for r in ALNUM.findall(text):
        out.add(r.lower())
        parts = CAMEL.findall(r)
        if len(parts) > 1:
            out.update(p.lower() for p in parts)
    return out


def ip_ok(text):
    try:
        ip = ipaddress.ip_address(text)
    except ValueError:
        return True  # not an address (a version number, a pair of module paths)
    if ip.version == 6 and ip.ipv4_mapped is not None:
        return ip_ok(str(ip.ipv4_mapped))
    if text in KNOWN_TEST_ADDRESSES:
        return True
    nets = (DOC_V4 if ip.version == 4 else DOC_V6) + [n for n in SPECIAL_NETS + TEST_NETS if n.version == ip.version]
    return any(ip in n for n in nets)


def scan_text(label, text, hashes, patterns, findings, emails=True):
    """label is a file path (for a name check, the same function is used on the path)."""
    for no, line in enumerate(text.split("\n"), 1):
        if len(line) > 200_000:
            continue
        if any(sha(w) in hashes for w in words_in(line)):
            findings.append((label, no, "server-specific word"))
        for m in V4.finditer(line):
            if not ip_ok(m.group(1)):
                findings.append((label, no, "ip address outside the documentation ranges"))
        for m in V6.finditer(line):
            g = m.group(1)
            if any(c.isdigit() for c in g) and not ip_ok(g):
                findings.append((label, no, "ip address outside the documentation ranges"))
        for m in (EMAIL.finditer(line) if emails else ()):
            dom = m.group(1)
            if not (OK_DOMAIN.search(dom) or dom.lower() in OK_EMAIL_DOMAINS or ("." in dom and ip_ok(dom) and dom[0].isdigit())):
                findings.append((label, no, "email address on a real domain"))
        if PATHS.search(line):
            findings.append((label, no, "absolute path of a particular machine"))
        low = line.lower()
        if any(p in low for p in patterns):
            findings.append((label, no, "private pattern"))


def check_tree(root, hashes=OWNER_WORD_HASHES, patterns=(), emails=True):
    findings = []
    patterns = [p.lower() for p in patterns if p]
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
        for name in sorted(filenames):
            if name in SKIP_DIRS:  # a worktree's .git is a file
                continue
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, root)
            if os.path.islink(full):
                continue
            # The name of the file or folder is part of the tree too.
            scan_text(rel + " (file name)", rel.replace(os.sep, "/"), hashes, patterns, findings, emails)
            try:
                if os.path.getsize(full) > MAX_BYTES:
                    continue
                data = open(full, "rb").read()
            except OSError:
                continue
            if b"\0" in data[:8000]:
                continue
            # A licence or a notices file keeps its authors' copyright lines whole, emails and all:
            # they are people, not details of a server.
            notice = name.startswith("LICENSE") or name == "THIRD_PARTY_NOTICES"
            scan_text(rel, data.decode("utf-8", "replace"), hashes, patterns, findings, emails and not notice)
    return findings


def report(findings):
    seen = set()
    for label, no, kind in findings:
        key = (label, no, kind)
        if key in seen:
            continue
        seen.add(key)
        print("%s:%s: %s" % (label, no, kind))
    print("check-portable: %d finding(s)" % len(seen))
    return len(seen)


def self_test():
    """Plants every kind of leak in a scratch tree and expects each to be caught, and a clean tree
    to pass. The planted word is made up, so its hash is passed in; the real list is checked to
    be non-empty."""
    assert OWNER_WORD_HASHES, "the server word list is empty"
    probe = "zzqx" + "selftest"
    hashes = frozenset([sha(probe)])
    cases = {
        "owner word": "name = %s\n" % probe,
        "owner word (camel case)": "let x = %sValue;\n" % probe,
        "owner word (in a host name)": "url = https://%s.example.net/x\n" % probe,
        "ip": "host = 192.168." + "77.9\n",
        "ipv6": "host = 2001:4860" + ":1234::5\n",
        "email": "contact: someone@" + "gmail.com\n",
        "path": "dir = /home/" + "somebody/work\n",
        "path (opt)": "dir = /opt/" + "codehaven/src\n",
    }
    bad = []
    for name, content in cases.items():
        with tempfile.TemporaryDirectory() as d:
            with open(os.path.join(d, "a.txt"), "w") as f:
                f.write(content)
            if not check_tree(d, hashes):
                bad.append(name)
    # The real list, not the made-up word: a host name, a runner name and the two domains of one deployment.
    for name, content in {"real list: host": "ssh de" + "v3\n", "real list: runner": "name = de" + "v3-1\n",
                          "real list: live domain": "url = https://code." + "aaron" + "collins.info\n",
                          "real list: old test domain": "url = https://te" + "st3." + "aaron" + "collins.info\n"}.items():
        with tempfile.TemporaryDirectory() as d:
            with open(os.path.join(d, "a.txt"), "w") as f:
                f.write(content)
            if not check_tree(d):
                bad.append(name)
    with tempfile.TemporaryDirectory() as d:
        os.mkdir(os.path.join(d, "x-" + probe))
        open(os.path.join(d, "x-" + probe, "f.txt"), "w").write("ok\n")
        if not check_tree(d, hashes):
            bad.append("owner word in a folder name")
    with tempfile.TemporaryDirectory() as d:
        open(os.path.join(d, "a.txt"), "w").write("private-marker-text\n")
        if not check_tree(d, hashes, patterns=["private-marker-text"]):
            bad.append("private pattern")
    with tempfile.TemporaryDirectory() as d:
        open(os.path.join(d, "a.txt"), "w").write(
            "url = https://code.example.com\nip = 203.0.113.7 and 127.0.0.1 and 2001:db8::1\nmail = a@example.com b@x.invalid\npath = /home/codehaven\n")
        if check_tree(d, hashes):
            bad.append("a clean tree was refused")
    with tempfile.TemporaryDirectory() as d:
        # A person's name and email are allowed: two names side by side are not a domain, and the
        # email rule can be switched off for commit metadata.
        open(os.path.join(d, "a.txt"), "w").write("Jane Doe <jane" + "@gmail.com>\n")
        if check_tree(d, hashes, emails=False):
            bad.append("a person's name and email were refused when emails are allowed")
    if bad:
        print("check-portable self-test FAILED: " + ", ".join(bad))
        return 1
    print("check-portable self-test: every planted leak was caught and a clean tree passed")
    return 0


def main(argv):
    root = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
    patterns = []
    emails = True
    args = list(argv)
    while args:
        a = args.pop(0)
        if a == "--self-test":
            return self_test()
        elif a == "--root":
            root = args.pop(0)
        elif a == "--allow-emails":
            emails = False
        elif a == "--patterns":
            with open(args.pop(0)) as f:
                patterns += [ln.rstrip("\n") for ln in f if ln.strip() and not ln.startswith("#")]
        else:
            print(__doc__)
            return 2
    return 1 if report(check_tree(root, patterns=patterns, emails=emails)) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
