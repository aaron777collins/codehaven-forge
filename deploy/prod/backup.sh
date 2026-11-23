#!/usr/bin/env bash
# Takes one backup of the instance and prints the path of the archive. Run it as root.
#   sudo bash deploy/prod/backup.sh
#
# The archive (mode 0600, written under CODEHAVEN_BACKUP_DIR, which must be outside the checkout)
# holds everything needed to bring the instance back on a new host: a pg_dump of the database, the
# git repositories, blobs and logs, the server's master key (data/secrets), the database
# credentials, and the env file. That makes it as sensitive as the instance itself: keep it
# somewhere only you can read, and encrypt it before it leaves the host. SHA256SUMS in the archive
# lists a checksum for every other member, and the archive is unpacked and checked against it
# before the script reports success. This script is the supported backup command: a
# `codehaven admin backup` would need the same root and Docker access and would add nothing.
#
# The server is stopped for the time the files are copied, so the database and the repositories
# match, and is started again by a trap whatever happens. Nothing else is stopped.
set -euo pipefail
CH_SCRIPT=backup
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
ch_env_load
ch_need CODEHAVEN_PROJECT CODEHAVEN_DATA_DIR CODEHAVEN_SECRETS_DIR CODEHAVEN_BACKUP_DIR
dir="$(ch_checkout)"
[ "$(id -u)" = 0 ] || ch_die "run this as root (sudo): the repositories belong to the server's user."
mkdir -p "$CODEHAVEN_BACKUP_DIR"; chmod 700 "$CODEHAVEN_BACKUP_DIR"
case "$(cd "$CODEHAVEN_BACKUP_DIR" && pwd)/" in "$dir"/*) ch_die "CODEHAVEN_BACKUP_DIR is inside the checkout." ;; esac
C="$dir/deploy/compose.sh"
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
stage="$CODEHAVEN_BACKUP_DIR/.staging-$stamp"
verify=""
archive="$CODEHAVEN_BACKUP_DIR/codehaven-$CODEHAVEN_PROJECT-$stamp.tar.gz"
umask 077
mkdir -p "$stage"
server_was_up=0
bash "$C" ps --status running --format '{{.Service}}' | grep -qx server && server_was_up=1
cleanup() {
  rc=$?
  set +e
  [ "$server_was_up" = 0 ] || bash "$C" start server >/dev/null 2>&1
  rm -rf "$stage" "${verify:-}"
  [ "$rc" = 0 ] || { rm -f "$archive"; echo "backup: failed, nothing was kept" >&2; }
  exit "$rc"
}
trap cleanup EXIT

bash "$C" ps --status running --format '{{.Service}}' | grep -qx postgres || ch_die "postgres is not running, so there is nothing to dump. Start the instance first."
[ "$server_was_up" = 0 ] || bash "$C" stop server >/dev/null
bash "$C" exec -T postgres pg_dump -U codehaven -d codehaven --format=custom > "$stage/pg.dump"
[ -s "$stage/pg.dump" ] || ch_die "pg_dump wrote nothing"
tar -C "$CODEHAVEN_DATA_DIR" -cf "$stage/data.tar" git blobs logs secrets
tar -C "$CODEHAVEN_SECRETS_DIR" -cf "$stage/compose-secrets.tar" .
cp "$CH_ENV_LOADED" "$stage/codehaven.env"
printf 'project=%s\ntag=%s\ncreated=%s\n' "$CODEHAVEN_PROJECT" "${CODEHAVEN_TAG:-unknown}" "$stamp" > "$stage/MANIFEST"
(cd "$stage" && sha256sum MANIFEST pg.dump data.tar compose-secrets.tar codehaven.env > SHA256SUMS)
tar -C "$stage" -czf "$archive" MANIFEST pg.dump data.tar compose-secrets.tar codehaven.env SHA256SUMS
chmod 600 "$archive"
# Check the finished archive against its checksums in a scratch copy beside it (same disk, mode
# 0700, removed by the trap if anything fails).
verify="$(mktemp -d "$CODEHAVEN_BACKUP_DIR/.verify-XXXXXX")"
tar -xzf "$archive" -C "$verify"
(cd "$verify" && sha256sum --quiet -c SHA256SUMS) || { rm -rf "$verify"; ch_die "the archive does not match its own checksums"; }
rm -rf "$verify"
if [ "$server_was_up" = 1 ]; then bash "$C" start server >/dev/null; server_was_up=0; fi
echo "$archive"
