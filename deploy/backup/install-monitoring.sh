#!/usr/bin/env bash
set -euo pipefail

[ "$(id -u)" -eq 0 ] || { echo 'Run this installer as root.' >&2; exit 1; }
clex_source_root="$(cd -- "$(dirname -- "$0")/../.." && pwd)"
for clex_command in python3 install systemctl systemd-analyze promtool; do
    command -v "$clex_command" >/dev/null
done
if [ -f "$clex_source_root/SHA256SUMS" ]; then
    (cd "$clex_source_root" && sha256sum --check --quiet SHA256SUMS)
fi
promtool check rules "$clex_source_root/deploy/prometheus/backup-alerts.yml"
(cd "$clex_source_root/deploy/prometheus" && promtool test rules backup-alerts.test.yml)
test -r /etc/clex-mqtt/backup.json
test -r /etc/prometheus/prometheus.yml
grep -Fq -- '--collector.textfile.directory=/var/lib/prometheus/node-exporter/clex-mqtt' \
    /etc/default/prometheus-node-exporter || {
    echo 'Configure the Node Exporter textfile directory before installation.' >&2
    exit 1
}
systemctl is-active --quiet prometheus.service prometheus-node-exporter.service

install -d -o root -g root -m 0755 /var/lib/prometheus/node-exporter/clex-mqtt
install -m 0755 "$clex_source_root/deploy/backup/clex-mqtt-backup-status.py" /usr/local/sbin/clex-mqtt-backup-status
install -m 0644 "$clex_source_root/deploy/systemd/clex-mqtt-backup-status.service" /etc/systemd/system/
install -m 0644 "$clex_source_root/deploy/systemd/clex-mqtt-backup-status.timer" /etc/systemd/system/
install -m 0644 "$clex_source_root/deploy/prometheus/backup-alerts.yml" /etc/prometheus/
install -d -m 0755 /opt/clex-mqtt/docs
install -m 0644 "$clex_source_root/docs/monitoring.md" /opt/clex-mqtt/docs/
install -m 0644 "$clex_source_root/docs/backups.md" /opt/clex-mqtt/docs/
systemd-analyze verify /etc/systemd/system/clex-mqtt-backup-status.service /etc/systemd/system/clex-mqtt-backup-status.timer
systemctl daemon-reload
systemctl start clex-mqtt-backup-status.service
systemctl enable --now clex-mqtt-backup-status.timer

python3 - <<'PY'
from pathlib import Path
import subprocess

path = Path("/etc/prometheus/prometheus.yml")
old = path.read_bytes()
text = old.decode()
rule = "/etc/prometheus/backup-alerts.yml"
if rule not in text:
    assert text.count("rule_files:\n") == 1, "Expected one rule_files section"
    text = text.replace("rule_files:\n", "rule_files:\n  - " + rule + "\n", 1)
backup = path.with_name(path.name + ".before-backup-monitoring")
if not backup.exists():
    backup.write_bytes(old)
    backup.chmod(0o640)
path.write_text(text)
result = subprocess.run(["promtool", "check", "config", str(path)])
if result.returncode:
    path.write_bytes(old)
    raise SystemExit(result.returncode)
result = subprocess.run(["systemctl", "reload", "prometheus.service"])
if result.returncode:
    path.write_bytes(old)
    subprocess.run(["systemctl", "reload", "prometheus.service"], check=False)
    raise SystemExit(result.returncode)
PY
cat /var/lib/prometheus/node-exporter/clex-mqtt/clex-mqtt-backup.prom
systemctl list-timers clex-mqtt-backup-status.timer --no-pager --full
echo 'Backup monitoring: OK'
