#!/usr/bin/env python3
"""Create verified local snapshots and rotate only completed CLEX MQTT backups."""
from __future__ import annotations

import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
import fcntl
import gzip
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import posixpath
import pwd
import re
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import uuid

FINAL_NAME = re.compile(r"backup-\d{8}T\d{6}\.\d{6}Z-[0-9a-f]{8}")
PENDING_NAME = re.compile(r"\.pending-[0-9a-f]{32}")
FILES = ("database.dump", "database-role.json", "files.tar.gz")


class BackupError(Exception):
    pass


def error_message(error: Exception) -> str:
    if isinstance(error, BackupError):
        return str(error)
    if isinstance(error, OSError) and error.errno is not None:
        return f"{type(error).__name__} (errno={error.errno}): {os.strerror(error.errno)}"
    return f"{type(error).__name__}: invalid backup configuration or metadata"


def private_directory(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid():
        raise BackupError("Backup directory must be a real directory owned by the service user.")
    path.chmod(0o700)


def sync_directory(path: Path) -> None:
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def digest(path: Path) -> str:
    checksum = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            checksum.update(chunk)
    return checksum.hexdigest()


def write_json(path: Path, value: object) -> None:
    with path.open("x", encoding="utf-8") as output:
        json.dump(value, output, indent=2, ensure_ascii=False)
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())
    path.chmod(0o600)


def load_config(path: Path) -> dict:
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022:
        raise BackupError("Configuration must be a root-owned regular file without group/other write access.")
    config = json.loads(path.read_text())
    validate_config(config)
    return config


def validate_config(config: dict) -> None:
    if config.get("version") != 1 or type(config.get("keep")) is not int or config["keep"] < 1:
        raise BackupError("Expected configuration version 1 and a positive keep count.")
    for key in ("database", "database_role"):
        if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{0,62}", config.get(key, "")):
            raise BackupError("Database and role names must be simple PostgreSQL identifiers.")
    if config.get("host") not in ("127.0.0.1", "::1") and not config.get("host", "").startswith("/"):
        raise BackupError("Only a local PostgreSQL server is supported.")
    if type(config.get("port")) is not int or not 1 <= config["port"] <= 65535:
        raise BackupError("Invalid PostgreSQL port.")
    for key in ("destination", "postgres_bin"):
        if not Path(config[key]).is_absolute():
            raise BackupError("Backup and PostgreSQL paths must be absolute.")
    destination = Path(config["destination"]).resolve()
    if not config.get("sources"):
        raise BackupError("No filesystem sources configured.")
    for source in config["sources"]:
        path = Path(source["path"])
        if not path.is_absolute() or ".." in path.parts or str(path) == "/":
            raise BackupError("Filesystem sources must be explicit absolute paths.")
        if destination == path.resolve() or destination.is_relative_to(path.resolve()):
            raise BackupError("Backup destination cannot be inside a filesystem source.")


class DatabaseRunner:
    def __init__(self, config: dict):
        self.config = config
        account = pwd.getpwnam(config["postgres_os_user"])
        self.identity = dict(user=account.pw_uid, group=account.pw_gid, extra_groups=[])
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("PG")}
        self.env.update(
            PGHOST=config["host"], PGPORT=str(config["port"]), PGUSER=config["postgres_user"],
            PGDATABASE=config["database"], PGAPPNAME="clex-mqtt-backup", PGCONNECT_TIMEOUT="10",
            PGPASSFILE="/dev/null", HOME="/",
        )

    def __call__(self, args: list[str], *, stdin=None, stdout=None) -> bytes:
        executable = str(Path(self.config["postgres_bin"]) / args[0])
        result = subprocess.run(
            [executable, *args[1:]], stdin=stdin, stdout=stdout if stdout is not None else subprocess.PIPE,
            stderr=subprocess.PIPE, env=self.env, cwd="/", **self.identity,
        )
        if result.returncode:
            # PostgreSQL diagnostics can contain SQL or connection details.
            raise BackupError(f"{args[0]} failed (exit {result.returncode}); previous backups retained.")
        return result.stdout or b""


