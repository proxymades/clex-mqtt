#!/usr/bin/env python3
"""Scan Git candidates without printing matching credential contents."""
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
EXAMPLES = {".env.example", ".env.production.example"}
PATTERNS = (
    ("private key", re.compile(rb"-----BEGIN (?:RSA |EC |OPENSSH |DSA )?PRIVATE KEY-----")),
    ("GitHub credential", re.compile(rb"(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,})")),
    ("AWS access key", re.compile(rb"(?:AKIA|ASIA)[A-Z0-9]{16}")),
    ("credential in connection URL", re.compile(rb"(?:postgres(?:ql)?|mysql|mongodb(?:\+srv)?|redis)://[^\s/:]+:([^\s@]+)@")),
)

def main() -> int:
    result = subprocess.run(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=ROOT, capture_output=True, check=True)
    failures = []
    for raw in result.stdout.split(b"\0"):
        if not raw:
            continue
        relative = raw.decode()
        path = ROOT / relative
        if not path.is_file():
            continue
        name = path.name
        if (name.startswith(".env") and name not in EXAMPLES) or name in {"database-role.json", "files.tar.gz"} or path.suffix in {".pem", ".key", ".p12", ".pfx", ".pgpass", ".dump"}:
            failures.append((relative, "private runtime file"))
            continue
        data = path.read_bytes()
        if b"\0" in data or len(data) > 5_000_000:
            continue
        for label, pattern in PATTERNS:
            for match in pattern.finditer(data):
                if label == "credential in connection URL":
                    password = match.group(1)
                    if password in {b"PASSWORD", b"CHANGE_ME", b"test-only-password", b"test-password"}:
                        continue
                    if re.fullmatch(rb"\{[a-z_][a-z0-9_]*\}|\$\{[A-Z_][A-Z0-9_]*\}", password):
                        continue
                    # Test suites receive URLs from the environment; literal production credentials are forbidden.
                failures.append((relative, label))
                break
    for relative, reason in sorted(set(failures)):
        print(f"BLOCKED: {relative}: {reason}", file=sys.stderr)
    if failures:
        return 1
    print("Secret check passed: Git candidates contain no recognized credentials or private runtime files.")
    return 0

if __name__ == "__main__":
    sys.exit(main())
