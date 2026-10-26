#!/usr/bin/env python3
"""Measure an isolated MQTT process using MQTT.js and a dedicated test schema."""
import argparse
import hashlib
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import secrets
import signal
import socket
import subprocess
import sys
import threading
import time
from urllib.parse import parse_qs, unquote, urlencode, urlparse, urlunparse
from urllib.request import ProxyHandler, Request, build_opener
from env import ROOT, local_environment
from bench_diagnostics import Diagnostics, process_stats

HTTP = build_opener(ProxyHandler({}))

def arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=("quick", "standard"), default="quick")
    parser.add_argument("--qos", choices=("all", "0", "1", "2"), default="all")
    parser.add_argument("--protocol", type=int, choices=(4, 5), default=5)
    parser.add_argument("--publishers", type=int)
    parser.add_argument("--subscribers", type=int)
    parser.add_argument("--messages", type=int)
    parser.add_argument("--payload-bytes", type=int, default=256)
    parser.add_argument("--publish-rate", type=int, default=0, help="Target aggregate publications per second; 0 sends without pacing")
    parser.add_argument("--window", type=int)
    parser.add_argument("--timeout", type=int)
    parser.add_argument("--subscriber-pause-ms", type=int, default=0, help="Pause subscriber sockets to verify delivery latency independently of publisher acknowledgements")
    parser.add_argument("--trace", action="store_true", help="Enable bounded message tracing for the benchmark topic")
    parser.add_argument("--debug", action="store_true", help="Use a debug binary; results are tagged accordingly")
    parser.add_argument("--binary", type=Path, help="Use an existing project-local binary for before/after comparisons")
    args = parser.parse_args()
    args.publishers = args.publishers if args.publishers is not None else (2 if args.profile == "quick" else 8)
    args.subscribers = args.subscribers if args.subscribers is not None else (2 if args.profile == "quick" else 8)
    args.messages = args.messages if args.messages is not None else (250 if args.profile == "quick" else 1000)
    args.window = args.window if args.window is not None else (8 if args.profile == "quick" else 16)
    args.timeout = args.timeout if args.timeout is not None else (120 if args.profile == "quick" else 600)
    for name, lower, upper in (("publishers", 1, 32), ("subscribers", 1, 32), ("messages", 1, 50000), ("payload_bytes", 32, 65536), ("window", 1, 128), ("timeout", 5, 600)):
        if not lower <= getattr(args, name) <= upper:
            parser.error(f"{name.replace('_', '-')} must be in {lower}..{upper}")
    if not 0 <= args.publish_rate <= 10000:
        parser.error("publish-rate must be in 0..10000")
    if not 0 <= args.subscriber_pause_ms <= 5000:
        parser.error("subscriber-pause-ms must be in 0..5000")
    if args.publishers * args.messages > 200000 or args.publishers * args.messages * args.subscribers > 2000000:
        parser.error("Use at most 200000 publications and 2000000 expected deliveries per QoS profile")
    return args

def psql(env, statement):
    result = subprocess.run(["psql", "-X", "-q", "-w", "-v", "ON_ERROR_STOP=1"], input=statement, text=True, env=env, capture_output=True)
    if result.returncode:
        raise RuntimeError("Isolated benchmark database operation failed")

class Resources:
    def __init__(self, pid):
        self.pid, self.peak, self.first, self.last = pid, 0, None, None
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.sample, daemon=True)
    def sample(self):
        while not self.stop.is_set():
            try:
                rss, value = process_stats(self.pid)
                self.first = value if self.first is None else self.first
                self.last = value
                self.peak = max(self.peak, int(rss))
            except (OSError, ValueError, subprocess.TimeoutExpired):
                pass
            self.stop.wait(.25)
    def finish(self, seconds):
        self.stop.set()
        self.thread.join(timeout=3)
        delta = max(0, self.last - self.first) if self.first is not None and self.last is not None else None
        return {"sampledPeakRssMiB": round(self.peak / 1024, 2) if self.peak else None, "cpuSeconds": round(delta, 3) if delta is not None else None, "averageCpuPercent": round(delta / seconds * 100, 1) if delta is not None and seconds else None, "sampleIntervalSeconds": .25}

def request(url, token=None, body=None):
    headers = {"Content-Type": "application/json"} if body is not None else {}
    if token:
        headers["Authorization"] = "Bearer " + token
    data = json.dumps(body).encode() if body is not None else None
    return HTTP.open(Request(url, data=data, headers=headers), timeout=5)

def stop_child(child):
    if child is not None and child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=10)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=5)