def safe_member(member: tarfile.TarInfo) -> None:
    path = PurePosixPath(member.name)
    if path.is_absolute() or ".." in path.parts:
        raise BackupError("Unsafe filesystem archive path.")
    if not (member.isfile() or member.isdir() or member.issym() or member.islnk()):
        raise BackupError("Unsupported filesystem object in backup.")
    if member.issym() or member.islnk():
        target = member.linkname if member.islnk() else posixpath.join(posixpath.dirname(member.name), member.linkname)
        target = posixpath.normpath(target)
        if target.startswith("/") or target == ".." or target.startswith("../"):
            raise BackupError("Filesystem archive link escapes the archive.")


def file_stamp(path: Path) -> tuple:
    info = path.lstat()
    return info.st_dev, info.st_ino, info.st_mode, info.st_size, info.st_mtime_ns


def archive_files(config: dict, target: Path) -> None:
    stamps = {}
    def capture(member: tarfile.TarInfo):
        safe_member(member)
        stamps[Path("/") / member.name] = file_stamp(Path("/") / member.name)
        return member
    with target.open("xb") as raw:
        target.chmod(0o600)
        with tarfile.open(fileobj=raw, mode="w:gz", dereference=False) as archive:
            for source in config["sources"]:
                path = Path(source["path"])
                if not path.exists() and not path.is_symlink():
                    if source.get("required", True):
                        raise BackupError("A required filesystem source is missing.")
                    continue
                archive.add(path, arcname=str(path).lstrip("/"), filter=capture)
        raw.flush()
        os.fsync(raw.fileno())
    if any(file_stamp(path) != stamp for path, stamp in stamps.items()):
        raise BackupError("A filesystem source changed during backup; retry on the next run.")


def verify_backup(path: Path, runner) -> dict:
    if path.is_symlink() or not path.is_dir():
        raise BackupError("Backup must be a real directory.")
    manifest_file = path / "manifest.json"
    if manifest_file.is_symlink() or not manifest_file.is_file():
        raise BackupError("Backup manifest is missing.")
    manifest = json.loads(manifest_file.read_text())
    if manifest.get("version") != 1 or set(manifest.get("files", {})) != set(FILES):
        raise BackupError("Unsupported backup manifest.")
    for name in FILES:
        file = path / name
        expected = manifest["files"][name]
        if file.is_symlink() or not file.is_file():
            raise BackupError("A backup file is missing or is a symlink.")
        if file.stat().st_size != expected["bytes"] or digest(file) != expected["sha256"]:
            raise BackupError("Backup checksum verification failed.")
    role = json.loads((path / "database-role.json").read_text())
    if not isinstance(role, dict) or role.get("name") != manifest["database_role"]:
        raise BackupError("Database role metadata is invalid.")
    # Read all compressed data, not just the tar directory or dump table of contents.
    with gzip.open(path / "files.tar.gz", "rb") as source:
        for _ in iter(lambda: source.read(1024 * 1024), b""):
            pass
    with tarfile.open(path / "files.tar.gz", "r:gz") as archive:
        for member in archive:
            safe_member(member)
    with (path / "database.dump").open("rb") as source:
        runner(["pg_restore", "--exit-on-error", "--file=/dev/null"], stdin=source)
    return manifest


@contextmanager
def backup_lock(base: Path):
    private_directory(base)
    descriptor = os.open(base / ".lock", os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0), 0o600)
    try:
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            raise BackupError("Invalid backup lock.")
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise BackupError("Another backup is already running.") from None
        yield
    finally:
        os.close(descriptor)


def completed(base: Path) -> list[Path]:
    return sorted(
        (path for path in base.iterdir() if FINAL_NAME.fullmatch(path.name)
         and not path.is_symlink() and path.is_dir()
         and (path / "manifest.json").is_file() and not (path / "manifest.json").is_symlink()),
        key=lambda path: path.name, reverse=True,
    )


