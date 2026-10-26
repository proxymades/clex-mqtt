"""Read-only, schema-scoped sampling for the local benchmark process."""
import json
import os
from pathlib import Path
import sys
import subprocess
import threading
import time
import uuid

def cpu_seconds(value):
    days, separator, rest = value.partition("-")
    fields = (rest if separator else days).split(":")
    total = float(fields[-1]) + (int(fields[-2]) * 60 if len(fields) > 1 else 0) + (int(fields[-3]) * 3600 if len(fields) > 2 else 0)
    return total + (int(days) * 86400 if separator else 0)


def process_stats(pid):
    if sys.platform == "linux":
        fields = Path(f"/proc/{int(pid)}/stat").read_text().rpartition(") ")[2].split()
        if len(fields) < 22:
            raise ValueError("Incomplete process counters")
        cpu = (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")
        rss = int(fields[21]) * os.sysconf("SC_PAGE_SIZE") / 1024
        return rss, cpu
    result = subprocess.run(
        ["ps", "-p", str(pid), "-o", "rss=", "-o", "time="],
        capture_output=True, text=True, timeout=2,
    )
    rss, cpu = result.stdout.split()
    return int(rss), cpu_seconds(cpu)


class Diagnostics:
    def __init__(self, env, schema, directory):
        self.env = {**env, "PGAPPNAME": "clex_benchmark_sampler"}
        self.schema, self.directory = schema, directory
        self.samples, self.errors, self.error = [], 0, None
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.sample, daemon=True)

    def query(self, sql):
        result = subprocess.run(
            ["psql", "-X", "-qAt", "-w", "-v", "ON_ERROR_STOP=1"],
            input=f"SET search_path TO {self.schema}; SET statement_timeout=1500;\n" + sql,
            env=self.env, text=True, capture_output=True, timeout=3,
        )
        if result.returncode:
            raise RuntimeError("Benchmark diagnostic query failed: " + result.stderr[:500])
        return json.loads(result.stdout)

    def sample(self):
        try:
            self.collect()
        except Exception as error:
            self.errors += 1
            self.error = str(error)

    def collect(self):
        started = time.monotonic()
        explained = set()
        accounting = self.query("SELECT to_json(EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema=current_schema() AND table_name='sessions' AND column_name='queued_count'));")
        self.accounting = accounting
        accounting_fields = """'accountingMismatch', (SELECT count(*) FROM sessions s LEFT JOIN usage q ON q.session_id=s.id WHERE (s.queued_count IS NOT NULL OR s.queued_bytes IS NOT NULL) AND (s.queued_count IS DISTINCT FROM coalesce(q.count,0) OR s.queued_bytes IS DISTINCT FROM coalesce(q.bytes,0))), 'accountingUnknown', (SELECT count(*) FROM sessions WHERE queued_count IS NULL OR queued_bytes IS NULL),""" if accounting else "'accountingMismatch', null, 'accountingUnknown', null,"
        while not self.stop.is_set():
            try:
                snapshot = self.query(f"""
WITH usage AS MATERIALIZED (SELECT d.session_id,count(*) AS count,sum(octet_length(m.payload)::bigint+octet_length(m.topic)+octet_length(m.properties::text))::bigint AS bytes FROM deliveries d JOIN messages m ON m.id=d.message_id GROUP BY d.session_id)
SELECT json_build_object(
 'queue', (SELECT coalesce(json_object_agg(phase,n),'{{}}') FROM
    (SELECT phase,count(*) n FROM deliveries GROUP BY phase) q),
 'queueBytes', (SELECT coalesce(sum(bytes),0) FROM usage),
 'incomingQos2', (SELECT count(*) FROM incoming_qos2 WHERE NOT released),
 'session', (SELECT session_id FROM usage ORDER BY count DESC LIMIT 1),
 {accounting_fields}
 'activity', (SELECT coalesce(json_agg(json_build_object('pid',pid,'state',state,'wait',wait_event,'kind',CASE
    WHEN query LIKE '%pg_advisory_xact_lock%' THEN 'state_lock'
    WHEN query LIKE '%AS bytes FROM deliveries%' THEN 'queue_quota'
    WHEN query LIKE '%SELECT queued_count,queued_bytes%' THEN 'queue_quota'
    WHEN query LIKE '%count(*) AS count%incoming_qos2%' THEN 'incoming_quota'
    WHEN query LIKE '%FROM subscriptions sub JOIN sessions%' THEN 'routing'
    WHEN query LIKE '%NOT(d.id=ANY%' OR query LIKE '%WITH pending AS MATERIALIZED%' THEN 'delivery_payloads'
    WHEN query LIKE '%SELECT id,packet_id FROM deliveries%' THEN 'delivery_inflight'
    WHEN query LIKE '%DELETE FROM deliveries d USING messages%' OR query LIKE '%expired AS MATERIALIZED%' THEN 'delivery_expiry'
    WHEN query LIKE '%INSERT INTO deliveries%' THEN 'delivery_enqueue'
    WHEN query LIKE '%FROM mqtt_acl_rules%' THEN 'acl'
    WHEN query LIKE '%FROM mqtt_users WHERE username%' THEN 'credentials'
    WHEN query LIKE '%FROM sessions WHERE id=%FOR UPDATE%' THEN 'session_fence'
    WHEN query LIKE '%UPDATE deliveries%' THEN 'delivery_transition'
    WHEN query LIKE '%DELETE FROM deliveries%' THEN 'delivery_ack'
    WHEN query LIKE '%COMMIT%' THEN 'commit'
    ELSE 'other' END)),'[]') FROM pg_stat_activity WHERE application_name='{self.schema}')
);
""")
                snapshot["seconds"] = round(time.monotonic() - started, 3)
                pids = [str(row["pid"]) for row in snapshot["activity"]]
                if pids:
                    snapshot["postgresProcesses"] = []
                    for pid in pids:
                        try:
                            rss, cpu = process_stats(pid)
                            snapshot["postgresProcesses"].append([pid, str(rss), str(cpu)])
                        except (OSError, ValueError, IndexError):
                            pass
                self.samples.append(snapshot)
                threshold = 6000 if sum(snapshot["queue"].values()) >= 6000 else 1000
                if threshold not in explained and sum(snapshot["queue"].values()) >= threshold and snapshot["session"]:
                    explained.add(threshold)
                    self.explain(str(uuid.UUID(snapshot["session"])), threshold)
                snapshot.pop("session", None)
            except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired) as error:
                self.errors += 1
                self.error = str(error)
            self.stop.wait(.5)

    def explain(self, session, threshold):
        statements = {
            "quota": f"SELECT count(*) AS count,coalesce(sum(octet_length(m.payload)+octet_length(m.topic)+octet_length(m.properties::text)),0)::bigint AS bytes FROM deliveries d JOIN messages m ON m.id=d.message_id WHERE d.session_id='{session}'",
            "inflight": f"SELECT id,packet_id FROM deliveries WHERE session_id='{session}' AND packet_id IS NOT NULL ORDER BY sequence LIMIT 65535",
            "payloads": f"SELECT d.id,m.payload FROM deliveries d JOIN messages m ON m.id=d.message_id WHERE d.session_id='{session}' AND NOT(d.id=ANY(ARRAY(SELECT id FROM deliveries WHERE session_id='{session}' AND packet_id IS NOT NULL))) ORDER BY d.sequence LIMIT 64",
            "bounded-payloads": f"WITH pending AS MATERIALIZED (SELECT * FROM deliveries WHERE session_id='{session}' AND NOT(id=ANY(ARRAY(SELECT id FROM deliveries WHERE session_id='{session}' AND packet_id IS NOT NULL))) ORDER BY sequence LIMIT 64) SELECT d.id,m.payload FROM pending d JOIN messages m ON m.id=d.message_id ORDER BY d.sequence",
            "expiry": f"SELECT d.id FROM deliveries d JOIN messages m ON d.message_id=m.id WHERE d.session_id='{session}' AND d.phase='queued' AND m.expires_at<=now()",
            "bounded-expiry": f"WITH expired AS MATERIALIZED (SELECT id FROM messages WHERE expires_at<=now()) SELECT d.id FROM deliveries d JOIN expired m ON d.message_id=m.id WHERE d.session_id='{session}' AND d.phase='queued'",
        }
        if self.accounting:
            statements["quota-counters"] = f"SELECT queued_count,queued_bytes FROM sessions WHERE id='{session}'"
        for name, sql in statements.items():
            plan = self.query("EXPLAIN (ANALYZE,BUFFERS,FORMAT JSON) " + sql)
            (self.directory / f"plan-{threshold}-{name}.json").write_text(json.dumps(plan, indent=2) + "\n")

    def finish(self):
        self.stop.set()
        self.thread.join(timeout=15)
        try:
            final = self.query(f"SELECT json_build_object('queueMessages',(SELECT count(*) FROM deliveries),'incomingQos2',(SELECT count(*) FROM incoming_qos2 WHERE NOT released),'tables',(SELECT json_agg(json_build_object('table',relname,'seqScans',seq_scan,'indexScans',idx_scan,'inserts',n_tup_ins,'updates',n_tup_upd,'deletes',n_tup_del,'deadRows',n_dead_tup)) FROM pg_stat_user_tables WHERE schemaname='{self.schema}'));")
        except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired):
            self.errors += 1
            final = {"queueMessages": None, "incomingQos2": None}
        (self.directory / "diagnostics.json").write_text(json.dumps({"intervalSeconds": .5, "errors": self.errors, "error": self.error, "final": final, "samples": self.samples}, indent=2) + "\n")
        peaks = {}
        for sample in self.samples:
            for phase, value in sample["queue"].items():
                peaks[phase] = max(peaks.get(phase, 0), value)
        processes = {}
        for sample in self.samples:
            for pid, _rss, cpu in sample.get("postgresProcesses", []):
                seconds = cpu_seconds(cpu)
                first, _last = processes.get(pid, (seconds, seconds))
                processes[pid] = (first, seconds)
        cpu = sum(last - first for first, last in processes.values())
        duration = self.samples[-1]["seconds"] if self.samples else 0
        return {"samples": len(self.samples), "errors": self.errors, "accountingMismatchSamples": sum(bool(s.get("accountingMismatch")) for s in self.samples), "accountingAuditedSamples": sum(s.get("accountingMismatch") is not None for s in self.samples), "peakQueueMessages": max((sum(s["queue"].values()) for s in self.samples), default=0), "peakQueueByPhase": peaks, "finalQueueMessages": final["queueMessages"], "finalIncomingQos2": final["incomingQos2"], "postgresCpuSeconds": round(cpu, 3) if processes else None, "postgresAverageCpuPercent": round(cpu / duration * 100, 1) if duration and processes else None}
