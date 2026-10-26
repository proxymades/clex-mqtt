#!/usr/bin/env python3
"""Check backup status collection without contacting production services."""
from datetime import datetime, timezone
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "backup_status", ROOT / "deploy/backup/clex-mqtt-backup-status.py"
)
status = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(status)


class StatusTests(unittest.TestCase):
    def setUp(self):
        cache = ROOT / ".cache" / "tmp"
        cache.mkdir(parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(prefix="backup-status-", dir=cache)
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.base = self.root / "backups"
        self.base.mkdir()
        self.output = self.root / "status.prom"
        self.config = self.root / "config.json"
        self.config.write_text(json.dumps({"destination": str(self.base)}))
        self.config.chmod(0o600)
        self.now = 1800000000.0
        self.healthy = dict(
            LoadState="loaded", Result="success", ExecMainStatus="0", ActiveState="inactive"
        )

    def snapshot(self, stamp, suffix="aaaaaaaa"):
        name = datetime.fromtimestamp(stamp, timezone.utc).strftime(
            "backup-%Y%m%dT%H%M%S.%fZ-"
        ) + suffix
        path = self.base / name
        path.mkdir()
        (path / "manifest.json").write_text(json.dumps({
            "version": 1, "created_at": datetime.fromtimestamp(stamp, timezone.utc).isoformat()
        }))
        return path

    def collect(self, state=None):
        return status.collect(
            self.config, self.output, self.now,
            lambda: self.healthy if state is None else state
        )

    def test_only_completed_real_directories_count_and_newest_timestamp_wins(self):
        self.snapshot(self.now - 1000)
        latest = self.snapshot(self.now - 100, "bbbbbbbb")
        (self.base / ".pending-unused").mkdir()
        (self.base / latest.name.replace("bbbbbbbb", "cccccccc")).symlink_to(latest)
        values = self.collect()
        self.assertEqual(values["completed_copies"], 2)
        self.assertEqual(values["last_success_timestamp_seconds"], self.now - 100)
        self.assertEqual(values["status_collection_success"], 1)

    def test_empty_destination_reports_zero_timestamp_for_stale_rule(self):
        values = self.collect()
        self.assertEqual(values["completed_copies"], 0)
        self.assertEqual(values["last_success_timestamp_seconds"], 0)

    def test_failed_attempt_survives_retry_until_successful_completion(self):
        self.snapshot(self.now - 1000)
        failed = self.collect(self.healthy | {"Result": "exit-code", "ExecMainStatus": "1",
                                            "ActiveState": "activating"})
        self.assertEqual(failed["last_run_success"], 0)
        status.write_metrics(self.output, failed)
        retry = self.collect(self.healthy | {"ActiveState": "activating"})
        self.assertEqual(retry["last_run_success"], 0)
        status.write_metrics(self.output, retry)
        self.assertEqual(self.collect()["last_run_success"], 1)

    def test_first_running_attempt_keeps_existing_successful_copy_status(self):
        self.snapshot(self.now - 1000)
        self.assertEqual(
            self.collect(self.healthy | {"ActiveState": "activating"})["last_run_success"], 1
        )

    def test_missing_unit_reports_collection_failure(self):
        self.assertEqual(self.collect({"LoadState": "not-found"})["status_collection_success"], 0)

    def test_corrupt_manifest_preserves_previous_values_and_marks_collection_failure(self):
        path = self.snapshot(self.now - 1000)
        old = self.collect()
        status.write_metrics(self.output, old)
        (path / "manifest.json").write_text("invalid")
        values = self.collect()
        self.assertEqual(values["last_success_timestamp_seconds"], self.now - 1000)
        self.assertEqual(values["status_collection_success"], 0)
        self.assertEqual(values["status_timestamp_seconds"], self.now)

    def test_future_timestamp_is_not_accepted_as_fresh_backup(self):
        self.snapshot(self.now + 600)
        self.assertEqual(self.collect()["status_collection_success"], 0)

    def test_systemctl_timeout_is_reported_without_private_diagnostics(self):
        self.snapshot(self.now - 1000)
        def failure():
            raise subprocess.TimeoutExpired("fixture-private-message", 10)
        values = status.collect(self.config, self.output, self.now, failure)
        self.assertEqual(values["status_collection_success"], 0)
        status.write_metrics(self.output, values)
        self.assertNotIn("fixture-private-message", self.output.read_text())

    def test_atomic_output_is_readable_and_contains_only_fixed_numeric_metrics(self):
        status.write_metrics(self.output, self.collect())
        self.assertEqual(self.output.stat().st_mode & 0o777, 0o644)
        self.assertFalse(list(self.root.glob(".backup-status-*")))
        for line in self.output.read_text().splitlines():
            if not line.startswith("#"):
                name, value = line.split()
                self.assertTrue(name.startswith("clex_mqtt_backup_"))
                float(value)

    def test_writable_configuration_is_rejected(self):
        self.config.chmod(0o666)
        self.assertEqual(self.collect()["status_collection_success"], 0)

    @unittest.skipUnless(os.environ.get("PROMTOOL") or shutil.which("promtool"),
                         "promtool not installed; installer runs rule tests before installation")
    def test_prometheus_alert_timing_and_missing_series(self):
        tool = os.environ.get("PROMTOOL") or shutil.which("promtool")
        subprocess.run(
            [tool, "test", "rules", "backup-alerts.test.yml"],
            cwd=ROOT / "deploy/prometheus", check=True
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
