#!/usr/bin/env python3
"""Export local backup metadata and service results for Node Exporter."""
from __future__ import annotations

import argparse
from datetime import datetime
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile
import time

PREFIX = "clex_mqtt_backup_"
FINAL = re.compile(r"backup-\d{8}T\d{6}\.\d{6}Z-[0-9a-f]{8}")
NAMES = ("last_success_timestamp_seconds", "completed_copies", "last_run_success")


def previous_metrics(path: Path) -> dict[str, float]:
    values = dict.fromkeys(NAMES, 0.0)
    try:
        for line in path.read_text().splitlines():
            fields = line.split()
            if len(fields) == 2 and fields[0].startswith(PREFIX):
                name = fields[0][len(PREFIX):]
                if name in values and re.fullmatch(r"[0-9]+(?:\.[0-9]+)?", fields[1]):
                    values[name] = float(fields[1])
    except FileNotFoundError:
        pass
    return values


def snapshot_status(base: Path, now: float) -> tuple[int, float]:
    count, latest = 0, 0.0
    for path in base.iterdir():
        if not FINAL.fullmatch(path.name) or path.is_symlink() or not path.is_dir():
            continue
        try:
            manifest_path = path / "manifest.json"
            if manifest_path.is_symlink():
                raise ValueError("Invalid manifest")
            manifest = json.loads(manifest_path.read_text())
            created = datetime.fromisoformat(manifest["created_at"])
            if created.tzinfo is None:
                raise ValueError("Missing timestamp timezone")
            stamp = created.timestamp()
            if not 0 < stamp <= now + 300:
                raise ValueError("Invalid backup timestamp")
            if manifest.get("version") != 1:
                raise ValueError("Unknown manifest version")
            count += 1
            latest = max(latest, stamp)
        except FileNotFoundError:
            # Rotation can remove an older complete directory during collection.
            continue
    return count, latest


def run_succeeded(state: dict[str, str], previous: float) -> float:
    if state.get("LoadState") != "loaded":
        raise ValueError("Backup service is not loaded")
    if state["Result"] != "success" or state["ExecMainStatus"] != "0":
        return 0.0
    if state["ActiveState"] in {"activating", "active", "deactivating"}:
        # A retry is not a successful completion until the oneshot exits.
        return previous
    return 1.0


def collect(config: Path, output: Path, now: float, state_reader) -> dict[str, float]:
    values = previous_metrics(output)
    values.update(status_timestamp_seconds=now, status_collection_success=0.0)
    phase = "configuration"
    try:
        info = config.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o022:
            raise ValueError("Unsafe backup configuration")
        settings = json.loads(config.read_text())
        base = Path(settings["destination"])
        if not base.is_absolute():
            raise ValueError("Backup destination must be absolute")
        phase = "backup metadata"
        count, latest = snapshot_status(base, now)
        phase = "service state"
        succeeded = run_succeeded(
            state_reader(), values["last_run_success"] if output.exists() else float(count > 0)
        )
        values.update(
            completed_copies=float(count), last_success_timestamp_seconds=latest,
            last_run_success=succeeded, status_collection_success=1.0,
        )
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print(f"Backup status: {phase} failed ({type(error).__name__})", file=sys.stderr)
        # Keep the last known values; never print configuration or subprocess output.
        pass
    return values


def service_state() -> dict[str, str]:
    result = subprocess.run(
        ["/usr/bin/systemctl", "show", "clex-mqtt-backup.service",
         "--property=LoadState", "--property=Result",
         "--property=ExecMainStatus", "--property=ActiveState", "--no-pager"],
        capture_output=True, text=True, check=True, timeout=10,
    )
    return dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)


def write_metrics(output: Path, values: dict[str, float]) -> None:
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w", prefix=".backup-status-", suffix=".tmp",
            dir=output.parent, delete=False,
        ) as stream:
            temporary = Path(stream.name)
            for name, value in sorted(values.items()):
                stream.write(f"# TYPE {PREFIX}{name} gauge\n{PREFIX}{name} {value:.6f}\n")
            stream.flush()
            os.fsync(stream.fileno())
            os.fchmod(stream.fileno(), 0o644)
        os.replace(temporary, output)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=Path("/etc/clex-mqtt/backup.json"))
    parser.add_argument("--output", type=Path, default=Path(
        "/var/lib/prometheus/node-exporter/clex-mqtt/clex-mqtt-backup.prom"
    ))
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("Run the installed collector as root.")
    os.umask(0o077)
    values = collect(args.config, args.output, time.time(), service_state)
    write_metrics(args.output, values)
    ok = bool(values["status_collection_success"])
    print("Backup status: " + ("OK" if ok else "collection failed"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
