#!/usr/bin/env python3
"""Backup rotation/failure checks and a real restore in the dedicated test database."""
from pathlib import Path
import errno
import gzip
import importlib.util
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from urllib.parse import unquote, urlparse
import uuid

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from env import local_environment
SPEC = importlib.util.spec_from_file_location("backup_tool", ROOT / "deploy/backup/clex-mqtt-backup.py")
backup = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(backup)


class FixtureRunner:
    def __init__(self):
        self.calls = 0
        self.fail_dump = False
        self.fail_verify = False

    def __call__(self, args, *, stdin=None, stdout=None):
        if args[0] == "pg_dump" and "--version" in args:
            return b"pg_dump (PostgreSQL) 17\n"
        if args[0] == "pg_dump":
            self.calls += 1
            stdout.write(b"fixture-dump-" + str(self.calls).encode())
            if self.fail_dump:
                raise backup.BackupError("Dump interrupted.")
            return b""
        if args[0] == "psql":
            return json.dumps({"name": "clex_mqtt", "password": "fixture-verifier", "login": True}).encode()
        if args[0] == "pg_restore":
            if self.fail_verify or not stdin.read().startswith(b"fixture-dump-"):
                raise backup.BackupError("Restore validation failed.")
            return b""
        raise AssertionError("Unexpected PostgreSQL command")


