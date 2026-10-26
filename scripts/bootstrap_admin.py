#!/usr/bin/env python3
"""Create a local panel administrator without placing its password in argv."""
from env import ROOT, local_environment
import os
import subprocess
import sys

env = local_environment()
password = env.get("CLEX_ADMIN_PASSWORD", "")
if len(password) < 12:
    sys.exit("CLEX_ADMIN_PASSWORD must contain at least 12 bytes; set it in the private local .env.")
command = ["cargo", "run", "--", "admin", "create", "--username", env.get("CLEX_ADMIN_USERNAME", "admin"), "--password-stdin"]
result = subprocess.run(command, cwd=ROOT, env=env, input=password + "\n", text=True)
sys.exit(result.returncode)
