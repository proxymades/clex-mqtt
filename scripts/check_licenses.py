#!/usr/bin/env python3
"""Verify that third-party legal notices match the committed lock files."""
from pathlib import Path
import hashlib
import json
import re
import sys

ROOT = Path(__file__).resolve().parents[1]


def main() -> int:
    manifest = json.loads((ROOT / "third-party/manifest.json").read_text())
    if manifest["formatVersion"] != 1:
        raise ValueError("Unsupported license inventory format")
    for name in ("Cargo.lock", "package-lock.json"):
        digest = hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
        if manifest["lockFiles"].get(name) != digest:
            raise ValueError(f"Refresh third-party notices after updating {name}")
    expected = set()
    for block in (ROOT / "Cargo.lock").read_text().split("[[package]]")[1:]:
        if not re.search(r"^source = ", block, re.MULTILINE):
            continue
        name = re.search(r'^name = "([^"]+)"', block, re.MULTILINE)
        version = re.search(r'^version = "([^"]+)"', block, re.MULTILINE)
        if name is None or version is None:
            raise ValueError("Invalid Cargo lock entry")
        expected.add(("cargo", name[1], version[1]))
    lock = json.loads((ROOT / "package-lock.json").read_text())
    for location, package in lock["packages"].items():
        if location and location != "frontend" and not package.get("link"):
            name = location.rsplit("node_modules/", 1)[-1]
            expected.add(("npm", name, package["version"]))
    actual = set()
    legal_paths = set()
    notices = (ROOT / "THIRD_PARTY_NOTICES.md").read_text()
    for package in manifest["packages"]:
        identity = (package["ecosystem"], package["name"], package["version"])
        actual.add(identity)
        if not package["license"] or not package["files"]:
            raise ValueError(f"Missing license declaration or legal texts: {identity}")
        for item in package["files"]:
            relative = Path(item["path"])
            if relative.parent.as_posix() != "third-party/licenses" or relative.suffix != ".txt":
                raise ValueError("Invalid legal text path")
            path = ROOT / relative
            if path.is_symlink() or not path.is_file():
                raise ValueError(f"Missing regular legal text: {relative}")
            if hashlib.sha256(path.read_bytes()).hexdigest() != item["sha256"]:
                raise ValueError(f"Changed upstream legal text: {relative}")
            if item["path"] not in notices:
                raise ValueError(f"Missing notice link: {relative}")
            legal_paths.add(relative.as_posix())
    if expected != actual:
        raise ValueError("Third-party package inventory differs from lock files")
    present = {p.relative_to(ROOT).as_posix() for p in (ROOT / "third-party/licenses").iterdir()}
    if present != legal_paths:
        raise ValueError("Unexpected or missing files in legal text directory")
    for name in ("package.json", "frontend/package.json"):
        if json.loads((ROOT / name).read_text()).get("license") != "MIT":
            raise ValueError(f"Project license metadata differs: {name}")
    if not re.search(r'^license = "MIT"$', (ROOT / "Cargo.toml").read_text(), re.MULTILINE):
        raise ValueError("Project Cargo license metadata differs")
    if not (ROOT / "LICENSE").read_text().startswith("MIT License\n"):
        raise ValueError("Missing MIT license")
    cargo = sum(p[0] == "cargo" for p in actual)
    npm = sum(p[0] == "npm" for p in actual)
    print(f"License inventory: OK ({cargo} Cargo, {npm} npm packages; {len(legal_paths)} legal texts)")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError) as error:
        sys.exit(f"License inventory failed: {error}")
