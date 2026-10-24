#!/usr/bin/env python3
"""Exercise signing failure and cleanup paths without accessing a real keychain."""

import base64
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("sign.sh")
SECRET_NAMES = [
    "MACOS_DEVELOPER_ID_P12",
    "MACOS_DEVELOPER_ID_PASSWORD",
    "MACOS_DEVELOPER_ID_IDENTITY",
    "APPLE_NOTARY_KEY_P8",
    "APPLE_NOTARY_KEY_ID",
    "APPLE_NOTARY_ISSUER_ID",
]


class SigningTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="diskr-sign-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "tools"
        self.bin.mkdir()
        self.payload = self.root / "payload"
        self.payload.mkdir()
        (self.payload / "diskr").write_text("#!/bin/sh\nexit 0\n")
        (self.payload / "diskr").chmod(0o755)
        self.runner = self.root / "runner"
        self.runner.mkdir()
        self.log = self.root / "calls.jsonl"
        self.env = os.environ.copy()
        for name in SECRET_NAMES:
            self.env.pop(name, None)
        self.env.update({
            "PATH": str(self.bin) + os.pathsep + os.environ.get("PATH", ""),
            "GITHUB_ACTIONS": "true",
            "RUNNER_TEMP": str(self.runner),
            "SIGN_TEST_LOG": str(self.log),
            "MACOS_DEVELOPER_ID_P12": base64.b64encode(b"test-p12").decode(),
            "MACOS_DEVELOPER_ID_PASSWORD": "test-secret-password",
            "MACOS_DEVELOPER_ID_IDENTITY": "Developer ID Application: Example (TESTTEAM)",
            "APPLE_NOTARY_KEY_P8": base64.b64encode(b"-----BEGIN PRIVATE KEY-----\ntest-only\n").decode(),
            "APPLE_NOTARY_KEY_ID": "TESTKEY123",
            "APPLE_NOTARY_ISSUER_ID": "01234567-89ab-cdef-0123-456789abcdef",
        })
        mock = '''#!/usr/bin/env python3
import json, os, pathlib, sys
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ["SIGN_TEST_LOG"], "a") as output:
    output.write(json.dumps([name, *args]) + "\\n")
if os.environ.get("SIGN_FAIL_COMMAND") == name:
    raise SystemExit(7)
if name == "hdiutil" and args[0] == "create":
    pathlib.Path(args[-1]).write_bytes(b"mock disk image")
if name == "xcrun" and args[:2] == ["notarytool", "submit"]:
    print(json.dumps({"id": "01234567-89ab-cdef-0123-456789abcdef", "status": os.environ.get("SIGN_NOTARY_STATUS", "Accepted")}))
elif name == "xcrun" and args[:2] == ["notarytool", "log"]:
    pathlib.Path(args[3]).write_text('{"issues": []}\\n')
'''
        for name in ["security", "codesign", "hdiutil", "xcrun", "spctl"]:
            tool = self.bin / name
            tool.write_text(mock)
            tool.chmod(0o755)

    def run_script(self, *extra):
        return subprocess.run([
            "sh", str(SCRIPT), "--binary-dir", str(self.payload),
            "--target", "aarch64-apple-darwin", "--output-dir", str(self.root / "output"), *extra,
        ], env=self.env, text=True, capture_output=True, timeout=30)

    def calls(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def assert_clean(self, result):
        self.assertEqual(list(self.runner.glob("diskr-sign.*")), [])
        for name in SECRET_NAMES:
            if name in self.env:
                self.assertNotIn(self.env[name], result.stdout + result.stderr)

    def test_success_verifies_notarization_staple_and_cleans_credentials(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertTrue(any(call[:3] == ["xcrun", "stapler", "validate"] for call in calls))
        self.assertTrue(any(call[:2] == ["spctl", "--assess"] for call in calls))
        self.assertEqual(calls[-1][:2], ["security", "delete-keychain"])
        self.assertTrue((self.root / "output" / "diskr-aarch64-apple-darwin.dmg").exists())
        self.assert_clean(result)

    def test_every_required_secret_is_checked_before_keychain_operations(self):
        for name in SECRET_NAMES:
            with self.subTest(name=name):
                value = self.env.pop(name)
                result = self.run_script()
                self.env[name] = value
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(name, result.stderr)
                self.assertEqual(self.calls(), [])
                self.assert_clean(result)

    def test_local_execution_and_development_identities_are_rejected(self):
        self.env["GITHUB_ACTIONS"] = "false"
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls(), [])
        self.env["GITHUB_ACTIONS"] = "true"
        self.env["MACOS_DEVELOPER_ID_IDENTITY"] = "Apple Development: Example"
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls(), [])
        self.assert_clean(result)

    def test_invalid_credentials_are_rejected_before_keychain_operations(self):
        self.env["APPLE_NOTARY_KEY_P8"] = "not-valid-base64"
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls(), [])
        self.assert_clean(result)

    def test_rejected_notarization_never_staples_or_claims_success(self):
        self.env["SIGN_NOTARY_STATUS"] = "Invalid"
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("notarization was not accepted", result.stderr)
        calls = self.calls()
        self.assertFalse(any(call[:2] == ["xcrun", "stapler"] for call in calls))
        self.assertEqual(calls[-1][:2], ["security", "delete-keychain"])
        self.assert_clean(result)

    def test_signing_failure_still_deletes_temporary_keychain(self):
        self.env["SIGN_FAIL_COMMAND"] = "codesign"
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls()[-1][:2], ["security", "delete-keychain"])
        self.assertFalse(any(call[:2] == ["xcrun", "notarytool"] for call in self.calls()))
        self.assert_clean(result)


if __name__ == "__main__":
    unittest.main()
