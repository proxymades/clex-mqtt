#!/usr/bin/env python3
"""Generate an ignored one-day localhost certificate for local TLS verification."""
from pathlib import Path
import os
import shutil
import subprocess
import sys

root = Path(__file__).resolve().parents[1]
folder = root / "certs" / "development"
certificate = folder / "localhost.pem"
key = folder / "localhost.key"
if certificate.exists() or key.exists():
    sys.exit("Development certificate files already exist; existing keys were left unchanged.")
if not shutil.which("openssl"):
    sys.exit("OpenSSL is required to generate a local test certificate.")
os.umask(0o077)
folder.mkdir(parents=True, mode=0o700, exist_ok=True)
result = subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", str(key), "-out", str(certificate), "-days", "1", "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1"], capture_output=True)
if result.returncode:
    sys.exit("OpenSSL could not generate the localhost certificate.")
print("Generated certs/development/localhost.pem and localhost.key (private, ignored files).")
print("Use these only for local TLS checks; production requires its own trusted certificate.")
