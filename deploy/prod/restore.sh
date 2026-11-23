#!/usr/bin/env bash
# Restores a backup made by backup.sh into an EMPTY instance. Run it as root.
#   sudo bash deploy/prod/restore.sh <archive>
#
# It refuses when the project is running, or when the data directories, the secrets directory or
# the database directory already hold anything. It never overwrites and never deletes: restore into
# a new, empty location (a new CODEHAVEN_PROJECT with its own CODEHAVEN_DATA_DIR and
# CODEHAVEN_SECRETS_DIR in the env file), then run deploy/prod/up.sh. The env file of the backup is
# unpacked beside the archive as codehaven.env.restored for you to compare; it is not applied.
# It checks the archive against its SHA256SUMS first and stops on any mismatch, before anything is written.
set -euo pipefail
CH_SCRIPT=restore
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
ch_env_load
ch_need CODEHAVEN_PROJECT CODEHAVEN_DATA_DIR CODEHAVEN_SECRETS_DIR
archive="${1:?usage: restore.sh <archive>}"
[ -f "$archive" ] || ch_die "no such archive: $archive"
[ "$(id -u)" = 0 ] || ch_die "run this as root (sudo)."
dir="$(ch_checkout)"
C="$dir/deploy/compose.sh"
uid=10001

if [ -n "$(docker ps -aq --filter "label=com.docker.compose.project=$CODEHAVEN_PROJECT")" ]; then
  ch_die "project '$CODEHAVEN_PROJECT' already has containers. Restore into a new, empty project."
fi
for d in "$CODEHAVEN_DATA_DIR"/postgres "$CODEHAVEN_DATA_DIR"/git "$CODEHAVEN_DATA_DIR"/blobs "$CODEHAVEN_DATA_DIR"/logs "$CODEHAVEN_DATA_DIR"/secrets "$CODEHAVEN_SECRETS_DIR"; do
  if [ -d "$d" ] && [ -n "$(ls -A "$d" 2>/dev/null)" ]; then ch_die "$d is not empty. Restore into an empty location."; fi
done

work="$(mktemp -d)"; chmod 700 "$work"
trap 'rm -rf "$work"' EXIT
tar -xzf "$archive" -C "$work"
for f in MANIFEST pg.dump data.tar compose-secrets.tar codehaven.env; do [ -e "$work/$f" ] || ch_die "the archive has no $f. Not a backup from backup.sh."; done

if [ -f "$work/SHA256SUMS" ]; then
  (cd "$work" && sha256sum --quiet -c SHA256SUMS) || ch_die "the archive does not match its checksums (SHA256SUMS). It is damaged; nothing was restored."
else
  echo "restore: this archive has no SHA256SUMS (made before checksums were added), so it cannot be checked." >&2
fi

mkdir -p "$CODEHAVEN_DATA_DIR"/{postgres,git,blobs,logs,secrets,dind} "$CODEHAVEN_SECRETS_DIR"
tar -C "$CODEHAVEN_DATA_DIR" -xf "$work/data.tar"
chown -R "$uid:$uid" "$CODEHAVEN_DATA_DIR"/{git,blobs,logs,secrets}
chmod 700 "$CODEHAVEN_DATA_DIR/secrets"
tar -C "$CODEHAVEN_SECRETS_DIR" -xf "$work/compose-secrets.tar"
chown root:"$uid" "$CODEHAVEN_SECRETS_DIR"; chmod 750 "$CODEHAVEN_SECRETS_DIR"
chown root:"$uid" "$CODEHAVEN_SECRETS_DIR"/*; chmod 440 "$CODEHAVEN_SECRETS_DIR"/*
cp "$work/codehaven.env" "$(dirname "$archive")/codehaven.env.restored"; chmod 600 "$(dirname "$archive")/codehaven.env.restored"

# An empty database first, with the restored credentials, then the dump into it.
bash "$C" up -d postgres
for _ in $(seq 1 40); do
  [ "$(bash "$C" ps --format '{{.Service}} {{.Health}}' | grep '^postgres ' | cut -d' ' -f2)" = healthy ] && break
  sleep 2
done
bash "$C" exec -T postgres pg_restore -U codehaven -d codehaven --no-owner --exit-on-error < "$work/pg.dump"
bash "$C" stop postgres >/dev/null
echo "restore: done. The instance is not started. Review $(dirname "$archive")/codehaven.env.restored, then run deploy/prod/up.sh"
