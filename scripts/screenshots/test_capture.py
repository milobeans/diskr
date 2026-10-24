"""Pure-data and disposable-directory checks; never mount or touch user Trash."""
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import generate
import render


def cells(text):
    return [[ch, "default", "default", False, False] for ch in text]


class CaptureSafetyTests(unittest.TestCase):
    def test_confirmation_requires_matching_adjacent_modal_body(self):
        shown = "  │ Move to Trash:                 │\n  │ target                         │\n  │ Path: …/ledger-api/target       │"
        self.assertTrue(generate.confirmation_matches(shown, "Move to Trash:", "target", "ledger-api/target"))
        wrong = shown.replace("│ target ", "│ other  ") + "\ntarget ledger-api/target"
        self.assertFalse(generate.confirmation_matches(wrong, "Move to Trash:", "target", "ledger-api/target"))
        self.assertFalse(generate.confirmation_matches(shown, "Move to Trash:", "target", "another/target"))

    def test_mount_requires_matching_image_and_device(self):
        info = {"images": [{"image-path": "/tmp/owned.sparseimage", "system-entities": [
            {"mount-point": "/tmp/demo", "dev-entry": "/dev/disk99s1"}]}]}
        self.assertEqual(generate.owned_image_mount(info, "/tmp/owned.sparseimage", "/tmp/demo"),
                         "/dev/disk99s1")
        for image, mount in [("/tmp/unrelated.sparseimage", "/tmp/demo"),
                             ("/tmp/owned.sparseimage", "/tmp/elsewhere")]:
            with self.assertRaises(RuntimeError):
                generate.owned_image_mount(info, image, mount)

    def test_unverified_mount_is_never_detached(self):
        with patch("generate.os.path.ismount", return_value=True), \
                patch("generate.verify_demo_volume", side_effect=RuntimeError("not owned")), \
                patch("generate.subprocess.run") as run:
            with self.assertRaises(RuntimeError):
                generate.detach_volume()
            run.assert_not_called()

    def test_shim_only_removes_owned_allowlisted_packages(self):
        with tempfile.TemporaryDirectory(prefix="diskr capture's ") as temporary:
            root = Path(temporary)
            prefix = root / "demo's cellar"
            cellar = prefix / "Cellar"
            valid = cellar / "llvm@18"
            valid.mkdir(parents=True)
            (valid / "fixture").write_text("synthetic package")
            protected = root / "protected"
            protected.mkdir()
            (protected / "keep").write_text("untouched")
            (cellar / "llvm").symlink_to(protected, target_is_directory=True)
            shim = root / "brew"
            shim.write_text(generate.brew_shim(prefix))
            shim.chmod(0o755)

            def command(*args):
                return subprocess.run([str(shim), *args], capture_output=True, text=True, timeout=5)

            self.assertEqual(command("--prefix").stdout.strip(), str(prefix))
            self.assertNotEqual(command("uninstall", "../../protected").returncode, 0)
            self.assertNotEqual(command("uninstall", "llvm").returncode, 0)
            self.assertNotEqual(command("uninstall", "llvm@18", "extra").returncode, 0)
            self.assertTrue(valid.exists())
            self.assertEqual(command("uninstall", "llvm@18").returncode, 0)
            self.assertFalse(valid.exists())
            self.assertEqual((protected / "keep").read_text(), "untouched")

    def test_long_replacement_preserves_neighbor_and_row_width(self):
        row = cells("prefix …abcdefgh│")
        size = len(row)
        render.replace_in_row(row, "…abcdefgh", "/Volumes/Demo")
        self.assertEqual(len(row), size)
        self.assertEqual(render.row_text(row), "prefix /Volumes/│")
        edge = cells("…abcdefgh")
        render.replace_in_row(edge, "…abcdefgh", "/Users/demo")
        self.assertEqual(render.row_text(edge), "/Users/de")

    def test_masked_partial_home_does_not_become_a_package_prefix(self):
        masker = render.Masker([
            ("/Users/owner/work/Demo/opt/homebrew", "/opt/homebrew"),
            ("/Users/owner/work/Demo/home", "/Users/demo"),
            ("/Users/owner", "/Users/demo"),
        ], "owner")
        shown = masker.apply([cells("diskr · /Users/ow│ modal")])
        self.assertEqual(render.row_text(shown[0]), "diskr · /Users/de│ modal")
        self.assertEqual(masker.leaks(shown), [])

    def test_short_replacements_keep_later_columns_and_do_not_loop(self):
        row = cells("/old/path   42│")
        size = len(row)
        render.replace_in_row(row, "/old/path", "/demo")
        self.assertEqual(len(row), size)
        self.assertTrue(render.row_text(row).endswith("42│"))
        render.replace_in_row(row, "/demo", "/demo")
        self.assertEqual(len(row), size)


if __name__ == "__main__":
    unittest.main()
