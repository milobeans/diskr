#!/usr/bin/env python3
"""Verify release installation assets keep native artifact integrity intact."""

import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from prepare import prepare


class PreparationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="diskr-release-test-")
        self.addCleanup(temporary.cleanup)
        self.assets = Path(temporary.name)
        self.installer = self.assets / "template.sh"
        self.installer.write_text("#!/bin/sh\nrelease_version=''\n")
        self.targets = ("aarch64-apple-darwin", "x86_64-apple-darwin")
        for target in self.targets:
            name = f"diskr-{target}.tar.gz"
            (self.assets / name).write_bytes(target.encode())
            (self.assets / f"{target}.sha256").write_text(
                f"{hashlib.sha256(target.encode()).hexdigest()}  {name}\n")

    def test_unsigned_assets_have_pinned_version_and_truthful_metadata(self):
        prepare("0.1.83", self.assets, self.installer, False)
        self.assertIn("release_version='0.1.83'", (self.assets / "install-diskr.sh").read_text())
        self.assertFalse(json.loads((self.assets / "distribution.json").read_text())["apple_notarized"])
        names = set()
        for line in (self.assets / "SHA256SUMS").read_text().splitlines():
            digest, name = line.split("  ")
            self.assertEqual(hashlib.sha256((self.assets / name).read_bytes()).hexdigest(), digest)
            names.add(name)
        self.assertEqual(len(names), 5)
        self.assertIn("diskr.rb", names)

    def test_changed_native_artifact_is_rejected_before_emission(self):
        (self.assets / f"diskr-{self.targets[0]}.tar.gz").write_text("tampered")
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            prepare("0.1.83", self.assets, self.installer, False)
        self.assertFalse((self.assets / "install-diskr.sh").exists())

    def test_signing_mode_requires_both_disk_images(self):
        with self.assertRaisesRegex(ValueError, "signing mode"):
            prepare("0.1.83", self.assets, self.installer, True)

    def test_signed_assets_include_verified_disk_images(self):
        for target in self.targets:
            name = f"diskr-{target}.dmg"
            (self.assets / name).write_bytes(b"signed image")
            with (self.assets / f"{target}.sha256").open("a") as checksums:
                checksums.write(f"{hashlib.sha256(b'signed image').hexdigest()}  {name}\n")
        prepare("0.1.83", self.assets, self.installer, True)
        self.assertTrue(json.loads((self.assets / "distribution.json").read_text())["stapled_disk_images"])
        self.assertEqual(len((self.assets / "SHA256SUMS").read_text().splitlines()), 7)


if __name__ == "__main__":
    unittest.main()
