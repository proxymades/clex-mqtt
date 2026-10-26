#!/usr/bin/env python3
"""Exercise two isolated local MQTT brokers and their durable bridge queues."""
import subprocess
import sys
from urllib.parse import urlparse
from env import ROOT, local_environment

env = local_environment()
url = env.get("TEST_DATABASE_URL", "")
if env.get("CLEX_ENV") == "production" or not url or url == env.get("DATABASE_URL") or "test" not in urlparse(url).path:
    sys.exit("Set a dedicated TEST_DATABASE_URL different from DATABASE_URL for bridge checks.")
env["CLEX_ENV"] = "test"
sys.exit(subprocess.call(["cargo", "test", "--test", "mqtt", "bridges::", "--locked", "--", "--include-ignored"], cwd=ROOT, env=env))
