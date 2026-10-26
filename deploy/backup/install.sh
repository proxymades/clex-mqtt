#!/usr/bin/env bash
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo 'Run this installer as root.' >&2
    exit 1
fi

clex_source_root="$(cd -- "$(dirname -- "$0")/../.." && pwd)"
for clex_command in python3 systemctl systemd-analyze install sha256sum; do
    command -v "$clex_command" >/dev/null
done
if [ -f "$clex_source_root/SHA256SUMS" ]; then
    (cd "$clex_source_root" && sha256sum --check --quiet SHA256SUMS)
fi
test -x /usr/lib/postgresql/17/bin/pg_dump
test -x /usr/lib/postgresql/17/bin/pg_restore
test -x /usr/lib/postgresql/17/bin/psql
test -r /etc/clex-mqtt/clex-mqtt.env
test -x /opt/clex-mqtt/clex-mqtt
test -r /etc/systemd/system/clex-mqtt.service
test -r /etc/nginx/sites-available/clex-mqtt
systemctl is-active --quiet postgresql@17-main.service

install -d -o root -g root -m 0700 /var/backups/clex-mqtt
install -m 0755 "$clex_source_root/deploy/backup/clex-mqtt-backup.py" /usr/local/sbin/clex-mqtt-backup
if [ ! -e /etc/clex-mqtt/backup.json ] && [ ! -L /etc/clex-mqtt/backup.json ]; then
    install -m 0600 "$clex_source_root/deploy/backup/backup.example.json" /etc/clex-mqtt/backup.json
fi
install -m 0644 "$clex_source_root/deploy/systemd/clex-mqtt-backup.service" /etc/systemd/system/
install -m 0644 "$clex_source_root/deploy/systemd/clex-mqtt-backup.timer" /etc/systemd/system/
install -m 0644 "$clex_source_root/docs/backups.md" /opt/clex-mqtt/docs/backups.md
install -m 0644 "$clex_source_root/docs/deployment.md" /opt/clex-mqtt/docs/deployment.md

systemd-analyze verify /etc/systemd/system/clex-mqtt-backup.service /etc/systemd/system/clex-mqtt-backup.timer
systemctl daemon-reload
systemctl start clex-mqtt-backup.service
systemctl enable --now clex-mqtt-backup.timer
systemctl list-timers clex-mqtt-backup.timer --no-pager
/usr/local/sbin/clex-mqtt-backup list
echo 'Backup installation: OK'
