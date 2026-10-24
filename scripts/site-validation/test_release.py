#!/usr/bin/env python3
"""Exercise website release gating without network or publication."""

import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
from urllib.request import Request

from check_release import (ARCHIVES, ReleaseCheckError, Response, SafeRedirect,
                           check_release, github_fetcher, main)

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "distribution"))
from prepare import prepare

VERSION = "0.1.84"
REPOSITORY = "milobeans/diskr"
API = f"https://api.github.com/repos/{REPOSITORY}/releases/tags/v{VERSION}"
DOWNLOAD = f"https://github.com/{REPOSITORY}/releases/download/v{VERSION}/"


class Fixture:
    def __init__(self, signed=False):
        with tempfile.TemporaryDirectory(prefix="diskr-readiness-test-") as temporary:
            root = Path(temporary)
            installer = root / "template.sh"
            installer.write_text("#!/bin/sh\nrelease_version=''\n")
            for archive in ARCHIVES:
                (root / archive).write_bytes(archive.encode())
                names = [archive]
                if signed:
                    image = archive.removesuffix(".tar.gz") + ".dmg"
                    (root / image).write_bytes(b"signed image fixture")
                    names.append(image)
                (root / archive.removeprefix("diskr-").replace(".tar.gz", ".sha256")).write_text("".join(
                    f"{hashlib.sha256((root / name).read_bytes()).hexdigest()}  {name}\n" for name in names))
            prepare(VERSION, root, installer, signed)
            names = [line.split("  ")[1] for line in (root / "SHA256SUMS").read_text().splitlines()] + ["SHA256SUMS"]
            self.payloads = {name: (root / name).read_bytes() for name in names}
        self.release = {"tag_name": f"v{VERSION}", "draft": False, "prerelease": False,
                        "published_at": "2026-10-24T12:00:00Z"}
        self.responses = {}
        self.calls = []
        self.sync()

    def sync(self, checksums=False):
        if checksums:
            self.payloads["SHA256SUMS"] = "".join(
                f"{hashlib.sha256(body).hexdigest()}  {name}\n"
                for name, body in sorted(self.payloads.items()) if name != "SHA256SUMS").encode()
        self.release["assets"] = [{"name": name, "state": "uploaded", "size": len(body)}
                                  for name, body in self.payloads.items()]

    def fetch(self, url, limit):
        self.calls.append((url, limit))
        if url in self.responses:
            return self.responses[url]
        if url == API:
            return Response(200, json.dumps(self.release).encode())
        if url.startswith(DOWNLOAD) and url[len(DOWNLOAD):] in self.payloads:
            return Response(200, self.payloads[url[len(DOWNLOAD):]])
        raise AssertionError(f"Unexpected request: {url}")

    def check(self):
        return check_release(VERSION, REPOSITORY, self.fetch)


