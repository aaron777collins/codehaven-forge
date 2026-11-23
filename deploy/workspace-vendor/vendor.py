#!/usr/bin/env python3
"""Vendors Monaco into the browser workspace.

The server embeds crates/ch-web/workspace/vendor/ and serves each X.gz file as X with
Content-Encoding: gzip, so the editor arrives as prebuilt, gzipped files rather than as an
npm dependency. This script takes the official monaco-editor tarball, checks it is the exact
release we recorded, drops the parts the workspace never loads, and writes the files plus the
LOCK and THIRD_PARTY_NOTICES that `cargo xtask vendor-check` then checks. The four dropped
language services are replaced by stubs of our own that start no worker.

Standard library only, and deterministic: the gzip members carry mtime 0, so running it twice
on the same tarball writes the same bytes. Run it from anywhere:

    python3 deploy/workspace-vendor/vendor.py --monaco path/to/monaco-editor-0.57.0.tgz
"""
import argparse
import base64
import gzip
import hashlib
import re
import shutil
import sys
import tarfile
from pathlib import Path

MONACO_VERSION = "0.57.0"
MONACO_SOURCE = "https://registry.npmjs.org/monaco-editor/-/monaco-editor-0.57.0.tgz"
MONACO_INTEGRITY = "sha512-5BkI9KGoqrNvBGUe15/QlZq3OooZ8WLg1AxTpaqHRCP3HNpzPPZKE2EDz8M7c+VRmCeUw1Brp4cx/PWm3kI/5A=="
MONACO_PUBLISHED = "2026-09-24"

# The npm tarball unpacks under package/. Only the prebuilt AMD bundle under package/min/vs/
# is worth anything to us, and even that in part.
VENDOR_PREFIX = "package/min/vs/"

# What the workspace never loads, and so what we do not ship: the localised strings, and the
# TypeScript, JSON, CSS and HTML language services, which between them are most of the download.
# The plain colour modules (typescript-, javascript-, css-, html-) are kept; only the language services go.
SKIP_PATTERNS = [
    re.compile(r"^(language|nls)/"),
    re.compile(r"^assets/(css|html|json|ts)\.worker"),  # the workers; the tiny vs/*.worker stubs that name them are kept
    re.compile(r"(^|/)(tsMode|jsonMode|cssMode|htmlMode|lspLanguageFeatures|workers)-"),
]

# The four language contributions inside editor.main load these modules when a file of their
# language opens. We do not ship the language services, so each one is replaced by a stub of our
# own that sets nothing up and starts no worker. Without it the browser asks for a file we do
# not have and logs a 404.
STUB_PATTERN = re.compile(r"^(tsMode|jsonMode|cssMode|htmlMode)-[A-Za-z0-9_-]+\.js$")

STUB_TEMPLATE = (
    'define("vs/%s",["exports"],(function(e){"use strict";'
    'const n={dispose(){}},s=()=>n,w=()=>Promise.reject(new Error("Codehaven does not ship language services"));'
    'e.setupMode=s;e.setupMode1=s;e.setupTypeScript=s;e.setupJavaScript=s;'
    'e.getWorker=w;e.getTypeScriptWorker=w;e.getJavaScriptWorker=w;'
    'Object.defineProperty(e,Symbol.toStringTag,{value:"Module"})}));\n'
)

DOMPURIFY_NOTICE = """Copyright Dr.-Ing. Mario Heiderich, Cure53. DOMPurify is dual licensed under the
Apache License 2.0 and the Mozilla Public License 2.0. Codehaven uses it under the
Apache License 2.0. The full text of the Apache License 2.0 is in the Monaco notices
above."""

CODICONS_NOTICE = """Copyright (c) Microsoft Corporation. The Codicons icons are licensed under the
Creative Commons Attribution 4.0 International licence (CC BY 4.0),
https://creativecommons.org/licenses/by/4.0/. The font inside Monaco's stylesheet is
unmodified."""

SEPARATOR = "-" * 70


def fail(message):
    print("vendor.py: " + message, file=sys.stderr)
    sys.exit(1)


def sha512_integrity(data):
    return "sha512-" + base64.b64encode(hashlib.sha512(data).digest()).decode("ascii")


def sha256_hex(data):
    return hashlib.sha256(data).hexdigest()


def read_member(tf, member):
    handle = tf.extractfile(member)
    if handle is None:
        fail("cannot read %s from the tarball" % member.name)
    return handle.read()


