#!/usr/bin/env bash
# What the backup timer runs (deploy/prod/backup-timer.sh installs it): one backup with
# deploy/prod/backup.sh, then backup-prune.py over the archive directory. Everything it has
# to say is printed and appended to $CODEHAVEN_BACKUP_DIR/backup-daily.log.
#
# Settings, all optional, from the environment or the env file:
#   CODEHAVEN_BACKUP_KEEP_DAILY   distinct days whose newest archive is kept (default 7)
#   CODEHAVEN_BACKUP_KEEP_WEEKLY  distinct ISO weeks whose newest archive is kept (default 4)
#   CODEHAVEN_BACKUP_MIN_FREE_MB  free MB the backup disk must have at least (default 2048)
#   CH_BACKUP_CMD                 the backup command (default: backup.sh in this checkout)
#   CODEHAVEN_BACKUP_OFFSITE      where an encrypted copy of each archive is sent (empty: nowhere)
#   CODEHAVEN_BACKUP_OFFSITE_PASSPHRASE_FILE  the root-only passphrase file that copy needs
#
# Nothing runs when the disk behind CODEHAVEN_BACKUP_DIR is too full, and nothing is pruned
# after a backup that failed.
set -euo pipefail
CH_SCRIPT=backup-daily
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
ch_env_load
ch_need CODEHAVEN_PROJECT CODEHAVEN_BACKUP_DIR
dir="$(ch_checkout)"
[ "$(id -u)" = 0 ] || ch_die "run this as root (sudo)."
mkdir -p "$CODEHAVEN_BACKUP_DIR"; chmod 700 "$CODEHAVEN_BACKUP_DIR"
log="$CODEHAVEN_BACKUP_DIR/backup-daily.log"
say() { local line; line="$(date -u +%Y-%m-%dT%H:%M:%SZ) $*"; printf '%s\n' "$line"; printf '%s\n' "$line" >> "$log"; }

keep_daily="${CODEHAVEN_BACKUP_KEEP_DAILY:-7}"
keep_weekly="${CODEHAVEN_BACKUP_KEEP_WEEKLY:-4}"
min_free_mb="${CODEHAVEN_BACKUP_MIN_FREE_MB:-2048}"
CH_BACKUP_CMD="${CH_BACKUP_CMD:-bash $(printf %q "$dir/deploy/prod/backup.sh")}"
errfile="$(mktemp)"
trap 'rm -f "$errfile"' EXIT

# Disk guard: the backup needs the staging copy (uncompressed), the archive and room to spare.
# So the largest of: CODEHAVEN_BACKUP_MIN_FREE_MB, three times the newest archive, and twice the
# data it copies (git, blobs, logs, secrets under CODEHAVEN_DATA_DIR, when that is set).
free_kb="$(df -Pk "$CODEHAVEN_BACKUP_DIR" | awk 'NR==2 {print $4}')"
newest="$(ls -1t "$CODEHAVEN_BACKUP_DIR"/codehaven-"$CODEHAVEN_PROJECT"-*.tar.gz 2>/dev/null | head -n 1 || true)"
newest_kb=0
if [ -n "$newest" ]; then newest_kb="$(du -k "$newest" | cut -f1)"; fi
need_kb=$((min_free_mb * 1024))
if [ $((newest_kb * 3)) -gt "$need_kb" ]; then need_kb=$((newest_kb * 3)); fi
if [ -n "${CODEHAVEN_DATA_DIR:-}" ] && [ -d "$CODEHAVEN_DATA_DIR" ]; then
  data_kb="$(du -sck "$CODEHAVEN_DATA_DIR"/git "$CODEHAVEN_DATA_DIR"/blobs "$CODEHAVEN_DATA_DIR"/logs "$CODEHAVEN_DATA_DIR"/secrets 2>/dev/null | awk 'END {print $1}')"
  if [ $(( ${data_kb:-0} * 2 )) -gt "$need_kb" ]; then need_kb=$(( data_kb * 2 )); fi
fi
if [ "$free_kb" -lt "$need_kb" ]; then
  say "SKIPPED: only $((free_kb / 1024)) MB free on the backup disk, need $((need_kb / 1024)) MB. No backup was taken."
  exit 0
fi

# The backup: its last stdout line is the archive path. With pipefail the pipeline fails
# when the backup fails, and a path that is not a file counts as a failure too.
if out_path="$(eval "$CH_BACKUP_CMD" 2>"$errfile" | tail -n 1)" && [ -n "$out_path" ] && [ -f "$out_path" ]; then
  say "OK: $out_path ($(du -k "$out_path" | cut -f1) KB)"
else
  say "FAILED: the backup did not finish. Last lines:"
  tail -n 5 "$errfile" | while IFS= read -r line; do say "$line"; done
  exit 1
fi

# Prune, and say what went. A prune that fails ends the run: keeping everything is safe,
# silently losing the schedule is not.
pruned="$(python3 "$dir/deploy/prod/backup-prune.py" \
  --dir "$CODEHAVEN_BACKUP_DIR" --project "$CODEHAVEN_PROJECT" \
  --keep-daily "$keep_daily" --keep-weekly "$keep_weekly" --apply)" \
  || { say "FAILED: pruning"; exit 1; }
for name in $pruned; do
  say "pruned: $name"
done

# The optional copy off the host (deploy/prod/backup-offsite.sh): nothing runs, and nothing
# lands in the log, unless CODEHAVEN_BACKUP_OFFSITE names a destination. It runs after the
# archive and the prune, so a failed send never costs the local backup.
if [ -n "${CODEHAVEN_BACKUP_OFFSITE:-}" ]; then
  if offsite_out="$(bash "$dir/deploy/prod/backup-offsite.sh" "$out_path" 2>&1)"; then
    say "OFFSITE OK: $(printf '%s\n' "$offsite_out" | tail -n 1)"
  else
    say "OFFSITE FAILED: the local backup is fine, the copy off the host was not made. Last lines:"
    printf '%s\n' "$offsite_out" | tail -n 5 | while IFS= read -r line; do say "$line"; done
    exit 1
  fi
fi

exit 0