class ReadinessTests(unittest.TestCase):
    def test_assets_from_unsigned_and_signed_preparation_are_ready(self):
        for signed in (False, True):
            with self.subTest(signed=signed):
                fixture = Fixture(signed)
                self.assertTrue(fixture.check().deploy)
                self.assertEqual(len(fixture.calls), 5)
                self.assertFalse(any(url.endswith((".tar.gz", ".dmg")) for url, _ in fixture.calls))

    def test_release_404_holds_successfully(self):
        fixture = Fixture()
        fixture.responses[API] = Response(404, b"")
        self.assertFalse(fixture.check().deploy)
        self.assertIn("HOLD", fixture.check().message)

    def test_each_missing_required_asset_holds_before_download(self):
        for name in Fixture().payloads:
            with self.subTest(name=name):
                fixture = Fixture()
                fixture.release["assets"] = [a for a in fixture.release["assets"] if a["name"] != name]
                self.assertFalse(fixture.check().deploy)
                self.assertEqual(len(fixture.calls), 1)

    def test_zero_size_or_unfinished_upload_holds(self):
        for field, value in (("size", 0), ("state", "starter")):
            fixture = Fixture()
            fixture.release["assets"][0][field] = value
            self.assertFalse(fixture.check().deploy)

    def test_asset_download_404_holds(self):
        fixture = Fixture()
        fixture.responses[DOWNLOAD + "install-diskr.sh"] = Response(404, b"")
        self.assertFalse(fixture.check().deploy)

    def test_draft_prerelease_and_unpublished_release_hold(self):
        for field, value in (("draft", True), ("prerelease", True), ("published_at", None)):
            fixture = Fixture()
            fixture.release[field] = value
            self.assertFalse(fixture.check().deploy)

    def test_unexpected_http_status_and_network_errors_fail(self):
        for url in (API, DOWNLOAD + "distribution.json"):
            for status in (403, 429, 500):
                fixture = Fixture()
                fixture.responses[url] = Response(status, b"")
                with self.assertRaisesRegex(ReleaseCheckError, f"HTTP {status}"):
                    fixture.check()
        with patch("check_release.build_opener") as opener:
            opener.return_value.open.side_effect = OSError("offline")
            with self.assertRaisesRegex(ReleaseCheckError, "request failed"):
                github_fetcher(None)(API, 1024)

    def test_malformed_api_and_distribution_json_fail(self):
        for url in (API, DOWNLOAD + "distribution.json"):
            fixture = Fixture()
            body = b"not JSON"
            fixture.responses[url] = Response(200, body)
            if url != API:
                next(a for a in fixture.release["assets"] if a["name"] == "distribution.json")["size"] = len(body)
            with self.assertRaisesRegex(ReleaseCheckError, "Malformed"):
                fixture.check()

    def test_wrong_version_targets_and_signing_policy_fail(self):
        for key, value in (("version", "0.1.83"), ("archives", [ARCHIVES[0]]),
                           ("minimum_macos", "14.0"), ("developer_id_signed", True),
                           ("apple_notarized", "false"), ("stapled_disk_images", True),
                           ("standalone_binary_ticket", "online")):
            with self.subTest(key=key):
                fixture = Fixture()
                metadata = json.loads(fixture.payloads["distribution.json"])
                metadata[key] = value
                fixture.payloads["distribution.json"] = json.dumps(metadata).encode()
                fixture.sync(checksums=True)
                with self.assertRaisesRegex(ReleaseCheckError, "Distribution"):
                    fixture.check()

    def test_signed_distribution_waits_for_both_disk_images(self):
        for archive in ARCHIVES:
            fixture = Fixture(True)
            missing = archive.removesuffix(".tar.gz") + ".dmg"
            fixture.release["assets"] = [a for a in fixture.release["assets"] if a["name"] != missing]
            result = fixture.check()
            self.assertFalse(result.deploy)
            self.assertIn(missing, result.message)

    def test_invalid_api_metadata_fails(self):
        for key, value in (("tag_name", "v0.1.83"), ("assets", {}), ("draft", "false"),
                           ("published_at", "invalid")):
            fixture = Fixture()
            fixture.release[key] = value
            with self.assertRaises(ReleaseCheckError):
                fixture.check()
        fixture = Fixture()
        del fixture.release["published_at"]
        with self.assertRaisesRegex(ReleaseCheckError, "publication metadata is missing"):
            fixture.check()
        fixture = Fixture()
        fixture.release["assets"][0]["size"] = "42"
        with self.assertRaisesRegex(ReleaseCheckError, "size/state"):
            fixture.check()

    def test_checksum_mismatch_and_missing_checksum_fail(self):
        fixture = Fixture()
        fixture.payloads["install-diskr.sh"] += b"# changed\n"
        fixture.sync()
        with self.assertRaisesRegex(ReleaseCheckError, "checksum mismatch"):
            fixture.check()
        fixture = Fixture()
        fixture.payloads["SHA256SUMS"] = fixture.payloads["SHA256SUMS"].split(b"\n", 1)[1]
        fixture.sync()
        with self.assertRaisesRegex(ReleaseCheckError, "checksums do not cover"):
            fixture.check()

    def test_checksum_valid_but_stale_installer_fails(self):
        fixture = Fixture()
        fixture.payloads["install-diskr.sh"] = fixture.payloads["install-diskr.sh"].replace(VERSION.encode(), b"0.1.83")
        fixture.sync(checksums=True)
        with self.assertRaisesRegex(ReleaseCheckError, "not pinned"):
            fixture.check()

    def test_oversized_response_fails(self):
        fixture = Fixture()
        fixture.responses[DOWNLOAD + "distribution.json"] = Response(200, b"x" * (32 * 1024 + 1))
        with self.assertRaisesRegex(ReleaseCheckError, "size bound"):
            fixture.check()

    def test_authorization_is_limited_to_api_host_and_removed_on_redirect(self):
        with patch("check_release.build_opener") as opener:
            response = opener.return_value.open.return_value.__enter__.return_value
            response.status, response.read.return_value = 200, b"{}"
            fetch = github_fetcher("fixture-token")
            fetch(API, 1024)
            self.assertEqual(opener.return_value.open.call_args.args[0].get_header("Authorization"), "Bearer fixture-token")
            fetch(DOWNLOAD + "distribution.json", 1024)
            self.assertIsNone(opener.return_value.open.call_args.args[0].get_header("Authorization"))
        request = Request(API, headers={"Authorization": "Bearer fixture-token"})
        redirected = SafeRedirect().redirect_request(request, None, 302, "Found", {}, "https://release-assets.githubusercontent.com/download")
        self.assertIsNone(redirected.get_header("Authorization"))
        for url in ("https://untrusted.example/asset", "http://github.com/asset"):
            with self.assertRaises(ReleaseCheckError):
                SafeRedirect().redirect_request(request, None, 302, "Found", {}, url)

    def test_cli_outputs_ready_hold_and_error_without_ambiguity(self):
        for status, expected_code, expected_deploy in ((200, 0, "true"), (404, 0, "false"), (500, 1, "false")):
            with self.subTest(status=status), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                manifest = root / "Cargo.toml"
                manifest.write_text(f'[package]\nname = "diskr"\nversion = "{VERSION}"\n')
                fixture = Fixture()
                if status != 200:
                    fixture.responses[API] = Response(status, b"")
                environment = {"GITHUB_OUTPUT": str(root / "output"), "GITHUB_STEP_SUMMARY": str(root / "summary")}
                code = main(["--manifest", str(manifest)], fixture.fetch, environment)
                self.assertEqual(code, expected_code)
                self.assertEqual((root / "output").read_text(), f"deploy={expected_deploy}\n")
                self.assertIn({200: "READY", 404: "HOLD", 500: "ERROR"}[status], (root / "summary").read_text())


if __name__ == "__main__":
    unittest.main()