def read_tarball(tarball):
    """Checks the tarball and returns (tarball bytes, [(rel, bytes)], licence text, notices text, [stub])."""
    if not tarball.is_file():
        fail("%s: no such file" % tarball)
    data = tarball.read_bytes()
    integrity = sha512_integrity(data)
    if integrity != MONACO_INTEGRITY:
        fail("%s: integrity is %s, expected %s (monaco-editor %s as published). "
             "Bump the constants at the top of this file only as a deliberate step."
             % (tarball, integrity, MONACO_INTEGRITY, MONACO_VERSION))
    try:
        tf = tarfile.open(tarball, "r:gz")
    except tarfile.TarError as err:
        fail("%s: cannot be opened as a tarball: %s" % (tarball, err))
    kept = []
    stubs = []
    licence_text = None
    notices_text = None
    with tf:
        members = tf.getmembers()
        for member in members:
            # Nothing is extracted by member name, but refuse the shapes that make path tricks
            # possible rather than quietly working around them.
            if ".." in member.name or member.name.startswith("/") or member.islnk() or member.issym():
                fail("refusing tarball member %s: path escape or link" % member.name)
        for member in members:
            if not member.isfile():
                continue
            if member.name == "package/LICENSE":
                licence_text = read_member(tf, member).decode("utf-8")
            elif member.name == "package/ThirdPartyNotices.txt":
                notices_text = read_member(tf, member).decode("utf-8")
            elif member.name.startswith(VENDOR_PREFIX):
                rel = member.name[len(VENDOR_PREFIX):]
                if STUB_PATTERN.match(rel):
                    kept.append((rel, (STUB_TEMPLATE % rel[:-3]).encode("utf-8")))
                    stubs.append(rel)
                elif not any(pattern.search(rel) for pattern in SKIP_PATTERNS):
                    kept.append((rel, read_member(tf, member)))
    if licence_text is None:
        fail("the tarball has no package/LICENSE")
    if notices_text is None:
        fail("the tarball has no package/ThirdPartyNotices.txt")
    return data, kept, licence_text, notices_text, stubs


def write_vendor_files(out, kept):
    """Writes <out>/monaco/vs/<rel>.gz for every kept file. Returns [(lock path, gz bytes)]."""
    monaco_dir = out / "monaco"
    if monaco_dir.exists():
        shutil.rmtree(monaco_dir)
    written = []
    for rel, data in sorted(kept):
        dest = monaco_dir / "vs" / (rel + ".gz")
        dest.parent.mkdir(parents=True, exist_ok=True)
        gz = gzip.compress(data, compresslevel=9, mtime=0)
        dest.write_bytes(gz)
        written.append(("monaco/vs/" + rel + ".gz", gz))
    return written


def build_lock(tarball_sha256, written, stubs):
    lines = [
        "# What is vendored for the browser workspace, and where it came from.",
        "# Written by deploy/workspace-vendor/vendor.py. Checked by `cargo xtask vendor-check`.",
        "component monaco-editor",
        "version " + MONACO_VERSION,
        "published " + MONACO_PUBLISHED,
        "source " + MONACO_SOURCE,
        "integrity " + MONACO_INTEGRITY,
        "tarball-sha256 " + tarball_sha256,
        "licence MIT",
        # Third-party code that lives inside Monaco's own files, not in files of our own.
        "bundles DOMPurify 3.4.15 Apache-2.0",
        "bundles marked MIT",
        "bundles codicons CC-BY-4.0",
    ]
    lines.append("# A stub line names a file that is Codehaven's own code in place of a Monaco language service.")
    lines += ["stub monaco/vs/" + rel + ".gz" for rel in sorted(stubs)]
    lines += ["file %s %s" % (path, sha256_hex(gz)) for path, gz in sorted(written)]
    return "\n".join(lines) + "\n"


def build_notices(licence_text, notices_text):
    title = "Third-party code in the Codehaven browser workspace"
    sections = [
        ("Monaco Editor %s (https://github.com/microsoft/monaco-editor), MIT" % MONACO_VERSION, licence_text),
        ("Notices that ship with Monaco Editor (ThirdPartyNotices.txt)", notices_text),
        ("DOMPurify 3.4.15 (https://github.com/cure53/DOMPurify), bundled inside Monaco", DOMPURIFY_NOTICE),
        ("Codicons (https://github.com/microsoft/vscode-codicons), bundled inside Monaco", CODICONS_NOTICE),
    ]
    parts = [
        title + "\n" + "=" * len(title) + "\n\n",
        "Codehaven itself is MIT licensed. The files under crates/ch-web/workspace/vendor/ are\n"
        "other people's work, redistributed under the licences below.\n\n",
    ]
    for heading, body in sections:
        parts.append(SEPARATOR + "\n" + heading + "\n" + SEPARATOR + "\n" + body.rstrip() + "\n\n")
    return "".join(parts).rstrip("\n") + "\n"


def main():
    # deploy/workspace-vendor/vendor.py: the repository root is two folders up.
    root = Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser(description="Vendor Monaco into the browser workspace.")
    parser.add_argument("--monaco", required=True, type=Path,
                        help="path to the monaco-editor %s tarball" % MONACO_VERSION)
    parser.add_argument("--out", type=Path, default=root / "crates/ch-web/workspace/vendor",
                        help="folder to write into (default: %(default)s)")
    args = parser.parse_args()

    tarball_bytes, kept, licence_text, notices_text, stubs = read_tarball(args.monaco)
    if len(stubs) != 4:
        fail("expected 4 language mode stubs, made %d" % len(stubs))
    args.out.mkdir(parents=True, exist_ok=True)
    written = write_vendor_files(args.out, kept)
    (args.out / "LOCK").write_text(build_lock(sha256_hex(tarball_bytes), written, stubs), encoding="utf-8")
    (args.out / "THIRD_PARTY_NOTICES").write_text(build_notices(licence_text, notices_text), encoding="utf-8")

    total = sum(len(gz) for _, gz in written)
    print("vendor.py: wrote %d files, %d bytes gzipped, into %s" % (len(written), total, args.out / "monaco"))


if __name__ == "__main__":
    main()
