#!/usr/bin/env python3
"""Generate the Homebrew formula from a release's verified binary checksums."""

import argparse
from pathlib import Path
import re
import sys


ARCHIVES = {
    "arm": "diskr-aarch64-apple-darwin.tar.gz",
    "intel": "diskr-x86_64-apple-darwin.tar.gz",
}


def render_formula(version: str, checksums: str) -> str:
    if not re.fullmatch(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", version):
        raise ValueError("version must be a stable X.Y.Z release number")
    hashes = {}
    for line in checksums.splitlines():
        if not line.strip():
            continue
        match = re.fullmatch(r"([a-fA-F0-9]{64}) [ *]([A-Za-z0-9_.-]+)", line)
        if not match:
            raise ValueError("invalid SHA256SUMS entry")
        digest, name = match.groups()
        if name in hashes:
            raise ValueError(f"duplicate checksum for {name}")
        hashes[name] = digest.lower()
    missing = sorted(set(ARCHIVES.values()) - hashes.keys())
    if missing:
        raise ValueError("missing archive checksums: " + ", ".join(missing))

    return f'''class Diskr < Formula
  desc "Explore disk usage, packages, and cleanup candidates in the macOS terminal"
  homepage "https://milobeans.github.io/diskr/"
  version "{version}"
  license "MIT"

  on_arm do
    url "https://github.com/milobeans/diskr/releases/download/v{version}/{ARCHIVES['arm']}"
    sha256 "{hashes[ARCHIVES['arm']]}"
  end

  on_intel do
    url "https://github.com/milobeans/diskr/releases/download/v{version}/{ARCHIVES['intel']}"
    sha256 "{hashes[ARCHIVES['intel']]}"
  end

  depends_on :macos
  depends_on macos: :sequoia

  def install
    bin.install "diskr"
    prefix.install "LICENSE"
    pkgshare.install "licenses"
  end

  test do
    assert_equal "diskr #{{version}}", shell_output("#{{bin}}/diskr --version").strip
    (testpath/"sample.txt").write "diskr test\\n"
    report = JSON.parse(shell_output("#{{bin}}/diskr --top 1 --json #{{testpath}}"))
    assert_equal 1, report.fetch("schema_version")
    assert_match "sample.txt", report.to_json
  end
end
'''


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True, help="stable release version without v")
    parser.add_argument("--checksums", required=True, type=Path, help="verified release SHA256SUMS")
    parser.add_argument("--output", type=Path, help="destination formula; defaults to stdout")
    args = parser.parse_args()
    try:
        output = render_formula(args.version, args.checksums.read_text(encoding="ascii"))
    except (OSError, UnicodeError, ValueError) as error:
        parser.error(str(error))
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(output, encoding="utf-8")
    else:
        sys.stdout.write(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
