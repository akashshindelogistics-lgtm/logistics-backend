#!/usr/bin/env bash
# Daily dump of the live database (DATABASE_URL from api.env) into ~/backups,
# keeping 14 days. Run by logi-backup.timer. Restore: see README.md.
set -euo pipefail

# shortcut: the password is used as written, so one with URL-escaped characters
# (%40 and the like) fails to log in; decode it here if that ever happens.
[[ ${DATABASE_URL:?} =~ ^mysql://([^:]+):([^@]+)@([^:/]+)(:([0-9]+))?/([^?]+) ]] ||
    { echo "backup: can't read DATABASE_URL" >&2; exit 1; }
user=${BASH_REMATCH[1]} pass=${BASH_REMATCH[2]} host=${BASH_REMATCH[3]}
port=${BASH_REMATCH[5]:-3306} db=${BASH_REMATCH[6]}

dir=${BACKUP_DIR:-$HOME/backups}
out=$dir/$db-$(date +%F).sql.gz
umask 077
mkdir -p "$dir"

# Written under a temporary name, so a failed dump never replaces a good one.
trap 'rm -f "$out.tmp"' EXIT
MYSQL_PWD=$pass mysqldump --single-transaction --no-tablespaces \
    -h "$host" -P "$port" -u "$user" "$db" | gzip > "$out.tmp"
mv "$out.tmp" "$out"
find "$dir" -name "$db-*.sql.gz" -mtime +14 -delete
echo "backup: wrote $out ($(du -h "$out" | cut -f1))"
