#!/usr/bin/env python3
"""Run the optimized fixture protocol and retain comparable measurement records."""

import argparse
import datetime
from decimal import Decimal
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parent.parent
BENCHMARKS = (
    "benchmark_streamed_listing",
    "benchmark_filesystem_boundaries",
    "benchmark_shared_scan_and_heap_paths",
    "benchmark_pip_dist_index_sizes",
    "benchmark_package_viewport_frames",
)
NATIVE_TEST = "native_tui_restores_terminal_after_filter_refresh_modal_and_quit"
NUMERIC_FIELD = re.compile(r"([a-zA-Z_][a-zA-Z_0-9]*)=([0-9]+(?:\.[0-9]+)?)(?![0-9.])")


def numeric_fields(text):
    return {
        name: float(value) if "." in value else int(value)
        for name, value in NUMERIC_FIELD.findall(text)
    }


def duration_ns(text):
    match = re.fullmatch(r"([0-9]+(?:\.[0-9]+)?)(ns|µs|us|ms|s)", text)
    if not match:
        raise ValueError(f"unrecognized Rust duration: {text}")
    units = {"ns": 1, "µs": 1000, "us": 1000, "ms": 1_000_000, "s": 1_000_000_000}
    return int(Decimal(match[1]) * units[match[2]])


def parse_measurements(log):
    records = []
    for raw_line in log.splitlines():
        line = re.sub(r"^test \S+ \.\.\. ", "", raw_line).strip()
        match = re.match(
            r"(boundary_fixture|raw_single|broker_single|raw_four|shared_four|first_row|listing_total|cursor|UI_batch_drain|native_10002_files): (.+)",
            line,
        )
        if match:
            values = numeric_fields(match[2])
            if match[1] == "boundary_fixture":
                calls = re.search(r"direct_scanner_syscalls=(\[[0-9, ]+\])", match[2])
                if calls:
                    values["direct_scanner_syscalls"] = json.loads(calls[1])
            records.append({"series": match[1], "dimensions": {}, "metrics": values})
            continue
        match = re.match(r"heap_lazy=(true|false): (.+)", line)
        if match:
            records.append({"series": "heap_path", "dimensions": {"lazy": match[1] == "true"}, "metrics": numeric_fields(match[2])})
            continue
        if line.startswith("listing_harness_rss_bytes="):
            records.append({"series": "listing_memory", "dimensions": {}, "metrics": numeric_fields(line)})
            continue
        if line.startswith("packages="):
            values = numeric_fields(line)
            dimensions = {key: values.pop(key) for key in ("packages", "frames")}
            records.append({"series": "package_viewport", "dimensions": dimensions, "metrics": values})
            continue
        if line.startswith("files="):
            values = numeric_fields(line)
            records.append({"series": "file_aggregates", "dimensions": {"files": values.pop("files")}, "metrics": values})
            continue
        match = re.match(
            r"pip index: (\d+) distributions, one enumeration in (\S+), (\d+) lookups in (\S+); repeated enumeration in (\S+)",
            line,
        )
        if match:
            records.append({
                "series": "pip_index",
                "dimensions": {"distributions": int(match[1]), "lookups": int(match[3])},
                "metrics": {"build_ns": duration_ns(match[2]), "lookup_ns": duration_ns(match[4]), "repeated_enumeration_ns": duration_ns(match[5])},
            })
    return records


def measurement_errors(records):
    expected = {
        "boundary_fixture": 1, "raw_single": 1, "broker_single": 1,
        "raw_four": 1, "shared_four": 1, "heap_path": 2,
        "first_row": 1, "listing_total": 1, "cursor": 1,
        "UI_batch_drain": 1, "listing_memory": 1, "package_viewport": 2,
        "file_aggregates": 1, "pip_index": 3, "native_10002_files": 1,
    }
    errors = []
    for series, count in expected.items():
        actual = sum(record["series"] == series for record in records)
        if actual != count:
            errors.append(f"{series}: expected {count} measurement records, got {actual}")
    fields = {
        "boundary_fixture": {"median_us", "p95_us", "direct_scanner_syscalls"},
        "heap_path": {"median_us", "p95_us"},
        "listing_memory": {"listing_harness_rss_bytes"},
        "package_viewport": {"frame_p50_us", "frame_p95_us", "allocations_p95"},
        "file_aggregates": {"aggregate_cold_us", "cold_allocations", "cached_p95_ns", "cached_allocations"},
        "pip_index": {"build_ns", "lookup_ns", "repeated_enumeration_ns"},
        "native_10002_files": {"first_paint_us", "first_row_us", "idle_seconds", "idle_cpu_ns", "rss_bytes", "cpu_percent"},
    }
    for record in records:
        required = fields.get(record["series"], {"median_us", "p95_us", "samples"})
        missing = required - record["metrics"].keys()
        if missing:
            errors.append(f"{record['series']}: missing metrics: {', '.join(sorted(missing))}")
    dimensions = {
        "heap_path": [{"lazy": False}, {"lazy": True}],
        "package_viewport": [{"packages": count, "frames": 200} for count in (100, 10_000)],
        "file_aggregates": [{"files": 100_000}],
        "pip_index": [{"distributions": count, "lookups": count} for count in (100, 1000, 5000)],
    }
    for series, expected_dimensions in dimensions.items():
        actual = [record["dimensions"] for record in records if record["series"] == series]
        for dimension in expected_dimensions:
            if actual and actual.count(dimension) != 1:
                errors.append(f"{series}: expected exactly one record with dimensions {dimension}")
    return errors


def capture(command):
    result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, timeout=30)
    if result.returncode:
        return None
    return result.stdout.strip()


