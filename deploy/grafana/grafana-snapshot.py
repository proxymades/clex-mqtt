#!/usr/bin/env python3
"""Create a consistent private Grafana SQLite snapshot before the main backup."""
import os
from pathlib import Path
import sqlite3
import tempfile


def main():
    os.umask(0o077)
    source = Path('/var/lib/grafana/grafana.db')
    destination = Path('/var/lib/clex-mqtt-grafana-backup/grafana.db')
    if not source.is_file() or source.is_symlink():
        raise RuntimeError('Grafana database is missing or is not a regular file')
    destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    if destination.parent.is_symlink():
        raise RuntimeError('Grafana snapshot directory must not be a symlink')
    fd, temporary = tempfile.mkstemp(prefix='.grafana-', suffix='.db', dir=destination.parent)
    os.close(fd)
    try:
        with sqlite3.connect(source.as_uri() + '?mode=ro', uri=True, timeout=30) as live:
            with sqlite3.connect(temporary, timeout=30) as copy:
                live.backup(copy, pages=256, sleep=0.05)
                if copy.execute('PRAGMA integrity_check').fetchall() != [('ok',)]:
                    raise RuntimeError('Grafana snapshot integrity check failed')
        os.chmod(temporary, 0o600)
        with open(temporary, 'rb') as handle:
            os.fsync(handle.fileno())
        os.replace(temporary, destination)
        directory = os.open(destination.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        Path(temporary).unlink(missing_ok=True)
    print('Grafana snapshot: OK')


if __name__ == '__main__':
    main()
