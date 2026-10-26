#!/usr/bin/env python3
"""Exercise a real local mTLS broker using temporary keys and a test DB schema."""
import subprocess
import sys
from urllib.parse import urlparse
from env import ROOT, local_environment

env = local_environment()
url = env.get("TEST_DATABASE_URL", "")
if env.get("CLEX_ENV") == "production" or not url or url == env.get("DATABASE_URL") or "test" not in urlparse(url).path:
    sys.exit("Set a dedicated TEST_DATABASE_URL different from DATABASE_URL for mTLS checks.")
env["CLEX_ENV"] = "test"
sys.exit(subprocess.call(["cargo", "test", "--test", "tls", "--locked", "--", "--include-ignored"], cwd=ROOT, env=env))
