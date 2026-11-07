#!/usr/bin/env bash
# Nightly backup to OCI Object Storage through rclone's S3 remote.
# - Postgres: a custom-format pg_dump per night, kept 14 days.
# - Maps: .mmw files are content-addressed and never change, so they're
#   synced; a map deleted locally moves to `worlds-deleted/<date>/` and is
#   kept 14 days, so every kept dump can still find its maps.
# Configure the `oci` remote in /etc/minimodes/rclone.conf (see README).
set -euo pipefail

REMOTE=${MINIMODES_BACKUP_REMOTE:-oci:minimodes-backups}
DATA=${MINIMODES_DATA_DIR:-/var/lib/minimodes}
KEEP=${MINIMODES_BACKUP_KEEP:-14d}
export RCLONE_CONFIG=${RCLONE_CONFIG:-/etc/minimodes/rclone.conf}
today=$(date -u +%Y-%m-%d)
dump="$DATA/backup/minimodes-$today.dump"

mkdir -p "$DATA/backup"
trap 'rm -f "$dump"' EXIT

echo "dumping Postgres"
pg_dump --format=custom --compress=6 --file="$dump" "${MINIMODES_BACKUP_DATABASE:-minimodes}"
rclone copyto "$dump" "$REMOTE/db/minimodes-$today.dump"

echo "syncing maps"
rclone sync "$DATA/worlds" "$REMOTE/worlds" --backup-dir "$REMOTE/worlds-deleted/$today" --fast-list

echo "pruning backups older than $KEEP"
rclone delete "$REMOTE/db" --min-age "$KEEP"
rclone delete "$REMOTE/worlds-deleted" --min-age "$KEEP" --rmdirs

echo "backup $today done"
