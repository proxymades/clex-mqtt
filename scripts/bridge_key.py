#!/usr/bin/env python3
"""Add a random bridge encryption key to the private local environment file."""
import base64
import os
from pathlib import Path
import secrets
import sys

path = Path(__file__).resolve().parents[1] / ".env"
if path.is_symlink() or not path.is_file():
    sys.exit("Prepare a regular private .env first.")
content = path.read_text()
if any(line.strip().startswith(("CLEX_BRIDGE_SECRET_KEY=", "export CLEX_BRIDGE_SECRET_KEY=")) for line in content.splitlines()):
    print("CLEX_BRIDGE_SECRET_KEY already exists; its value was preserved.")
else:
    # O_APPEND preserves every existing setting. A mode of 0600 also protects
    # configurations originally created with a permissive umask.
    fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_NOFOLLOW)
    try:
        os.fchmod(fd, 0o600)
        key = base64.b64encode(secrets.token_bytes(32)).decode("ascii")
        os.write(fd, ("\nCLEX_BRIDGE_SECRET_KEY=" + key + "\n").encode())
        os.fsync(fd)
    finally:
        os.close(fd)
    print("Added a random bridge encryption key to private .env (0600). Restart the broker to load it.")
