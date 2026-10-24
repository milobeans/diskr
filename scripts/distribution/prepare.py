#!/usr/bin/env python3
"""Assemble installation assets after verifying both native build artifacts."""

import argparse
import hashlib
import json
from pathlib import Path
import re

from formula import ARCHIVES, render_formula


def prepare(version: str, assets: Path, installer: Path, signed: bool) -> None:
    expected = set(ARCHIVES.values())
    if signed:
        expected.update(name.removesuffix(".tar.gz") + ".dmg" for name in ARCHIVES.values())
    checksums = "".join((assets / f"{target}.sha256").read_text(encoding="ascii")
                        for target in ("aarch64-apple-darwin", "x86_64-apple-darwin"))
    formula = render_formula(version, checksums)
    records = {}
    for line in checksums.splitlines():
        match = re.fullmatch(r"([0-9a-fA-F]{64}) [ *]([A-Za-z0-9_.-]+)", line)
        if not match:
            raise ValueError("invalid native build checksum")
        digest, name = match.groups()
        if name in records:
            raise ValueError(f"duplicate native build checksum: {name}")
        records[name] = digest.lower()
    if set(records) != expected:
        raise ValueError("native build assets do not match the requested signing mode")
    for name, digest in records.items():
        if hashlib.sha256((assets / name).read_bytes()).hexdigest() != digest:
            raise ValueError(f"native build checksum mismatch: {name}")
    source = installer.read_text()
    if source.count("release_version=''") != 1:
        raise ValueError("installer template must contain exactly one release version marker")
    (assets / "install-diskr.sh").write_text(source.replace("release_version=''", f"release_version='{version}'"))
    (assets / "diskr.rb").write_text(formula)
    (assets / "distribution.json").write_text(json.dumps({
        "version": version,
        "minimum_macos": "15.0",
        "developer_id_signed": signed,
        "apple_notarized": signed,
        "stapled_disk_images": signed,
        "standalone_binary_ticket": "online" if signed else "unavailable",
        "archives": sorted(ARCHIVES.values()),
    }, indent=2) + "\n")
    names = sorted(expected | {"install-diskr.sh", "diskr.rb", "distribution.json"})
    (assets / "SHA256SUMS").write_text("".join(
        f"{hashlib.sha256((assets / name).read_bytes()).hexdigest()}  {name}\n" for name in names
    ))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--assets-dir", type=Path, required=True)
    parser.add_argument("--installer", type=Path, required=True)
    parser.add_argument("--signed", action="store_true")
    args = parser.parse_args()
    try:
        prepare(args.version, args.assets_dir, args.installer, args.signed)
    except (OSError, UnicodeError, ValueError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
