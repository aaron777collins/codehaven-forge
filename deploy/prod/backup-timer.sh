#!/usr/bin/env bash
# Installs, removes or shows the status of the systemd timer that runs backup-daily.sh once
# a day: 04:17 local time, up to 10 minutes later at random, and caught up after the host
# was off.
#   sudo bash deploy/prod/backup-timer.sh install|remove|status
#
# The timer runs a copy of deploy/ taken at install time, kept in
# $CODEHAVEN_BUILD_ROOT/backup-tool/deploy, so a checkout that is being synced or edited is
# never what runs at night. Run install again after updating the checkout to refresh the
# copy. remove takes the timer away and keeps every archive already taken.
set -euo pipefail
CH_SCRIPT=backup-timer
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
ch_env_load
ch_need CODEHAVEN_PROJECT CODEHAVEN_BUILD_ROOT CODEHAVEN_BACKUP_DIR
dir="$(ch_checkout)"
[ "$(id -u)" = 0 ] || ch_die "run this as root (sudo)."
unit="codehaven-backup-$CODEHAVEN_PROJECT"
tool="$CODEHAVEN_BUILD_ROOT/backup-tool"
# The service must reach the env file whatever the checkout looks like when it fires.
env_file="$(cd "$(dirname "$CH_ENV_LOADED")" && pwd)/$(basename "$CH_ENV_LOADED")"
case "${1:-}" in
  install)
    rm -rf "$tool.new"; mkdir -p "$tool.new"; cp -a "$dir/deploy" "$tool.new/deploy"
    rm -rf "$tool"; mv "$tool.new" "$tool"; chmod -R go-w "$tool"
    cat > "/etc/systemd/system/$unit.service" <<EOF
[Unit]
Description=Codehaven daily backup ($CODEHAVEN_PROJECT)
[Service]
Type=oneshot
Environment=CH_ENV_FILE=$env_file
ExecStart=/bin/bash $tool/deploy/prod/backup-daily.sh
Nice=10
IOSchedulingClass=idle
EOF
    cat > "/etc/systemd/system/$unit.timer" <<EOF
[Unit]
Description=Codehaven daily backup ($CODEHAVEN_PROJECT)
[Timer]
OnCalendar=*-*-* 04:17:00
RandomizedDelaySec=600
Persistent=true
[Install]
WantedBy=timers.target
EOF
    systemctl daemon-reload; systemctl enable --now "$unit.timer"
    echo "backup-timer: installed $unit.timer. Run one now with: systemctl start $unit.service"
    ;;
  remove)
    systemctl disable --now "$unit.timer" 2>/dev/null || true
    rm -f "/etc/systemd/system/$unit.service" "/etc/systemd/system/$unit.timer"
    systemctl daemon-reload
    rm -rf "$tool"
    echo "backup-timer: removed $unit.timer. Backups already taken are kept."
    ;;
  status)
    systemctl list-timers "$unit.timer" --all
    tail -n 20 "$CODEHAVEN_BACKUP_DIR/backup-daily.log" 2>/dev/null || echo "no log yet"
    ;;
  *)
    echo "usage: backup-timer.sh install|remove|status" >&2
    exit 2
    ;;
esac
