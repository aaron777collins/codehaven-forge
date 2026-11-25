#!/usr/bin/env bash
# Sends one encrypted copy of a backup archive off the host. backup-daily.sh runs it after
# its own backup and prune:   backup-offsite.sh <archive>
#
# Nothing is sent, and nothing is printed, until CODEHAVEN_BACKUP_OFFSITE names a
# destination; with it unset or empty this exits at once and the daily run is unchanged.
#
# Settings, from the environment or the env file:
#   CODEHAVEN_BACKUP_OFFSITE                  rsync:<destination> or s3://<bucket>/<prefix>
#   CODEHAVEN_BACKUP_OFFSITE_ENDPOINT         an S3-compatible endpoint, passed as --endpoint-url
#   CODEHAVEN_BACKUP_OFFSITE_PASSPHRASE_FILE  the root-only passphrase file (mode 600 or 400)
#
# The archive holds the instance's master key, so it never leaves the host unencrypted: it
# is sealed with openssl AES-256 into a scratch copy, and only that .enc file is sent. The
# passphrase is read by openssl from its file and never appears on a command line. The S3
# credentials are whatever the aws CLI already has for root; this script reads and prints
# no credential.
#
# The remote side is never pruned. What the destination keeps is the operator's retention
# policy, not Codehaven's.
set -euo pipefail
CH_SCRIPT=backup-offsite
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
ch_env_load

# Off by default: with no destination named there is nothing to do.
[ -n "${CODEHAVEN_BACKUP_OFFSITE:-}" ] || exit 0

[ $# -eq 1 ] || { printf 'usage: backup-offsite.sh <archive>\n' >&2; exit 2; }
archive="$1"
[ -f "$archive" ] || ch_die "$archive is not a regular file."

# Where to send. Everything after rsync: is the destination, and it may not start with '-'.
dest=""
case "$CODEHAVEN_BACKUP_OFFSITE" in
  rsync:?*)
    dest="${CODEHAVEN_BACKUP_OFFSITE#rsync:}"
    case "$dest" in
      -*) ch_die "CODEHAVEN_BACKUP_OFFSITE: the rsync destination may not start with '-'." ;;
    esac
    ;;
  s3://?*) dest="$CODEHAVEN_BACKUP_OFFSITE" ;;
  *)
    ch_die "CODEHAVEN_BACKUP_OFFSITE must be rsync:<destination> (for example rsync:backup@host:/srv/codehaven-backups/) or s3://<bucket>/<prefix>."
    ;;
esac

# One trailing slash on the destination, whatever it was given with, so the file lands
# inside it and the last line names the whole remote path.
while :; do
  case "$dest" in
    */) dest="${dest%/}" ;;
    *) break ;;
  esac
done
dest="$dest/"

# The passphrase file: root-only, or the passphrase is readable by others. openssl reads it
# directly; this script never opens it.
passfile="${CODEHAVEN_BACKUP_OFFSITE_PASSPHRASE_FILE:-}"
[ -n "$passfile" ] || ch_die "CODEHAVEN_BACKUP_OFFSITE_PASSPHRASE_FILE is not set, so the archive cannot be encrypted before it leaves. Point it at a root-only file holding the passphrase, or leave CODEHAVEN_BACKUP_OFFSITE empty to send nothing."
[ -f "$passfile" ] || ch_die "CODEHAVEN_BACKUP_OFFSITE_PASSPHRASE_FILE=$passfile is not a regular file."
[ -s "$passfile" ] || ch_die "CODEHAVEN_BACKUP_OFFSITE_PASSPHRASE_FILE=$passfile is empty, so there is no passphrase."
mode="$(stat -c %a "$passfile" 2>/dev/null || stat -f %Lp "$passfile")"
case "$mode" in
  600|400) ;;
  *) ch_die "CODEHAVEN_BACKUP_OFFSITE_PASSPHRASE_FILE=$passfile has mode $mode: it must be 600 or 400, readable by root alone." ;;
esac

# Test hooks: name the commands to run so the tests can substitute fakes. Not for users.
CH_OFFSITE_RSYNC="${CH_OFFSITE_RSYNC:-rsync}"
CH_OFFSITE_AWS="${CH_OFFSITE_AWS:-aws}"

# Seal the archive into a scratch directory that is removed whatever happens. Only the .enc
# file is sent, never the archive itself.
name="$(basename "$archive")"
umask 077
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
enc="$tmp/$name.enc"
openssl enc -aes-256-cbc -pbkdf2 -iter 600000 -salt -pass "file:$passfile" \
  -in "$archive" -out "$enc" \
  || ch_die "openssl could not encrypt $name; nothing was sent."

case "$CODEHAVEN_BACKUP_OFFSITE" in
  rsync:*)
    "$CH_OFFSITE_RSYNC" -e "ssh -o BatchMode=yes" "$enc" "$dest" \
      || ch_die "$CH_OFFSITE_RSYNC to $dest failed; nothing was sent."
    ;;
  s3://*)
    if [ -n "${CODEHAVEN_BACKUP_OFFSITE_ENDPOINT:-}" ]; then
      "$CH_OFFSITE_AWS" s3 cp "$enc" "$dest" --endpoint-url "$CODEHAVEN_BACKUP_OFFSITE_ENDPOINT" \
        || ch_die "$CH_OFFSITE_AWS s3 cp to $dest failed; nothing was sent."
    else
      "$CH_OFFSITE_AWS" s3 cp "$enc" "$dest" \
        || ch_die "$CH_OFFSITE_AWS s3 cp to $dest failed; nothing was sent."
    fi
    ;;
esac

printf 'offsite: %s\n' "$dest$name.enc"
