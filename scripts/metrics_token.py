#!/usr/bin/env python3
"""Prepare a private token for the local Prometheus endpoint."""
import os
from pathlib import Path
import secrets
import sys

path = Path(__file__).resolve().parents[1] / ".env"
if path.is_symlink() or not path.is_file():
    sys.exit("Prepare a regular private .env first.")
content = path.read_text()
if any(line.strip().startswith(("CLEX_METRICS_TOKEN=", "export CLEX_METRICS_TOKEN=")) for line in content.splitlines()):
    print("CLEX_METRICS_TOKEN already exists; its value was preserved.")
else:
    fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_NOFOLLOW)
    try:
        os.fchmod(fd, 0o600)
        os.write(fd, ("\nCLEX_METRICS_TOKEN=" + secrets.token_urlsafe(32) + "\n").encode())
        os.fsync(fd)
    finally:
        os.close(fd)
    print("Added a random metrics token to private .env (0600). Restart the broker to load it.")
