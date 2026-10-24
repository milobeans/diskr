import unittest
from unittest import mock
import plistlib
from types import SimpleNamespace

import benchmarks


class MeasurementParsingTests(unittest.TestCase):
    def test_duration_units_preserve_nanoseconds(self):
        for value, expected in [("12ns", 12), ("1.234µs", 1234), ("1.234us", 1234), ("1.234567ms", 1234567), ("1.000000001s", 1000000001)]:
            with self.subTest(value=value):
                self.assertEqual(benchmarks.duration_ns(value), expected)
        with self.assertRaises(ValueError):
            benchmarks.duration_ns("unknown")

    def test_parses_interleaved_runner_lines_arrays_and_dimensions(self):
        records = benchmarks.parse_measurements("""test bulkstat::tests::benchmark_filesystem_boundaries ... boundary_fixture: median_us=10 p95_us=11 direct_scanner_syscalls=[326, 326]
heap_lazy=true: median_us=20 p95_us=30
packages=10000 frames=200 frame_p50_us=150 frame_p95_us=180 allocations_p95=1490
files=100000 aggregate_cold_us=12 cold_allocations=0 cached_p95_ns=33 cached_allocations=0
listing_harness_rss_bytes=123456
pip index: 100 distributions, one enumeration in 1.25ms, 100 lookups in 12.4µs; repeated enumeration in 0.25s
native_10002_files: first_paint_us=100 first_row_us=200 idle_seconds=5.002 idle_cpu_ns=999 rss_bytes=123 cpu_percent=0.001
test result: ok. 5 passed; 0 failed; 0 ignored
""")
        self.assertEqual(len(records), 7)
        self.assertEqual(records[0]["metrics"]["direct_scanner_syscalls"], [326, 326])
        self.assertEqual(records[1]["dimensions"], {"lazy": True})
        self.assertEqual(records[2]["dimensions"], {"packages": 10000, "frames": 200})
        self.assertEqual(records[3]["metrics"]["cached_p95_ns"], 33)
        self.assertEqual(records[5]["metrics"], {"build_ns": 1250000, "lookup_ns": 12400, "repeated_enumeration_ns": 250000000})
        self.assertEqual(records[6]["metrics"]["idle_seconds"], 5.002)
        self.assertEqual(records[6]["metrics"]["cpu_percent"], 0.001)

    def test_missing_measurements_are_not_a_successful_empty_run(self):
        errors = benchmarks.measurement_errors([])
        self.assertEqual(len(errors), 15)
        self.assertIn("pip_index: expected 3 measurement records, got 0", errors)

    def test_changed_metric_names_and_duplicate_dimensions_fail_validation(self):
        records = [
            {"series": "heap_path", "dimensions": {"lazy": True}, "metrics": {"median_us": 1, "p95_us": 2}},
            {"series": "heap_path", "dimensions": {"lazy": True}, "metrics": {"median_us": 1, "p95_us": 2}},
            {"series": "raw_single", "dimensions": {}, "metrics": {"median_us": 1, "p95_us": 2}},
        ]
        errors = benchmarks.measurement_errors(records)
        self.assertIn("raw_single: missing metrics: samples", errors)
        self.assertTrue(any("heap_path: expected exactly one record" in error for error in errors))

    def test_filesystem_metadata_uses_the_fixture_device_not_its_directory(self):
        def capture(command):
            if command[0] == "df":
                return "Filesystem 512-blocks Used Available Capacity Mounted on\n/dev/disk9s1 100 50 50 50% /Volumes/Fixture Disk"
            if command == ["sysctl", "-n", "hw.memsize"]:
                return "1024"
            return "fixture"

        result = SimpleNamespace(returncode=0, stdout=plistlib.dumps({"FilesystemType": "apfs", "SolidState": True}))
        with mock.patch.object(benchmarks, "capture", side_effect=capture), mock.patch.object(benchmarks.subprocess, "run", return_value=result) as run:
            metadata = benchmarks.metadata(5)
        run.assert_called_once_with(["diskutil", "info", "-plist", "/dev/disk9s1"], capture_output=True, timeout=30)
        self.assertEqual(metadata["fixture_filesystem"], {"FilesystemType": "apfs", "SolidState": True})
        self.assertIsNone(metadata["fixture_filesystem_diagnostic"])


if __name__ == "__main__":
    unittest.main()
