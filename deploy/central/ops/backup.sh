#!/bin/sh
# Daily database backup: pg_dump (custom format) into BACKUP_DIR, keeping the last 7 days.
#   ops/backup.sh COMPOSE_DIR BACKUP_DIR
set -eu
[ $# -eq 2 ] || { echo "usage: $0 COMPOSE_DIR BACKUP_DIR" >&2; exit 2; }
umask 077
mkdir -p "$2"
out="$2/redex-$(date -u +%Y%m%dT%H%M%SZ).dump"
cd "$1"
docker compose exec -T postgres pg_dump -U redex -d redex -Fc > "$out.tmp"
[ -s "$out.tmp" ] || { rm -f "$out.tmp"; echo "backup: empty dump" >&2; exit 1; }
mv "$out.tmp" "$out"
find "$2" -maxdepth 1 -name 'redex-*.dump' -mtime +6 -delete
echo "backup: $out ($(du -h "$out" | cut -f1))"
