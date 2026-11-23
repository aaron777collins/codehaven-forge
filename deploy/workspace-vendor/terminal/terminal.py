#!/usr/bin/env python3
"""Vendors xterm.js and just-bash into the workspace terminal.

The terminal is xterm.js in the frame plus just-bash inside a Worker, and both arrive from
crates/ch-web/workspace/vendor/terminal/ as prebuilt, gzipped files rather than as npm
dependencies. build.sh packs the pinned xterm tarball and bundles just-bash with esbuild;
this script checks every input against the constants below, then writes the three files
plus the LOCK and THIRD_PARTY_NOTICES that `cargo xtask vendor-check` checks. vendor.py
rewrites vendor/LOCK whole, so the terminal keeps its own LOCK and notices in its own
subfolder and neither script touches the other's output.

Standard library only, and deterministic: the gzip members carry mtime 0, so running it
twice on the same inputs writes the same bytes. Run by build.sh, or alone:

    python3 deploy/workspace-vendor/terminal/terminal.py --xterm xterm-xterm-6.0.0.tgz \
        --bundle just-bash.js --meta meta.json --modules node_modules --lock package-lock.json
"""
import argparse
import base64
import gzip
import hashlib
import json
import sys
import tarfile
from pathlib import Path

XTERM_VERSION = "6.0.0"
XTERM_SOURCE = "https://registry.npmjs.org/@xterm/xterm/-/xterm-6.0.0.tgz"
XTERM_INTEGRITY = "sha512-TQwDdQGtwwDt+2cgKDLn0IRaSxYu1tSUjgKarSDkUM0ZNiSRXFpjxEsvc/Zgc5kq5omJ+V0a8/kIM2WD3sMOYg=="
XTERM_PUBLISHED = "2025-12-22"
JUST_BASH_VERSION = "3.6.0"
JUST_BASH_SOURCE = "https://registry.npmjs.org/just-bash/-/just-bash-3.6.0.tgz"
JUST_BASH_INTEGRITY = "sha512-2COjEgcHdqKXeQSiNfPAHmpwQx4S8wATaIwog83g2t645U5tom6FED4CBBdr3M+0ioX4xZ26eODw3/nD6RQ6xw=="
JUST_BASH_PUBLISHED = "2026-10-01"
JUST_BASH_LICENCE = "Apache-2.0"

# Packages our own esbuild step pulls in beside just-bash's prebuilt browser bundle: name -> (version, integrity, licence).
OWN_INPUTS = {
    "diff": ("8.0.4", "sha512-DPi0FmjiSU5EvQV0++GFDOJ9ASQUVFh5kD+OzOnYdi7n3Wpm9hWWGfB/O2blfHcMVTL5WkQXSnRiK9makhrcnw==", "BSD-3-Clause"),
    "sprintf-js": ("1.1.3", "sha512-Oo+0REFV59/rz3gfJNKQiBlwfHaSESl1pcGyABQsnnIfWOFt6JNj5gCog2U6MLZ//IGYD+nA8nI+mTShREReaA==", "BSD-3-Clause"),
}

# Code that just-bash's own build already inlined into dist/bundle/browser.js, found by matching
# each dependency's string literals against that file. Listed with the licence of the package.
INLINED = [("re2js", "MIT"), ("file-type", "MIT"), ("strtok3", "MIT"), ("token-types", "MIT"), ("@tokenizer/inflate", "MIT"),
           ("@borewit/text-codec", "MIT"), ("ieee754", "BSD-3-Clause"), ("uint8array-extras", "MIT"),
           ("debug", "MIT"), ("ms", "MIT")]

# The only files our esbuild step may read. Anything else in meta.json fails the build.
ALLOWED_INPUT_PREFIXES = ("entry.js", "stubs/", "node_modules/just-bash/dist/bundle/browser.js", "node_modules/diff/", "node_modules/sprintf-js/")

