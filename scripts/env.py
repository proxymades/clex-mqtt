"""Load simple local dotenv assignments without evaluating shell expressions."""
from pathlib import Path
import os

ROOT = Path(__file__).resolve().parents[1]

def local_environment() -> dict[str, str]:
    env = dict(os.environ)
    file = ROOT / ".env"
    if file.exists():
        for line in file.read_text().splitlines():
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            key, separator, value = line.partition("=")
            if separator and key.strip() not in env:
                env[key.strip()] = value.strip().strip("\"'")
    temporary = ROOT / ".cache" / "tmp"
    temporary.mkdir(parents=True, exist_ok=True)
    env["TMPDIR"] = str(temporary)
    env.setdefault("XDG_CACHE_HOME", str(ROOT / ".cache"))
    return env
