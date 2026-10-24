import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from formula import render_formula


ARM = "diskr-aarch64-apple-darwin.tar.gz"
INTEL = "diskr-x86_64-apple-darwin.tar.gz"
CHECKSUMS = f"{'a' * 64}  {ARM}\n{'b' * 64}  {INTEL}\n"


class FormulaTests(unittest.TestCase):
    def test_architecture_urls_and_checksums_stay_paired(self):
        formula = render_formula("1.2.3", CHECKSUMS)
        arm, intel = formula.split("  on_intel do", 1)
        self.assertIn(f"/v1.2.3/{ARM}", arm)
        self.assertIn(f'sha256 "{"a" * 64}"', arm)
        self.assertIn(f"/v1.2.3/{INTEL}", intel)
        self.assertIn(f'sha256 "{"b" * 64}"', intel)
        self.assertIn('pkgshare.install "licenses"', formula)
        self.assertIn("depends_on :macos", formula)
        self.assertIn("depends_on macos: :sequoia", formula)

    def test_rejects_missing_duplicate_or_malformed_archives(self):
        invalid = [
            CHECKSUMS.splitlines()[0],
            CHECKSUMS + CHECKSUMS.splitlines()[0] + "\n",
            CHECKSUMS.replace("a" * 64, "a" * 63),
            CHECKSUMS.replace(ARM, "../" + ARM),
            CHECKSUMS.replace(ARM, ARM + '"; system("bad")'),
        ]
        for checksums in invalid:
            with self.subTest(checksums=checksums), self.assertRaises(ValueError):
                render_formula("1.2.3", checksums)

    def test_rejects_versions_that_could_change_release_url_or_ruby(self):
        for version in ["v1.2.3", "01.2.3", "1.2", "1.2.3-beta", "1.2.3/../latest", '1.2.3"']:
            with self.subTest(version=version), self.assertRaises(ValueError):
                render_formula(version, CHECKSUMS)

    def test_binary_checksum_marker_and_unrelated_assets_are_supported(self):
        binary = CHECKSUMS.replace("  ", " *") + f"{'c' * 64}  install.sh\n"
        self.assertEqual(render_formula("1.2.3", CHECKSUMS), render_formula("1.2.3", binary))

    def test_invalid_input_does_not_replace_existing_formula(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            checksums = root / "SHA256SUMS"
            checksums.write_text("invalid")
            output = root / "diskr.rb"
            output.write_text("original")
            result = subprocess.run(
                [sys.executable, str(Path(__file__).with_name("formula.py")),
                 "--version", "1.2.3", "--checksums", str(checksums), "--output", str(output)],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(output.read_text(), "original")


if __name__ == "__main__":
    unittest.main()