def main():
    args = arguments()
    env = local_environment()
    raw = env.get("TEST_DATABASE_URL", "")
    database = urlparse(raw)
    runtime = urlparse(env.get("DATABASE_URL", ""))
    if env.get("CLEX_ENV") == "production" or database.scheme not in ("postgres", "postgresql") or database.hostname not in ("127.0.0.1", "localhost", "::1") or "test" not in database.path.lower():
        raise RuntimeError("Use a dedicated loopback TEST_DATABASE_URL containing 'test' in its database name")
    if runtime.hostname in ("127.0.0.1", "localhost", "::1") and (database.port or 5432, database.path) == (runtime.port or 5432, runtime.path):
        raise RuntimeError("Benchmark and runtime databases must differ")
    build = ["cargo", "build", "--locked"] + ([] if args.debug else ["--release"])
    if args.binary is None:
        subprocess.run(build, cwd=ROOT, env=env, check=True)
    mode = "debug" if args.debug else "release"
    binary = args.binary.resolve() if args.binary else ROOT / "target" / mode / "clex-mqtt"
    if ROOT not in binary.parents or not binary.is_file():
        raise RuntimeError("Benchmark binary must be a file inside the project")
    binary_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
    identifier = secrets.token_hex(16)
    schema = "benchmark_" + identifier
    directory = ROOT / "work" / "benchmarks" / (datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + identifier[:8])
    directory.mkdir(parents=True, mode=0o700)
    password, admin_password, token = secrets.token_urlsafe(32), secrets.token_urlsafe(32), secrets.token_urlsafe(32)
    username = "benchmark-" + identifier[:12]
    pgenv = {key: value for key, value in os.environ.items() if not key.startswith("PG")}
    pgenv.update(PGHOST=database.hostname, PGPORT=str(database.port or 5432), PGUSER=unquote(database.username or ""), PGPASSWORD=unquote(database.password or ""), PGDATABASE=unquote(database.path[1:]))
    if parse_qs(database.query).get("sslmode"):
        pgenv["PGSSLMODE"] = parse_qs(database.query)["sslmode"][0]
    child, worker, resources, diagnostics, created = None, None, None, None, False
    reservations = [socket.socket(), socket.socket()]
    for sock in reservations:
        sock.bind(("127.0.0.1", 0))
    mqtt_port, http_port = [sock.getsockname()[1] for sock in reservations]
    query = parse_qs(database.query)
    query["options[search_path]"] = [schema]
    query["application_name"] = [schema]
    isolated_url = urlunparse(database._replace(query=urlencode(query, doseq=True)))
    broker_env = {"PATH": env.get("PATH", ""), "TMPDIR": env["TMPDIR"], "DATABASE_URL": isolated_url, "CLEX_ENV": "test", "CLEX_MQTT_ADDR": f"127.0.0.1:{mqtt_port}", "CLEX_HTTP_ADDR": f"127.0.0.1:{http_port}", "CLEX_PUBLIC_URL": f"http://127.0.0.1:{http_port}", "CLEX_COOKIE_SECURE": "false", "CLEX_METRICS_TOKEN": token, "CLEX_DB_MAX_CONNECTIONS": "10", "CLEX_MAX_CONNECTIONS": "256", "CLEX_MAX_PACKET_BYTES": "1048576", "CLEX_MAX_INFLIGHT": "128", "CLEX_MAX_PENDING": "10000", "CLEX_MAX_PENDING_BYTES": "67108864", "RUST_LOG": "off"}
    log_path = directory / "broker.log"
    fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as log:
        try:
            psql(pgenv, f"CREATE SCHEMA {schema};")
            created = True
            subprocess.run([str(binary), "migrate"], cwd="/", env=broker_env, stdout=log, stderr=log, check=True)
            subprocess.run([str(binary), "admin", "create", "--username", username, "--password-stdin"], input=admin_password + "\n", text=True, cwd="/", env=broker_env, stdout=log, stderr=log, check=True)
            for sock in reservations:
                sock.close()
            child = subprocess.Popen([str(binary), "serve"], cwd="/", env=broker_env, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
            base = f"http://127.0.0.1:{http_port}"
            deadline = time.monotonic() + 15
            while True:
                if child.poll() is not None:
                    raise RuntimeError("Isolated benchmark broker exited before readiness")
                try:
                    with request(base + "/health/ready") as response:
                        if response.status == 200:
                            break
                except OSError:
                    pass
                if time.monotonic() >= deadline:
                    raise RuntimeError("Isolated broker readiness deadline exceeded")
                time.sleep(.05)
            with request(base + "/api/v1/session", body={"username": username, "password": admin_password}) as response:
                session = json.load(response)
                cookie = response.headers["Set-Cookie"].split(";", 1)[0]
            user_body = json.dumps({"username": username, "password": password, "publishFilters": ["bench/#"], "subscribeFilters": ["bench/#"]}).encode()
            req = Request(base + "/api/v1/users", data=user_body, headers={"Content-Type": "application/json", "Cookie": cookie, "Origin": base, "X-CSRF-Token": session["csrfToken"]})
            with HTTP.open(req, timeout=10) as response:
                if response.status != 201:
                    raise RuntimeError("Benchmark MQTT account creation failed")
            if args.trace:
                req = Request(base + "/api/v1/diagnostics", data=json.dumps({"topic":"bench/#","seconds":300}).encode(), headers={"Content-Type":"application/json","Cookie":cookie,"Origin":base,"X-CSRF-Token":session["csrfToken"]})
                with HTTP.open(req, timeout=10) as response:
                    if response.status != 201:
                        raise RuntimeError("Benchmark trace start failed")
            config = {"publishers": args.publishers, "subscribers": args.subscribers, "messages": args.messages, "payloadBytes": args.payload_bytes, "publishRate": args.publish_rate, "window": args.window, "timeout": args.timeout, "protocol": args.protocol, "subscriberPauseMs": args.subscriber_pause_ms, "qos": [0, 1, 2] if args.qos == "all" else [int(args.qos)]}
            worker_env = {"PATH": env.get("PATH", ""), "TMPDIR": env["TMPDIR"], "CLEX_BENCH_URL": f"mqtt://127.0.0.1:{mqtt_port}", "CLEX_BENCH_USERNAME": username, "CLEX_BENCH_PASSWORD": password, "CLEX_BENCH_CONFIG": json.dumps(config)}
            resources = Resources(child.pid)
            resources.thread.start()
            diagnostics = Diagnostics(pgenv, schema, directory)
            diagnostics.thread.start()
            started = time.monotonic()
            print(f"Running isolated {mode} MQTT {args.protocol} benchmark: QoS {config['qos']}, {args.publishers} publishers, {args.subscribers} subscribers", flush=True)
            worker = subprocess.Popen(["node", str(ROOT / "scripts" / "bench_worker.mjs")], cwd=ROOT, env=worker_env, stdout=subprocess.PIPE, stderr=log, text=True)
            output, _ = worker.communicate(timeout=len(config["qos"]) * (args.timeout * 2 + 30))
            elapsed = time.monotonic() - started
            resource_report = resources.finish(elapsed)
            diagnostic_report = diagnostics.finish()
            result = json.loads(output)
            if diagnostic_report["accountingMismatchSamples"] or diagnostic_report["errors"] or not diagnostic_report["samples"]:
                result["ok"] = False
            with request(base + "/metrics", token=token) as response:
                metrics = response.read().decode()
            (directory / "metrics.openmetrics").write_text(metrics)
            version = re.search(r'^version\s*=\s*"([^"]+)"', (ROOT / "Cargo.toml").read_text(), re.M).group(1)
            trace_report = None
            if args.trace:
                with HTTP.open(Request(base + "/api/v1/diagnostics", headers={"Cookie":cookie}), timeout=10) as response:
                    trace = json.load(response)
                messages = []
                for item in trace["messages"]:
                    with HTTP.open(Request(
                        base + "/api/v1/diagnostics/messages/" + item["id"],
                        headers={"Cookie": cookie},
                    ), timeout=10) as response:
                        messages.append(json.load(response))
                trace_path = directory / "trace.json"
                trace_path.write_text(json.dumps(
                    {"page": trace, "messages": messages}, ensure_ascii=False, indent=2
                ) + "\n")
                trace_path.chmod(0o600)
                trace_report = {key:trace[key] for key in ("active","capacity","eventCapacity","bytes","evicted")}
                trace_report["entries"] = len(trace["messages"])
                if trace_report["entries"] > 100 or trace_report["bytes"] > 8 * 1024 * 1024:
                    result["ok"] = False
            report = {"schemaVersion": 2, "createdAt": datetime.now(timezone.utc).isoformat(), "brokerVersion": version, "binarySha256": binary_hash, "build": mode, "tracing": trace_report, "transport": "TCP loopback", "client": "MQTT.js", "profile": args.profile, "resources": resource_report, "diagnostics": diagnostic_report, **result}
            (directory / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
            for row in result["profiles"]:
                print(f"QoS {row['qos']}: {row['published']} published, {row['received']}/{row['expectedDeliveries']} delivered, p95 {row['deliveryLatencyMs']['p95']} ms, p99 {row['deliveryLatencyMs']['p99']} ms, {row['deliveredPerSecond']} deliveries/s", flush=True)
            print(f"Broker sampled peak RSS: {resource_report['sampledPeakRssMiB']} MiB; average CPU: {resource_report['averageCpuPercent']}%", flush=True)
            print("Report: " + str((directory / "report.json").relative_to(ROOT)), flush=True)
            if worker.returncode or result.get("ok") is False:
                raise RuntimeError("Benchmark validation failed; see failedProfile, delivery counters and queue accounting diagnostics in the saved report")
        finally:
            stop_child(worker)
            if resources and resources.thread.is_alive():
                resources.finish(1)
            if diagnostics and diagnostics.thread.is_alive():
                diagnostics.finish()
            stop_child(child)
            for sock in reservations:
                sock.close()
            if created:
                psql(pgenv, f"DROP SCHEMA {schema} CASCADE;")

if __name__ == "__main__":
    def interrupted(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    try:
        main()
    except RuntimeError as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        print("Benchmark cancelled after fixture cleanup.", file=sys.stderr)
        sys.exit(130)
    except (OSError, subprocess.SubprocessError, ValueError):
        print("Benchmark stopped. Check the dedicated test database, local dependencies and private work/benchmarks fixture log. Temporary processes and the test schema are cleaned up on exit.", file=sys.stderr)
        sys.exit(1)
