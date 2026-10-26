#!/usr/bin/env python3
"""Run real account UI flows against a disposable loopback broker and test schema."""
from env import ROOT, local_environment
import argparse
import json
import os
import secrets
import signal
import socket
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import parse_qs, unquote, urlencode, urlparse, urlunparse
from urllib.request import ProxyHandler, build_opener

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--suite', choices=('accounts', 'messages'), default='accounts')
    suite = parser.parse_args().suite
    env = local_environment()
    database = urlparse(env.get("TEST_DATABASE_URL", ""))
    runtime = urlparse(env.get("DATABASE_URL", ""))
    if env.get("CLEX_ENV") == "production" or database.scheme not in ("postgres", "postgresql") or database.hostname not in ("127.0.0.1", "localhost", "::1") or "test" not in database.path.lower():
        raise RuntimeError("Use a dedicated loopback TEST_DATABASE_URL with 'test' in the database name")
    if (database.hostname, database.port or 5432, database.path) == (runtime.hostname, runtime.port or 5432, runtime.path):
        raise RuntimeError("The account UI database must differ from the runtime database")
    binary = ROOT / "target" / "debug" / "clex-mqtt"
    if not binary.is_file():
        raise RuntimeError("Build the frontend, then cargo build --locked before running this check")
    os.umask(0o077)
    identifier = secrets.token_hex(8)
    schema = "accounts_ui_" + identifier
    output = ROOT / "work" / "accounts-ui" / (datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + identifier)
    output.mkdir(parents=True, mode=0o700)
    pgenv = {k: v for k, v in os.environ.items() if not k.startswith("PG")}
    pgenv.update(PGHOST=database.hostname, PGPORT=str(database.port or 5432), PGUSER=unquote(database.username or ""), PGPASSWORD=unquote(database.password or ""), PGDATABASE=unquote(database.path[1:]))
    if parse_qs(database.query).get("sslmode"):
        pgenv["PGSSLMODE"] = parse_qs(database.query)["sslmode"][0]
    def psql(sql):
        result = subprocess.run(["psql", "-X", "-q", "-w", "-v", "ON_ERROR_STOP=1"], input=sql, text=True, env=pgenv, capture_output=True)
        if result.returncode:
            raise RuntimeError("Account UI test database operation failed")
    query = parse_qs(database.query)
    query["options[search_path]"] = [schema]
    query["application_name"] = [schema]
    isolated_url = urlunparse(database._replace(query=urlencode(query, doseq=True)))
    sockets = [socket.socket(), socket.socket()]
    for sock in sockets:
        sock.bind(("127.0.0.1", 0))
    mqtt_port, http_port = [sock.getsockname()[1] for sock in sockets]
    base = "http://127.0.0.1:" + str(http_port)
    broker_env = {
        "PATH": env.get("PATH", ""), "TMPDIR": env["TMPDIR"], "CLEX_ENV": "test",
        "DATABASE_URL": isolated_url, "CLEX_MQTT_ADDR": "127.0.0.1:" + str(mqtt_port),
        "CLEX_HTTP_ADDR": "127.0.0.1:" + str(http_port), "CLEX_PUBLIC_URL": base,
        "CLEX_COOKIE_SECURE": "false", "CLEX_REGISTRATION_ENABLED": "true", "CLEX_MAX_ACCOUNTS": "4",
        "CLEX_TLS_ADDR": "", "CLEX_TLS_CERT_FILE": "", "CLEX_TLS_KEY_FILE": "", "CLEX_TLS_CLIENT_AUTH": "none",
        "CLEX_TLS_CLIENT_CA_FILE": "", "CLEX_METRICS_TOKEN": "", "CLEX_BRIDGE_SECRET_KEY": "",
        "CLEX_HTTP_TRUSTED_PROXY_IPS": "", "RUST_LOG": "off",
    }
    admin_password = secrets.token_urlsafe(24)
    child = worker = None
    created = False
    try:
        psql("CREATE SCHEMA " + schema + ";")
        created = True
        with (output / "broker.log").open("w") as log:
            subprocess.run([str(binary), "migrate"], cwd=ROOT, env=broker_env, stdout=log, stderr=log, check=True)
            subprocess.run([str(binary), "admin", "create", "--username", "ui-admin", "--password-stdin"], cwd=ROOT, env=broker_env, input=admin_password + "\n", text=True, stdout=log, stderr=log, check=True)
            for sock in sockets:
                sock.close()
            child = subprocess.Popen([str(binary), "serve"], cwd=ROOT, env=broker_env, stdout=log, stderr=log)
            http = build_opener(ProxyHandler({}))
            deadline = time.monotonic() + 15
            while True:
                if child.poll() is not None:
                    raise RuntimeError("Disposable account UI broker exited; inspect the private fixture log")
                try:
                    with http.open(base + "/health/ready", timeout=1) as response:
                        if json.load(response)["status"] == "ready":
                            break
                except (OSError, ValueError):
                    pass
                if time.monotonic() >= deadline:
                    raise RuntimeError("Disposable account UI readiness deadline exceeded")
                time.sleep(.1)
            ui_env = dict(os.environ)
            ui_env.update(
                PATH=env.get("PATH", ""), TMPDIR=env["TMPDIR"],
                PLAYWRIGHT_BROWSERS_PATH=str(ROOT / ".cache" / "playwright"),
                CLEX_UI_BASE_URL=base, CLEX_UI_MQTT_URL="mqtt://127.0.0.1:" + str(mqtt_port),
                CLEX_UI_TEST_USERNAME="ui-admin", CLEX_UI_TEST_PASSWORD=admin_password,
                CLEX_ACCOUNT_TEST_PASSWORD=secrets.token_urlsafe(24), CLEX_ACCOUNT_UI_OUTPUT=str(output),
            )
            worker = subprocess.Popen(["node", "scripts/accounts-ui.mjs" if suite == "accounts" else "scripts/messages-ui.mjs"], cwd=ROOT, env=ui_env)
            result = worker.wait(timeout=180)
            if result:
                raise RuntimeError("Account UI checks failed")
    finally:
        for process in (worker, child):
            if process and process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
        for sock in sockets:
            sock.close()
        if created:
            psql("DROP SCHEMA " + schema + " CASCADE;")
    print("Account UI fixture cleanup: OK")
    print("Screenshots and report: " + str(output.relative_to(ROOT)))

if __name__ == "__main__":
    def interrupted(_signum, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    try:
        main()
    except (RuntimeError, OSError, subprocess.SubprocessError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        print("Account UI check cancelled after fixture cleanup.", file=sys.stderr)
        sys.exit(130)