def create_backup(config: dict, runner) -> tuple[Path, int]:
    validate_config(config)
    base = Path(config["destination"])
    with backup_lock(base):
        # A killed job can leave only its private, unpublished staging directory.
        for path in base.iterdir():
            if PENDING_NAME.fullmatch(path.name) and not path.is_symlink() and path.is_dir():
                shutil.rmtree(path)
        stage = base / (".pending-" + uuid.uuid4().hex)
        stage.mkdir(mode=0o700)
        published = None
        try:
            with (stage / "database.dump").open("xb") as output:
                (stage / "database.dump").chmod(0o600)
                runner(["pg_dump", "--no-password", "--format=custom", "--lock-wait-timeout=60s",
                        "--dbname=" + config["database"]], stdout=output)
                output.flush()
                os.fsync(output.fileno())
            role_name = config["database_role"]  # validated identifier, never a connection URL
            sql = ("SELECT json_build_object('name',rolname,'password',rolpassword,"
                   "'login',rolcanlogin,'superuser',rolsuper,'createdb',rolcreatedb,"
                   "'createrole',rolcreaterole,'replication',rolreplication,'bypassrls',rolbypassrls,"
                   "'inherit',rolinherit,'connection_limit',rolconnlimit,'valid_until',rolvaliduntil)"
                   " FROM pg_authid WHERE rolname='" + role_name + "'")
            role = json.loads(runner(["psql", "-X", "-w", "-A", "-t", "-v", "ON_ERROR_STOP=1", "-c", sql]))
            if not isinstance(role, dict) or role.get("name") != role_name:
                raise BackupError("The configured broker database role does not exist.")
            write_json(stage / "database-role.json", role)
            archive_files(config, stage / "files.tar.gz")
            now = datetime.now(timezone.utc)
            manifest = {
                "version": 1, "created_at": now.isoformat(), "database": config["database"],
                "database_role": role_name,
                "postgres_version": runner(["pg_dump", "--version"]).decode().strip(),
                "files": {name: {"bytes": (stage / name).stat().st_size, "sha256": digest(stage / name)} for name in FILES},
            }
            write_json(stage / "manifest.json", manifest)
            verify_backup(stage, runner)
            sync_directory(stage)
            name = "backup-" + now.strftime("%Y%m%dT%H%M%S.%fZ-") + uuid.uuid4().hex[:8]
            published = base / name
            os.rename(stage, published)
            sync_directory(base)
        finally:
            if stage.exists():
                shutil.rmtree(stage)
        # Publication and successful verification always precede deletion.
        older = [path for path in completed(base) if path != published]
        for path in older[config["keep"] - 1:]:
            shutil.rmtree(path)
        sync_directory(base)
        return published, len(completed(base))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=Path("/etc/clex-mqtt/backup.json"))
    sub = parser.add_subparsers(dest="action", required=True)
    sub.add_parser("create")
    sub.add_parser("list")
    verify = sub.add_parser("verify")
    verify.add_argument("directory", type=Path)
    args = parser.parse_args()
    if os.geteuid() != 0:
        raise BackupError("Run the installed backup tool as root.")
    os.umask(0o077)
    config = load_config(args.config)
    runner = DatabaseRunner(config)
    if args.action == "create":
        path, count = create_backup(config, runner)
        print(f"Backup: OK\nDirectory: {path}\nCompleted copies: {count}")
    elif args.action == "verify":
        verify_backup(args.directory, runner)
        print("Verify: OK")
    else:
        base = Path(config["destination"])
        with backup_lock(base):
            for path in completed(base):
                size = sum(file.stat().st_size for file in path.iterdir() if file.is_file())
                print(f"{path.name}  {size} bytes")
    return 0


def interrupted(signum, frame):
    raise BackupError("Backup interrupted; unpublished files will be removed.")


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    try:
        sys.exit(main())
    except (BackupError, OSError, ValueError, KeyError, tarfile.TarError) as error:
        # Do not print configuration, database URLs, subprocess output or role metadata.
        message = error_message(error)
        print(message, file=sys.stderr)
        sys.exit(1)
