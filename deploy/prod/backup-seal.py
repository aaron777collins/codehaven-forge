#!/usr/bin/env python3
"""Authenticates an off-host backup copy (encrypt, then MAC), with the Python standard library.

  backup-seal.py tag    <enc-file> <passphrase-file>          append the tag to an openssl enc file
  backup-seal.py verify <sealed-file> <passphrase-file> <out>  check the tag, write the enc file without it

The sealed file is what `openssl enc -aes-256-cbc -pbkdf2 -salt` wrote ("Salted__", 8 bytes of
salt, the ciphertext) followed by a 32 byte HMAC-SHA256 tag over all of it. The MAC key is
PBKDF2-HMAC-SHA256 of the passphrase with its own salt (a fixed label and the file's salt), so it
is not the key the ciphertext uses. `verify` compares in constant time and writes nothing unless
the tag matches. The passphrase is the first line of its file, as openssl reads it, and is never
put on a command line. Exit status: 0 good, 2 usage, 3 refused.
"""
import hashlib
import hmac
import os
import sys

MAGIC = b"Salted__"
HEADER = 16
TAG = 32
ITER = 600000
LABEL = b"codehaven-backup-mac-v1"
CHUNK = 1 << 20


def passphrase(path):
    with open(path, "rb") as f:
        line = f.readline()
    line = line.split(b"\n", 1)[0]
    if not line:
        raise SystemExit("the passphrase file is empty")
    return line


def mac_over(f, length, header, key):
    h = hmac.new(key, LABEL, hashlib.sha256)
    h.update(header)
    left = length - len(header)
    while left > 0:
        chunk = f.read(min(CHUNK, left))
        if not chunk:
            raise SystemExit("short read")
        h.update(chunk)
        left -= len(chunk)
    return h.digest()


def key_for(pw, header):
    return hashlib.pbkdf2_hmac("sha256", pw, LABEL + header[8:16], ITER, 32)


def read_header(f):
    header = f.read(HEADER)
    if len(header) < HEADER or header[:8] != MAGIC:
        return None
    return header


def tag(enc, passfile):
    pw = passphrase(passfile)
    size = os.path.getsize(enc)
    with open(enc, "rb") as f:
        header = read_header(f)
        if header is None:
            print("backup-seal: not an openssl enc file with a salt", file=sys.stderr)
            return 3
        digest = mac_over(f, size, header, key_for(pw, header))
    with open(enc, "ab") as f:
        f.write(digest)
    return 0


def verify(sealed, passfile, out):
    pw = passphrase(passfile)
    size = os.path.getsize(sealed)
    if size < HEADER + TAG:
        print("backup-seal: the copy is too short to be a sealed backup; nothing was written.", file=sys.stderr)
        return 3
    body = size - TAG
    with open(sealed, "rb") as f:
        header = read_header(f)
        if header is None:
            print("backup-seal: the copy is not a sealed backup; nothing was written.", file=sys.stderr)
            return 3
        want = mac_over(f, body, header, key_for(pw, header))
        got = f.read(TAG)
        if not hmac.compare_digest(want, got):
            print("backup-seal: the copy does not match its tag (changed, cut short, or the wrong passphrase); nothing was written.", file=sys.stderr)
            return 3
        f.seek(0)
        fd = os.open(out, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "wb") as o:
            left = body
            while left > 0:
                chunk = f.read(min(CHUNK, left))
                o.write(chunk)
                left -= len(chunk)
    return 0


def main(argv):
    if len(argv) == 4 and argv[1] == "tag":
        return tag(argv[2], argv[3])
    if len(argv) == 5 and argv[1] == "verify":
        return verify(argv[2], argv[3], argv[4])
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
