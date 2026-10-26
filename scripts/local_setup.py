#!/usr/bin/env python3
"""Prepare isolated development databases and an ignored, private .env file."""
from pathlib import Path
import os
import secrets
import base64
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
DATABASES = ("clex_mqtt_dev", "clex_mqtt_test", "clex_mqtt_codegen", "clex_mqtt_shadow")
ROLE = "clex_mqtt_local"

def psql(sql: str) -> str:
    result = subprocess.run(
        ["psql", "-X", "-w", "-v", "ON_ERROR_STOP=1", "-d", "postgres", "-At"],
        input=sql, text=True, capture_output=True,
    )
    if result.returncode:
        raise RuntimeError("Local PostgreSQL operation failed. Ensure psql can connect through its local socket with role/database creation privileges.")
    return result.stdout.strip()

def main() -> None:
    os.umask(0o077)
    env_file = ROOT / ".env"
    if env_file.exists():
        print(".env already exists; configuration and database credentials were left unchanged.")
        return
    if not shutil.which("psql"):
        raise RuntimeError("psql is required. Install PostgreSQL and start the local service first.")
    host = os.environ.get("PGHOST", "")
    if host and not (host.startswith("/") or host in {"localhost", "127.0.0.1", "::1"}):
        raise RuntimeError("Local setup accepts only a local Unix socket or loopback PostgreSQL host.")
    host_address = os.environ.get("PGHOSTADDR", "")
    if host_address and host_address not in {"127.0.0.1", "::1"}:
        raise RuntimeError("PGHOSTADDR must be a loopback address for local setup.")
    port = int(psql("SHOW port;"))
    if psql(f"SELECT 1 FROM pg_roles WHERE rolname = '{ROLE}';"):
        raise RuntimeError("The development role already exists, but .env is missing. Restore the local credentials; this script will not reset an existing role.")
    existing = psql("SELECT datname FROM pg_database WHERE datname IN (" + ",".join(f"'{name}'" for name in DATABASES) + ");")
    if existing:
        raise RuntimeError("A reserved development database already exists. No database was changed; inspect the existing local setup first.")
    db_password = secrets.token_urlsafe(32)
    admin_password = secrets.token_urlsafe(24)
    # The generated alphabet contains no quotes; credentials are sent over stdin, never argv.
    psql(f"CREATE ROLE {ROLE} LOGIN PASSWORD '{db_password}' NOSUPERUSER NOCREATEDB NOCREATEROLE;")
    try:
        for database in DATABASES:
            psql(f"CREATE DATABASE {database} OWNER {ROLE} TEMPLATE template0 ENCODING 'UTF8';")
    except RuntimeError:
        raise RuntimeError("Development setup was interrupted. Some isolated databases may exist; inspect them before retrying.") from None
    def url(database: str) -> str:
        return f"postgresql://{ROLE}:{db_password}@127.0.0.1:{port}/{database}"
    content = "\n".join([
        "# Generated locally. This file contains credentials and must remain untracked.",
        "CLEX_ENV=development", f"DATABASE_URL={url(DATABASES[0])}",
        f"TEST_DATABASE_URL={url(DATABASES[1])}", f"CLEX_CODEGEN_DATABASE_URL={url(DATABASES[2])}",
        f"SHADOW_DATABASE_URL={url(DATABASES[3])}",
        "CLEX_MQTT_ADDR=127.0.0.1:1883", "CLEX_HTTP_ADDR=127.0.0.1:8085",
        "CLEX_PUBLIC_URL=http://127.0.0.1:8085", "CLEX_COOKIE_SECURE=false",
        "CLEX_DB_MAX_CONNECTIONS=10", "CLEX_MAX_PACKET_BYTES=1048576",
        "CLEX_MAX_CONNECTIONS=1000", "CLEX_MAX_INFLIGHT=32", "CLEX_MAX_PENDING=10000",
        "CLEX_MAX_PENDING_BYTES=67108864",
        "CLEX_BRIDGE_SECRET_KEY=" + base64.b64encode(secrets.token_bytes(32)).decode("ascii"),
        "CLEX_METRICS_TOKEN=" + secrets.token_urlsafe(32),
        "CLEX_ADMIN_USERNAME=admin", f"CLEX_ADMIN_PASSWORD={admin_password}",
        "RUST_LOG=clex_mqtt=info,tower_http=warn", "",
    ])
    fd = os.open(env_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as output:
        output.write(content)
    print("Created four isolated local databases and a private .env (mode 0600).")
    print("The panel login is admin; its generated password is in CLEX_ADMIN_PASSWORD in .env.")
    print("Next: npm ci, npm run ui:build, cargo run -- migrate, then python3 scripts/bootstrap_admin.py.")

if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