# What a bundle that grew a network or code-loading path would hold. just-bash's curl code holds
# "fetch(" but is never registered, so that one is not checked.
FORBIDDEN_IN_BUNDLE = ("importScripts(", "new Worker(", "WebAssembly.instantiate")

# Where a package keeps its licence text, in the order npm packages usually spell the name.
LICENCE_FILE_NAMES = ("LICENSE", "LICENSE.md", "LICENSE.txt", "license", "license.md", "LICENCE")

JUST_BASH_BROWSER = "node_modules/just-bash/dist/bundle/browser.js"

LEFT_OUT = """The terminal bundle replaces turndown (used by html-to-markdown) and node:zlib (used by
gzip, gunzip and zcat) with stubs of Codehaven's own that refuse to run, so neither is
shipped. just-bash's Python, JavaScript, SQLite and network commands are not bundled and
not offered."""

SEPARATOR = "-" * 70


def fail(message):
    print("terminal.py: " + message, file=sys.stderr)
    sys.exit(1)


def sha512_integrity(data):
    return "sha512-" + base64.b64encode(hashlib.sha512(data).digest()).decode("ascii")


def sha256_hex(data):
    return hashlib.sha256(data).hexdigest()


def read_json(path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as err:
        fail("%s: cannot be read as JSON: %s" % (path, err))


def strip_source_map(text):
    """Removes a final sourceMappingURL comment: we ship no map, so the browser must not ask for one."""
    lines = text.split("\n")
    while lines and lines[-1] == "":
        lines.pop()
    if lines and lines[-1].startswith("//# sourceMappingURL="):
        lines.pop()
        return "\n".join(lines) + "\n"
    return text


def read_xterm(tarball):
    """Checks the xterm tarball; returns (tarball bytes, xterm.js, xterm.css, licence text)."""
    if not tarball.is_file():
        fail("%s: no such file" % tarball)
    data = tarball.read_bytes()
    integrity = sha512_integrity(data)
    if integrity != XTERM_INTEGRITY:
        fail("%s: integrity is %s, expected %s (@xterm/xterm %s as published). "
             "Bump the constants at the top of this file only as a deliberate step."
             % (tarball, integrity, XTERM_INTEGRITY, XTERM_VERSION))
    try:
        tf = tarfile.open(tarball, "r:gz")
    except tarfile.TarError as err:
        fail("%s: cannot be opened as a tarball: %s" % (tarball, err))
    wanted = {"package/lib/xterm.js", "package/css/xterm.css", "package/LICENSE"}
    found = {}
    with tf:
        members = tf.getmembers()
        for member in members:
            # Nothing is extracted by member name, but refuse the shapes that make path tricks
            # possible rather than quietly working around them.
            if ".." in member.name or member.name.startswith("/") or member.islnk() or member.issym():
                fail("refusing tarball member %s: path escape or link" % member.name)
        for member in members:
            if member.isfile() and member.name in wanted:
                handle = tf.extractfile(member)
                if handle is None:
                    fail("cannot read %s from the tarball" % member.name)
                found[member.name] = handle.read()
    for name in sorted(wanted):
        if name not in found:
            fail("the tarball has no %s" % name)
    try:
        js = found["package/lib/xterm.js"].decode("utf-8")
        licence_text = found["package/LICENSE"].decode("utf-8")
    except UnicodeDecodeError as err:
        fail("the tarball holds a file that is not UTF-8: %s" % err)
    if "Terminal" not in js:
        fail("package/lib/xterm.js does not contain Terminal, so it is not the xterm bundle we pin")
    return data, strip_source_map(js).encode("utf-8"), found["package/css/xterm.css"], licence_text


def check_lock(lock_path):
    """Checks that the lockfile pins just-bash and our own inputs at the versions recorded above."""
    packages = read_json(lock_path).get("packages", {})
    wanted = {"node_modules/just-bash": (JUST_BASH_VERSION, JUST_BASH_INTEGRITY)}
    for name, (version, integrity, _licence) in OWN_INPUTS.items():
        wanted["node_modules/" + name] = (version, integrity)
    for key, (version, integrity) in wanted.items():
        entry = packages.get(key)
        if entry is None:
            fail("%s: has no %s entry" % (lock_path, key))
        for field, expected in (("version", version), ("integrity", integrity)):
            if entry.get(field) != expected:
                fail("%s: %s has %s %s, expected %s"
                     % (lock_path, key, field, entry.get(field), expected))


def check_meta(meta_path):
    """Checks that the esbuild bundle reads only the inputs we allow, and just-bash above all."""
    inputs = read_json(meta_path).get("inputs", {})
    for name in inputs:
        if not name.startswith(ALLOWED_INPUT_PREFIXES):
            fail("%s: input %s is not allowed; the bundle may read only entry.js, the stubs, "
                 "just-bash's browser bundle, diff and sprintf-js" % (meta_path, name))
    if JUST_BASH_BROWSER not in inputs:
        fail("%s: %s is not an input, so the bundle is not built from just-bash"
             % (meta_path, JUST_BASH_BROWSER))


def check_bundle(bundle_path):
    """Checks the esbuild bundle and returns its bytes."""
    try:
        data = bundle_path.read_bytes()
    except OSError as err:
        fail("%s: cannot be read: %s" % (bundle_path, err))
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError as err:
        fail("%s: is not UTF-8: %s" % (bundle_path, err))
    if "JustBash" not in text:
        fail("%s: does not contain JustBash, so the global-name step went wrong" % bundle_path)
    for needle in FORBIDDEN_IN_BUNDLE:
        if needle in text:
            fail("%s: contains %s, which means a network or code-loading path got in"
                 % (bundle_path, needle))
    return data


def read_licences(modules):
    """Returns {name: (version from its package.json, licence text)} for every package we ship."""
    expected = {"just-bash": JUST_BASH_LICENCE}
    expected.update({name: fields[2] for name, fields in OWN_INPUTS.items()})
    expected.update(dict(INLINED))
    licences = {}
    for name, want in expected.items():
        package_dir = modules / name
        text = None
        for file_name in LICENCE_FILE_NAMES:
            candidate = package_dir / file_name
            if candidate.is_file():
                try:
                    text = candidate.read_text(encoding="utf-8")
                except (OSError, UnicodeDecodeError) as err:
                    fail("%s: cannot be read: %s" % (candidate, err))
                break
        if text is None:
            fail("%s: has no licence file (looked for %s)" % (package_dir, ", ".join(LICENCE_FILE_NAMES)))
        fields = read_json(package_dir / "package.json")
        if fields.get("license") != want:
            fail("%s: package.json says license %s, expected %s"
                 % (package_dir / "package.json", fields.get("license"), want))
        licences[name] = (fields.get("version"), text)
    return licences


def prepare_out(out):
    """Creates the output folder and clears only the files a previous run of this script wrote."""
    out.mkdir(parents=True, exist_ok=True)
    for path in sorted(out.iterdir()):
        if path.is_file() and (path.name.endswith(".gz") or path.name in ("LOCK", "THIRD_PARTY_NOTICES")):
            path.unlink()


def write_gz(out, name, data):
    """Writes <out>/<name>.gz and returns the gz bytes."""
    gz = gzip.compress(data, compresslevel=9, mtime=0)
    (out / (name + ".gz")).write_bytes(gz)
    print("terminal.py: wrote %s.gz, %d bytes gzipped" % (name, len(gz)))
    return gz


def build_lock(tarball_sha256, written):
    lines = [
        "# What is vendored for the workspace terminal, and where it came from.",
        "# Written by deploy/workspace-vendor/terminal/build.sh. Checked by `cargo xtask vendor-check`.",
        "component xterm",
        "version " + XTERM_VERSION,
        "published " + XTERM_PUBLISHED,
        "source " + XTERM_SOURCE,
        "integrity " + XTERM_INTEGRITY,
        "tarball-sha256 " + tarball_sha256,
        "licence MIT",
        "component just-bash",
        "version " + JUST_BASH_VERSION,
        "published " + JUST_BASH_PUBLISHED,
        "source " + JUST_BASH_SOURCE,
        "integrity " + JUST_BASH_INTEGRITY,
        "licence " + JUST_BASH_LICENCE,
    ]
    lines += ["bundles %s %s %s" % (name, version, licence)
              for name, (version, _integrity, licence) in OWN_INPUTS.items()]
    lines += ["bundles %s %s" % (name, licence) for name, licence in INLINED]
    lines.append("# just-bash's own build inlined the packages above; the version is whatever its lockfile held.")
    lines += ["file terminal/%s.gz %s" % (name, sha256_hex(gz)) for name, gz in written]
    return "\n".join(lines) + "\n"


def build_notices(xterm_licence, licences):
    """Builds the notices; licences maps each package name to (version, licence text)."""
    title = "Third-party code in the Codehaven workspace terminal"
    sections = [
        ("xterm.js %s (https://github.com/xtermjs/xterm.js), MIT" % XTERM_VERSION, xterm_licence),
        ("just-bash %s (https://github.com/vercel-labs/just-bash), %s"
         % (JUST_BASH_VERSION, JUST_BASH_LICENCE), licences["just-bash"][1]),
    ]
    for name in OWN_INPUTS:
        version, text = licences[name]
        sections.append(("%s %s, %s, bundled inside just-bash.js" % (name, version, OWN_INPUTS[name][2]), text))
    for name, licence in INLINED:
        version, text = licences[name]
        sections.append(("%s %s, %s, bundled inside just-bash.js" % (name, version, licence), text))
    sections.append(("What was left out", LEFT_OUT))
    parts = [
        title + "\n" + "=" * len(title) + "\n\n",
        "Codehaven itself is MIT licensed. The files under crates/ch-web/workspace/vendor/terminal/ are\n"
        "other people's work, redistributed under the licences below.\n\n",
    ]
    for heading, body in sections:
        parts.append(SEPARATOR + "\n" + heading + "\n" + SEPARATOR + "\n" + body.rstrip() + "\n\n")
    return "".join(parts).rstrip("\n") + "\n"


def main():
    # deploy/workspace-vendor/terminal/terminal.py: the repository root is three folders up.
    root = Path(__file__).resolve().parents[3]
    parser = argparse.ArgumentParser(description="Vendor xterm.js and just-bash for the workspace terminal.")
    parser.add_argument("--xterm", required=True, type=Path,
                        help="path to the @xterm/xterm %s tarball" % XTERM_VERSION)
    parser.add_argument("--bundle", required=True, type=Path,
                        help="path to the just-bash.js esbuild wrote")
    parser.add_argument("--meta", required=True, type=Path,
                        help="path to the esbuild metafile of the bundle")
    parser.add_argument("--modules", required=True, type=Path,
                        help="path to the node_modules the bundle was built from")
    parser.add_argument("--lock", required=True, type=Path,
                        help="path to the package-lock.json npm ci installed")
    parser.add_argument("--out", type=Path, default=root / "crates/ch-web/workspace/vendor/terminal",
                        help="folder to write into (default: %(default)s)")
    args = parser.parse_args()

    tarball_bytes, xterm_js, xterm_css, xterm_licence = read_xterm(args.xterm)
    check_lock(args.lock)
    check_meta(args.meta)
    bundle = check_bundle(args.bundle)
    licences = read_licences(args.modules)

    prepare_out(args.out)
    written = [
        ("just-bash.js", write_gz(args.out, "just-bash.js", bundle)),
        ("xterm.css", write_gz(args.out, "xterm.css", xterm_css)),
        ("xterm.js", write_gz(args.out, "xterm.js", xterm_js)),
    ]
    (args.out / "LOCK").write_text(build_lock(sha256_hex(tarball_bytes), written), encoding="utf-8")
    (args.out / "THIRD_PARTY_NOTICES").write_text(build_notices(xterm_licence, licences), encoding="utf-8")


if __name__ == "__main__":
    main()
