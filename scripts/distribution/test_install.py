#!/usr/bin/env python3
"""Exercise the native installer with local archives and injected system probes."""

import hashlib
import io
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tarfile
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "install.sh"
VERSION = "0.1.83"


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="diskr-installer-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.prefix = self.root / "prefix with spaces"
        self.home = self.root / "home"
        self.home.mkdir()
        self.tools = self.root / "tools"
        self.tools.mkdir()
        self.assets = self.root / "assets"
        self.assets.mkdir()
        self.curl_log = self.root / "curl.log"
        self.env = dict(os.environ, HOME=str(self.home), TMPDIR=str(self.root),
                        PATH=f"{self.tools}:/usr/bin:/bin:/usr/sbin:/sbin")
        self.tool("uname", 'case "$1" in -s) echo Darwin ;; -m) echo arm64 ;; esac')
        self.tool("sw_vers", "echo 15.0")
        self.tool("curl", f'''
out=''
while [ "$#" -gt 0 ]; do
  case "$1" in --output) shift; out=$1 ;; https://*) url=$1 ;; esac
  shift
done
printf '%s\\n' "$url" >> {shlex.quote(str(self.curl_log))}
cp {shlex.quote(str(self.assets))}/"${{url##*/}}" "$out"
''')
        self.archive()

    def tool(self, name, body):
        path = self.tools / name
        path.write_text("#!/bin/sh\nset -eu\n" + body + "\n")
        path.chmod(0o755)

    def archive(self, *, version=VERSION, target="aarch64-apple-darwin", symlink=False):
        name = f"diskr-{target}.tar.gz"
        path = self.assets / name
        with tarfile.open(path, "w:gz") as archive:
            if symlink:
                member = tarfile.TarInfo("diskr")
                member.type = tarfile.SYMTYPE
                member.linkname = "LICENSE"
                archive.addfile(member)
            else:
                data = f'#!/bin/sh\nprintf "diskr {version}\\n"\n'.encode()
                member = tarfile.TarInfo("diskr")
                member.size = len(data)
                member.mode = 0o755
                archive.addfile(member, io.BytesIO(data))
            for name, data in [("LICENSE", b"MIT\n"), ("licenses/index.json", b"[]\n")]:
                member = tarfile.TarInfo(name)
                member.size = len(data)
                member.mode = 0o644
                archive.addfile(member, io.BytesIO(data))
        (self.assets / "SHA256SUMS").write_text(
            hashlib.sha256(path.read_bytes()).hexdigest() + "  " + path.name + "\n"
        )

    def run_install(self, *args, script=SCRIPT):
        return subprocess.run(["/bin/sh", str(script), *args], env=self.env,
                              text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              timeout=10)

    def install(self, **kwargs):
        return self.run_install("--version", VERSION, "--prefix", str(self.prefix), **kwargs)

    def test_native_install_checks_version_and_preserves_license_inventory(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        executable = self.prefix / "bin/diskr"
        self.assertEqual(subprocess.check_output([str(executable), "--version"], text=True),
                         f"diskr {VERSION}\n")
        self.assertTrue((self.prefix / f"share/diskr/{VERSION}/licenses/index.json").is_file())
        self.assertIn("Add this directory", result.stdout)
        urls = self.curl_log.read_text().splitlines()
        self.assertEqual(len(urls), 2)
        self.assertTrue(all(f"/download/v{VERSION}/" in url for url in urls))
        self.assertFalse(list(self.root.glob("diskr-install.*")))
        self.assertFalse(list((self.prefix / "bin").glob(".diskr.*")))

    def test_intel_archive_is_selected(self):
        self.tool("uname", 'case "$1" in -s) echo Darwin ;; -m) echo x86_64 ;; esac')
        self.archive(target="x86_64-apple-darwin")
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("diskr-x86_64-apple-darwin.tar.gz", self.curl_log.read_text())

    def test_release_installer_defaults_to_its_immutable_release(self):
        packaged = self.root / "install-diskr.sh"
        packaged.write_text(SCRIPT.read_text().replace("release_version=''", f"release_version='{VERSION}'", 1))
        result = self.run_install(script=packaged)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.home / ".local/bin/diskr").is_file())

    def test_bad_checksum_does_not_replace_existing_executable(self):
        installed = self.prefix / "bin/diskr"
        installed.parent.mkdir(parents=True)
        installed.write_text("previous installation")
        (self.assets / "SHA256SUMS").write_text("0" * 64 + "  diskr-aarch64-apple-darwin.tar.gz\n")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(installed.read_text(), "previous installation")
        self.assertFalse(list(self.root.glob("diskr-install.*")))

    def test_duplicate_or_missing_checksums_are_rejected(self):
        checksums = self.assets / "SHA256SUMS"
        valid = checksums.read_text()
        for invalid in (valid + valid, "", valid.replace("diskr-aarch64", "different-aarch64")):
            with self.subTest(checksums=invalid):
                checksums.write_text(invalid)
                self.assertNotEqual(self.install().returncode, 0)
                self.assertFalse((self.prefix / "bin/diskr").exists())

    def test_mismatched_release_executable_is_rejected(self):
        self.archive(version="0.0.1")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("version does not match", result.stderr)
        self.assertFalse((self.prefix / "bin/diskr").exists())

    def test_symlink_executable_is_rejected(self):
        self.archive(symlink=True)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("regular executable", result.stderr)

    def test_unsupported_platforms_fail_before_download(self):
        for system, arch, version in [("Linux", "x86_64", "15"), ("Darwin", "arm64", "14"),
                                      ("Darwin", "unknown", "15")]:
            with self.subTest(system=system, arch=arch, version=version):
                self.tool("uname", f'case "$1" in -s) echo {system} ;; -m) echo {arch} ;; esac')
                self.tool("sw_vers", f"echo {version}")
                self.assertNotEqual(self.install().returncode, 0)
                self.assertFalse(self.curl_log.exists())

    def test_invalid_arguments_fail_before_download(self):
        for args in [("--version",), ("--version", "1.2.3/evil"), ("--unknown",),
                     ("--version", VERSION, "--prefix", "relative"),
                     ("--version", VERSION, "--prefix", "/")]:
            with self.subTest(args=args):
                self.assertNotEqual(self.run_install(*args).returncode, 0)
                self.assertFalse(self.curl_log.exists())

    def test_failed_download_preserves_existing_installation(self):
        self.tool("curl", "exit 22")
        installed = self.prefix / "bin/diskr"
        installed.parent.mkdir(parents=True)
        installed.write_text("previous installation")
        self.assertNotEqual(self.install().returncode, 0)
        self.assertEqual(installed.read_text(), "previous installation")

    def test_installation_does_not_modify_shell_startup(self):
        startup = self.home / ".zshrc"
        startup.write_text("# preserved user configuration\n")
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(startup.read_text(), "# preserved user configuration\n")

    def test_directory_created_during_replacement_is_preserved_without_nested_binary(self):
        destination = self.prefix / "bin/diskr"
        self.tool("mv", f'''
mkdir {shlex.quote(str(destination))}
printf 'preserve me\\n' > {shlex.quote(str(destination / 'sentinel'))}
/bin/mv "$@"
''')
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("Installed diskr", result.stdout)
        self.assertIn("no regular executable was installed", result.stderr)
        self.assertTrue(destination.is_dir())
        self.assertEqual((destination / "sentinel").read_text(), "preserve me\n")
        self.assertEqual([path.name for path in destination.iterdir()], ["sentinel"])
        self.assertFalse(list(destination.parent.glob(".diskr.*")))
        self.assertFalse(list(self.root.glob("diskr-install.*")))

    def test_symlink_directory_created_during_replacement_is_not_followed(self):
        destination = self.prefix / "bin/diskr"
        other_directory = self.root / "other directory"
        other_directory.mkdir()
        (other_directory / "sentinel").write_text("preserve me\n")
        self.tool("mv", f'''
ln -s {shlex.quote(str(other_directory))} {shlex.quote(str(destination))}
/bin/mv "$@"
''')
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(destination.is_file())
        self.assertFalse(destination.is_symlink())
        self.assertEqual(subprocess.check_output([str(destination), "--version"], text=True),
                         f"diskr {VERSION}\n")
        self.assertEqual((other_directory / "sentinel").read_text(), "preserve me\n")
        self.assertEqual([path.name for path in other_directory.iterdir()], ["sentinel"])
        self.assertFalse(list(destination.parent.glob(".diskr.*")))
        self.assertFalse(list(self.root.glob("diskr-install.*")))

    def test_version_changed_during_replacement_does_not_report_success(self):
        destination = self.prefix / "bin/diskr"
        self.tool("mv", f'''
/bin/mv "$@"
printf '#!/bin/sh\\nprintf "diskr 0.0.1\\\\n"\\n' > {shlex.quote(str(destination))}
''')
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("Installed diskr", result.stdout)
        self.assertIn("installed executable version does not match", result.stderr)

    def test_help_needs_no_platform_probe(self):
        self.tool("uname", "exit 1")
        self.assertEqual(self.run_install("--help").returncode, 0)


if __name__ == "__main__":
    unittest.main()
