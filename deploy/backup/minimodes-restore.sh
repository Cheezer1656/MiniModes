#!/usr/bin/env bash
# Restores a nightly backup onto a stopped network.
# Usage: minimodes-restore.sh YYYY-MM-DD
# Stop everything first: systemctl stop minimodes.target
set -euo pipefail

DATE=${1:?usage: minimodes-restore.sh YYYY-MM-DD}
REMOTE=${MINIMODES_BACKUP_REMOTE:-oci:minimodes-backups}
DATA=${MINIMODES_DATA_DIR:-/var/lib/minimodes}
DB=${MINIMODES_BACKUP_DATABASE:-minimodes}
export RCLONE_CONFIG=${RCLONE_CONFIG:-/etc/minimodes/rclone.conf}

if systemctl is-active --quiet minimodes-lobby.service; then
	echo "stop the network first: systemctl stop minimodes.target" >&2
	exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
rclone copyto "$REMOTE/db/minimodes-$DATE.dump" "$work/db.dump"

echo "restoring Postgres into $DB (replacing it)"
pg_restore --clean --if-exists --no-owner --dbname="$DB" "$work/db.dump"

echo "restoring maps (current ones, plus any deleted since $DATE)"
rclone copy "$REMOTE/worlds" "$DATA/worlds"
mkdir -p "$work/deleted"
# worlds-deleted only exists once a map has been deleted since the first backup.
if rclone lsf --max-depth 1 "$REMOTE/worlds-deleted" 2>/dev/null | grep -q .; then
	rclone copy "$REMOTE/worlds-deleted" "$work/deleted"
fi
find "$work/deleted" -name '*.mmw' -exec cp -n {} "$DATA/worlds/" \;
chown -R minimodes:minimodes "$DATA/worlds"

echo "restored $DATE; start with: systemctl start minimodes.target"
