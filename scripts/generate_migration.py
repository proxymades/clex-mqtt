#!/usr/bin/env python3
"""Use the official Prisma generator against an isolated schema-authoring database."""
from env import ROOT, local_environment
import re
import subprocess
import sys

if len(sys.argv) != 2 or not re.fullmatch(r"[a-z][a-z0-9_]{0,63}", sys.argv[1]):
    sys.exit("Usage: python3 scripts/generate_migration.py descriptive_name")
env = local_environment()
url = env.get("CLEX_CODEGEN_DATABASE_URL")
shadow = env.get("SHADOW_DATABASE_URL")
if not url or not shadow:
    sys.exit("Set CLEX_CODEGEN_DATABASE_URL and SHADOW_DATABASE_URL to isolated development databases.")
if env.get("CLEX_ENV") == "production" or url == env.get("DATABASE_URL") or url == shadow:
    sys.exit("Migration generation requires distinct development, runtime and shadow databases.")
env["DATABASE_URL"] = url
command = ["npx", "--no-install", "prisma", "migrate", "dev", "--create-only", "--skip-generate", "--schema", "db/schema.prisma", "--name", sys.argv[1]]
sys.exit(subprocess.run(command, cwd=ROOT, env=env).returncode)