def metadata(idle_seconds):
    filesystem = {}
    filesystem_diagnostic = "owning device unavailable"
    device_report = capture(["df", "-P", tempfile.gettempdir()])
    if device_report and len(device_report.splitlines()) == 2:
        device = device_report.splitlines()[1].split()[0]
        result = subprocess.run(
            ["diskutil", "info", "-plist", device], capture_output=True, timeout=30,
        )
        if result.returncode == 0:
            info = plistlib.loads(result.stdout)
            filesystem = {
                key: info[key]
                for key in ("FilesystemType", "FilesystemName", "FileSystemPersonality", "TypeBundle", "SolidState")
                if key in info
            }
            filesystem_diagnostic = None if filesystem else "filesystem fields unavailable"
        else:
            filesystem_diagnostic = f"diskutil exited {result.returncode}"
    return {
        "schema_version": 1,
        "recorded_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "revision": capture(["git", "rev-parse", "HEAD"]),
        "working_tree_dirty": bool(capture(["git", "status", "--porcelain"])),
        "runner": os.environ.get("BENCHMARK_RUNNER", "local"),
        "architecture": platform.machine(),
        "os_version": capture(["sw_vers", "-productVersion"]),
        "os_build": capture(["sw_vers", "-buildVersion"]),
        "cpu": capture(["sysctl", "-n", "machdep.cpu.brand_string"]),
        "logical_cpus": os.cpu_count(),
        "memory_bytes": int(capture(["sysctl", "-n", "hw.memsize"]) or "0") or None,
        "rustc": capture(["rustc", "-Vv"]),
        "cargo": capture(["cargo", "-V"]),
        "sdk_version": capture(["xcrun", "--show-sdk-version"]),
        "fixture_filesystem": filesystem,
        "fixture_filesystem_diagnostic": filesystem_diagnostic,
        "protocol": {
            "build_profile": "release",
            "test_threads": 1,
            "native_terminal_columns": 100,
            "native_terminal_rows": 24,
            "idle_seconds": idle_seconds,
            "cache_state": "Temporary fixtures; warm-cache loops as defined by each benchmark; OS caches are not flushed.",
            "timing_policy": "Informational; no machine-dependent timing thresholds.",
            "syscall_scope": "Direct test-instrumented scanner calls only, not whole-process or OS totals.",
        },
    }


def run_logged(command, log_path, env):
    start = time.monotonic()
    with log_path.open("w") as log:
        result = subprocess.run(command, cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT)
    return {
        "command": command,
        "log": log_path.name,
        "exit_code": result.returncode,
        "elapsed_seconds": round(time.monotonic() - start, 3),
    }


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def write_summary(path, machine, results):
    lines = [
        "# Performance sample", "",
        f"Revision: `{machine['revision']}`. Runner: `{machine['runner']}`. "
        f"macOS {machine['os_version']} ({machine['architecture']}). "
        f"Working tree dirty: {machine['working_tree_dirty']}.", "",
        "Timing is informational. Compare samples only with compatible machine, toolchain and fixture metadata. "
        "Test assertion failures and missing measurement records fail the run.", "",
        "| Series | Dimensions | Measurements |", "| --- | --- | --- |",
    ]
    for record in results["measurements"]:
        dimensions = ", ".join(f"{key}={value}" for key, value in record["dimensions"].items()) or "—"
        metrics = ", ".join(f"{key}={value}" for key, value in record["metrics"].items())
        lines.append(f"| {record['series']} | {dimensions} | {metrics} |")
    if results["errors"]:
        lines.extend(["", "Failures:", ""] + [f"- {error}" for error in results["errors"]])
    path.write_text("\n".join(lines) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "target/benchmark-results")
    parser.add_argument("--idle-seconds", type=int, default=5, choices=range(1, 61), metavar="1..60")
    args = parser.parse_args()
    if platform.system() != "Darwin":
        parser.error("benchmark execution requires macOS")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    machine = metadata(args.idle_seconds)
    write_json(output / "metadata.json", machine)
    env = dict(os.environ, CARGO_TERM_COLOR="never")
    commands = []
    print("Running five optimized benchmarks serially; progress is retained in benchmarks.log.", flush=True)
    commands.append(run_logged(
        ["cargo", "test", "--release", "--locked", "benchmark_", "--", "--ignored", "--nocapture", "--test-threads=1"],
        output / "benchmarks.log", env,
    ))
    print("Running the native terminal lifecycle and idle sample; progress is retained in native-tui.log.", flush=True)
    env["DISKR_BENCH_IDLE_SECONDS"] = str(args.idle_seconds)
    commands.append(run_logged(
        ["cargo", "test", "--release", "--locked", "--test", "cli", NATIVE_TEST, "--", "--exact", "--nocapture", "--test-threads=1"],
        output / "native-tui.log", env,
    ))
    logs = {command["log"]: (output / command["log"]).read_text() for command in commands}
    records = parse_measurements("\n".join(logs.values()))
    errors = measurement_errors(records)
    for command in commands:
        if command["exit_code"]:
            errors.append(f"{command['log']}: command exited {command['exit_code']}")
    for benchmark in BENCHMARKS:
        if not re.search(r"test \S*" + re.escape(benchmark) + r" \.\.\. ", logs["benchmarks.log"]):
            errors.append(f"benchmark did not execute: {benchmark}")
    results = {"schema_version": 1, "status": "failed" if errors else "passed", "metadata_file": "metadata.json", "commands": commands, "measurements": records, "errors": errors}
    write_json(output / "results.json", results)
    write_summary(output / "summary.md", machine, results)
    print(f"{results['status']}: {len(records)} measurement records written to {output}", flush=True)
    for error in errors:
        print(error, file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
