#!/usr/bin/env bash
# Opens an off-host backup copy made by backup-offsite.sh:   backup-open.sh <name.enc> <out-archive>
#
# It checks the copy's tag first and refuses a copy that was changed, cut short, or sealed with
# another passphrase; nothing is decrypted and nothing is written in that case. Then it decrypts
# to <out-archive>, which must not exist yet. The passphrase file is the one the daily run uses:
# CODEHAVEN_BACKUP_OFFSITE_PASSPHRASE_FILE, from the environment or the env file. Needs openssl
# and python3 (the standard library only), and no root.
set -euo pipefail
CH_SCRIPT=backup-open
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
ch_env_load
[ $# -eq 2 ] || { printf 'usage: backup-open.sh <name.enc> <out-archive>\n' >&2; exit 2; }
sealed="$1"; out="$2"
[ -f "$sealed" ] || ch_die "$sealed is not a regular file."
[ ! -e "$out" ] || ch_die "$out already exists; this never overwrites."
passfile="${CODEHAVEN_BACKUP_OFFSITE_PASSPHRASE_FILE:-}"
[ -n "$passfile" ] && [ -s "$passfile" ] || ch_die "CODEHAVEN_BACKUP_OFFSITE_PASSPHRASE_FILE must name the passphrase file."
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
umask 077
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
python3 "$here/backup-seal.py" verify "$sealed" "$passfile" "$tmp/enc" \
  || ch_die "$sealed was refused; nothing was written."
openssl enc -d -aes-256-cbc -pbkdf2 -iter 600000 -pass "file:$passfile" -in "$tmp/enc" -out "$tmp/plain" \
  || ch_die "openssl could not decrypt $sealed; nothing was written."
mv "$tmp/plain" "$out"