class BackupTests(unittest.TestCase):
    def setUp(self):
        cache = ROOT / ".cache" / "tmp"
        cache.mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(prefix="backup-test-", dir=cache)
        self.root = Path(self.temporary.name)
        self.source = self.root / "configuration"
        self.source.mkdir()
        (self.source / "service.env").write_text("FIXTURE_SECRET=fixture-only-value\n")
        (self.source / "binary").write_bytes(bytes(range(256)))
        (self.source / "relative-link").symlink_to("binary")
        self.config = dict(
            version=1, destination=str(self.root / "backups"), keep=7,
            postgres_bin="/example/postgresql/bin", host="/example/socket", port=5432,
            database="clex_mqtt", database_role="clex_mqtt", postgres_user="postgres",
            postgres_os_user="postgres", sources=[dict(path=str(self.source), required=True)],
        )
        self.runner = FixtureRunner()
        self.addCleanup(self.temporary.cleanup)

    def create(self):
        return backup.create_backup(self.config, self.runner)[0]

    def names(self):
        return [path.name for path in backup.completed(Path(self.config["destination"]))]

    def seven(self):
        return [self.create() for _ in range(7)]

    def test_eighth_and_ninth_verified_copies_replace_only_the_oldest(self):
        paths = [self.create() for _ in range(9)]
        self.assertEqual(set(self.names()), {path.name for path in paths[-7:]})
        self.assertFalse(paths[0].exists())
        latest = paths[-1]
        manifest = backup.verify_backup(latest, self.runner)
        self.assertEqual(set(manifest["files"]), set(backup.FILES))
        self.assertEqual(latest.stat().st_mode & 0o777, 0o700)
        for file in latest.iterdir():
            self.assertEqual(file.stat().st_mode & 0o777, 0o600)
        with tarfile.open(latest / "files.tar.gz") as archive:
            self.assertEqual(
                archive.extractfile(str(self.source / "binary").lstrip("/")).read(),
                bytes(range(256)),
            )

    def test_failed_dump_preserves_all_seven_and_next_run_succeeds(self):
        self.seven()
        before = self.names()
        self.runner.fail_dump = True
        with self.assertRaises(backup.BackupError):
            self.create()
        self.assertEqual(self.names(), before)
        self.assertFalse(list(Path(self.config["destination"]).glob(".pending-*")))
        self.runner.fail_dump = False
        latest = self.create()
        self.assertIn(latest.name, self.names())
        self.assertEqual(len(self.names()), 7)

    def test_failed_full_archive_validation_does_not_rotate(self):
        self.seven()
        before = self.names()
        self.runner.fail_verify = True
        with self.assertRaises(backup.BackupError):
            self.create()
        self.assertEqual(self.names(), before)

    def test_disk_full_and_failed_publication_preserve_old_copies(self):
        self.seven()
        before = self.names()
        with patch.object(backup, "archive_files", side_effect=OSError(errno.ENOSPC, "fixture disk full")):
            with self.assertRaises(OSError):
                self.create()
        self.assertEqual(self.names(), before)
        with patch.object(backup.os, "rename", side_effect=PermissionError("fixture")):
            with self.assertRaises(PermissionError):
                self.create()
        self.assertEqual(self.names(), before)

    def test_lock_blocks_duplicate_jobs_and_cleanup_ignores_unrelated_files(self):
        base = Path(self.config["destination"])
        with backup.backup_lock(base):
            with self.assertRaises(backup.BackupError):
                self.create()
        stale = base / (".pending-" + "a" * 32)
        stale.mkdir()
        (stale / "partial").write_text("partial")
        unrelated = base / "manual-backup"
        unrelated.mkdir()
        (unrelated / "keep").write_text("unrelated")
        self.create()
        self.assertFalse(stale.exists())
        self.assertTrue((unrelated / "keep").exists())

    def test_corruption_and_unsafe_archive_members_are_rejected(self):
        path = self.create()
        (path / "database.dump").write_bytes(b"corrupt")
        with self.assertRaises(backup.BackupError):
            backup.verify_backup(path, self.runner)
        path = self.create()
        with tarfile.open(path / "files.tar.gz", "w:gz") as archive:
            member = tarfile.TarInfo("../../outside")
            member.size = 1
            archive.addfile(member, io.BytesIO(b"x"))
        manifest = json.loads((path / "manifest.json").read_text())
        manifest["files"]["files.tar.gz"] = dict(
            bytes=(path / "files.tar.gz").stat().st_size,
            sha256=backup.digest(path / "files.tar.gz"),
        )
        (path / "manifest.json").write_text(json.dumps(manifest))
        with self.assertRaises(backup.BackupError):
            backup.verify_backup(path, self.runner)
        self.assertFalse((self.root / "outside").exists())

    def test_destination_inside_sources_and_remote_database_are_rejected(self):
        for changes in (
            dict(destination=str(self.source / "backups")),
            dict(host="db.example.com"), dict(keep=0), dict(database="invalid;command"),
        ):
            with self.assertRaises(backup.BackupError):
                backup.validate_config(self.config | changes)

    def test_database_runner_uses_peer_identity_and_drops_inherited_pg_settings(self):
        with patch.object(backup.pwd, "getpwnam", return_value=SimpleNamespace(pw_uid=1001, pw_gid=1002)):
            with patch.dict(os.environ, {"PGPASSWORD": "fixture-not-inherited", "PGSERVICE": "unrelated"}):
                runner = backup.DatabaseRunner(self.config)
        with patch.object(backup.subprocess, "run", return_value=SimpleNamespace(returncode=0, stdout=b"version")) as call:
            self.assertEqual(runner(["pg_dump", "--version"]), b"version")
        kwargs = call.call_args.kwargs
        self.assertEqual((kwargs["user"], kwargs["group"]), (1001, 1002))
        self.assertNotIn("PGPASSWORD", kwargs["env"])
        self.assertNotIn("PGSERVICE", kwargs["env"])
        self.assertEqual(kwargs["env"]["PGPASSFILE"], "/dev/null")
        self.assertNotIn("fixture-not-inherited", repr(call.call_args))

    def test_system_errors_are_actionable_without_printing_private_values(self):
        private = "fixture-private-value"
        error = PermissionError(errno.EPERM, private, private)
        message = backup.error_message(error)
        self.assertIn("PermissionError", message)
        self.assertIn("errno=1", message)
        self.assertNotIn(private, message)
        self.assertNotIn(private, backup.error_message(KeyError(private)))

    def test_real_pg_dump_restores_binary_data_in_an_isolated_test_schema(self):
        env = local_environment()
        url = env.get("TEST_DATABASE_URL", "")
        parsed = urlparse(url)
        if not url or url == env.get("DATABASE_URL") or "test" not in parsed.path.lower():
            self.fail("A dedicated TEST_DATABASE_URL is required.")
        pg_env = {key: value for key, value in env.items() if not key.startswith("PG")}
        pg_env.update(
            PGHOST=parsed.hostname or "127.0.0.1", PGPORT=str(parsed.port or 5432),
            PGUSER=unquote(parsed.username or ""), PGPASSWORD=unquote(parsed.password or ""),
            PGDATABASE=unquote(parsed.path.lstrip("/")), PGCONNECT_TIMEOUT="5", PGPASSFILE="/dev/null",
        )
        schema = "backup_test_" + uuid.uuid4().hex
        old_schema = schema + "_source"
        def psql(sql):
            result = subprocess.run(
                ["psql", "-X", "-w", "-A", "-t", "-v", "ON_ERROR_STOP=1"],
                input=sql.encode(), stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=pg_env,
            )
            if result.returncode:
                raise AssertionError("Isolated PostgreSQL fixture operation failed.")
            return result.stdout.decode().strip()

        def real_runner(args, *, stdin=None, stdout=None):
            if args[0] == "psql":
                return json.dumps({"name": "clex_mqtt", "password": "fixture-verifier"}).encode()
            command = [shutil.which(args[0]), *args[1:]]
            if args[0] == "pg_dump" and "--version" not in args:
                command = [arg for arg in command if not arg.startswith("--dbname=")]
                command += ["--schema=" + schema]
            result = subprocess.run(
                command, stdin=stdin, stdout=stdout if stdout is not None else subprocess.PIPE,
                stderr=subprocess.PIPE, env=pg_env,
            )
            if result.returncode:
                raise AssertionError("Isolated PostgreSQL dump/validation failed.")
            return result.stdout or b""

        try:
            psql(f'CREATE SCHEMA "{schema}"; CREATE TABLE "{schema}".payloads (id int PRIMARY KEY, payload bytea); INSERT INTO "{schema}".payloads VALUES (1,decode(\'00ff1020\',\'hex\'));')
            path, _ = backup.create_backup(self.config, real_runner)
            psql(f'ALTER SCHEMA "{schema}" RENAME TO "{old_schema}";')
            with (path / "database.dump").open("rb") as source:
                result = subprocess.run(
                    ["pg_restore", "--no-password", "--no-owner", "--no-acl", "--exit-on-error", "--dbname=" + pg_env["PGDATABASE"]],
                    stdin=source, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=pg_env,
                )
            self.assertEqual(result.returncode, 0, "Restore into the dedicated test database failed.")
            self.assertEqual(psql(f'SELECT encode(payload,\'hex\') FROM "{schema}".payloads WHERE id=1;'), "00ff1020")
        finally:
            psql(f'DROP SCHEMA IF EXISTS "{schema}" CASCADE; DROP SCHEMA IF EXISTS "{old_schema}" CASCADE;')


if __name__ == "__main__":
    unittest.main(verbosity=2)
