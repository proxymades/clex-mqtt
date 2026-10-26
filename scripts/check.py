#!/usr/bin/env python3
"""Run repeatable local checks using only the isolated test database."""
from env import ROOT, local_environment
import subprocess
import sys

env = local_environment()
if not env.get("TEST_DATABASE_URL"):
    sys.exit("TEST_DATABASE_URL is required for database/MQTT integration checks.")
if env["TEST_DATABASE_URL"] == env.get("DATABASE_URL"):
    sys.exit("Use an isolated test database; TEST_DATABASE_URL must differ from DATABASE_URL.")
env["CLEX_ENV"] = "test"
commands = [
    ["python3", "scripts/check_secrets.py"],
    ["python3", "scripts/check_licenses.py"],
    ["python3", "tests/test_backups.py"],
    ["python3", "tests/test_backup_status.py"],
    ["python3", "tests/test_benchmark_tools.py"],
    ["node", "--test", "tests/bench_pacing.test.mjs"],
    ["npm", "run", "ui:build"],
    ["cargo", "fmt", "--all", "--", "--check"],
    ["cargo", "clippy", "--all-targets", "--locked", "--", "-D", "warnings"],
    ["cargo", "test", "--all-targets", "--locked", "--", "--include-ignored"],
]
for command in commands:
    print("Running " + " ".join(command), flush=True)
    result = subprocess.run(command, cwd=ROOT, env=env)
    if result.returncode:
        sys.exit(result.returncode)
