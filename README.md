# diskr

A fast, macOS-native terminal file explorer and disk analyzer built in Rust.

## Why diskr

Most disk usage tools show you a tree of sizes and leave you to figure out the rest. diskr goes further: it tells you *what changed*, *what's reclaimable*, and *why your free space doesn't match what Finder says*.

**Fast, lazy scanning.** Directory sizing uses `getattrlistbulk(2)`, a macOS syscall that returns attributes for many entries in a single kernel crossing. Where the typical `readdir` + `stat` pattern makes one syscall per file, diskr batches them. Performance depends on the filesystem, tree shape and cache state; no fixed speedup is promised. Scans run on a thread pool and are started in small batches around the current selection, so opening `/` or another broad directory does not immediately walk every child subtree. Press `S` when you want to fill in every missing or stale visible directory size without refreshing the directory or invalidating known fresh sizes. Cached sizes persist between runs; on relaunch diskr replays macOS FSEvents since the previous session to revalidate only the directories that actually changed, so unchanged trees stay fresh without a rescan.

**Allocated vs. apparent size.** APFS clones, sparse files, and compressed files mean logical size and on-disk size often diverge. diskr tracks both, sorts by allocated size, and shows apparent size when they differ. This is the number you actually care about when reclaiming space.

**Reclaimable space detection.** `diskr --reclaim` scans for developer caches, build artifacts, package manager stores, and other space sinks. Every finding is tagged as *safe* (pure cache, auto-regenerated), *regenerable* (costs a rebuild or re-download), or *risky* (may contain irreplaceable data). diskr reports what it finds but never deletes anything in this mode.

**Scan baselines and diffing.** Save a snapshot with `diskr --save`, then run `diskr --diff` days or weeks later to see exactly which directories grew, shrank, appeared, or disappeared. Baselines are stored under `~/Library/Application Support/diskr/` and diffs don't overwrite them.

**APFS space accounting.** `diskr --space` explains the gap between `df` free space and what's actually available to you by surfacing APFS container info, local Time Machine snapshots, and the user-available vs. free-block distinction. It can also thin local snapshots on request (`--thin-snapshots`), with a dry-run by default.

Space discovery uses the owning volume mount point. APFS failures remain visible
in text and details; JSON retains the error text and an `apfs_diagnostic` object
whose `kind` is `timeout`, `tool_failure` or `malformed_output`.

**Package and dependency inspection.** The TUI has a dedicated packages pane showing Homebrew, Cargo, pip, npm, and other package managers with per-package sizes. It also finds project dependency directories (`node_modules`, `target`, `.venv`) under the current path and shows how much space each one takes.

Package reports distinguish empty inventories from unavailable managers, command
failures, malformed output, and partial results. Package details retain inventory
and dependency-graph warnings. JSON includes every manager's `scan_state` and
each package's dependency evidence; incomplete graphs have unknown leaf status.
Sizes describe known allocation within their stated footprint: directory trees,
app and manager metadata, installed binaries, RECORD-listed files, or installer
metadata. Details and JSON retain unreadable, skipped-mount and missing-file
counts. A `≥` marker denotes a partial measurement; manager totals also report
unknown and limited-scope footprints.
Manager details and JSON identify the executable and package roots used by the
scan. Uninstall uses that retained installation scope. Cargo follows
`CARGO_INSTALL_ROOT`, its user configuration and `CARGO_HOME`; Homebrew, Bun,
npm and pip supply their active locations through their inventory commands.

Project dependency counts are unique declared names across runtime, development,
optional, peer and build groups, not a count of installed transitive packages.
Cargo target tables and Python PEP/Poetry groups are included; Python names are
normalized and merged across requirements and pyproject declarations. Local
requirements includes are read with size and recursion limits; constraint files
do not add direct dependencies. Go `indirect` requirements are excluded.
Manifests are never executed. Dynamic declarations, executable Gemfiles and
read/parse failures show unknown or lower-bound counts with warnings. JSON uses
`dep_count: null` for an incomplete count and retains `known_dep_count`,
`dependency_count_complete` and `dependency_warnings`.

**macOS-native integration.** Reveal any file in Finder with `f`, open with the default app with `O`, and move files or project dependency directories to Trash with `d`. On an installed package, `d` runs that manager's uninstall flow after confirmation.

## Install

Verified binary archives are available from [GitHub Releases](https://github.com/milobeans/diskr/releases)
for Apple Silicon (`aarch64-apple-darwin`) and Intel (`x86_64-apple-darwin`).
They target macOS 15 or newer and include dependency license notices and an
inventory alongside the executable. In a new, empty working directory:

```sh
case "$(uname -m)" in
  arm64) target=aarch64-apple-darwin ;;
  x86_64) target=x86_64-apple-darwin ;;
  *) echo "Unsupported architecture" >&2; exit 1 ;;
esac
archive="diskr-$target.tar.gz"
curl --fail --location --remote-name "https://github.com/milobeans/diskr/releases/latest/download/$archive"
curl --fail --location --remote-name "https://github.com/milobeans/diskr/releases/latest/download/SHA256SUMS"
checksum=$(awk -v archive="$archive" '$2 == archive { print }' SHA256SUMS)
test -n "$checksum" && printf '%s\n' "$checksum" | shasum -a 256 -c -
```

Only after checksum verification succeeds, extract and install:

```sh
tar -xzf "$archive"
mkdir -p "$HOME/.local/bin"
install -m 755 diskr "$HOME/.local/bin/diskr"
"$HOME/.local/bin/diskr" --help
```

Add `$HOME/.local/bin` to your shell's `PATH` if needed. Archives are not
Developer ID signed or notarized; macOS or organization policy may require a
different installation route. No quarantine/security-policy bypass is required
or recommended by these instructions. A maintained Homebrew formula is not
currently provided.

To build from source with Rust 1.88 or newer:

```sh
cargo install diskr
```

Or from a local checkout:

```sh
cargo install --path .
```

## Usage

Launch the interactive TUI:

```sh
diskr              # starts in ~
diskr ~/Downloads   # starts in a specific directory
```

### CLI reports

All report modes support `--json` for scripting and automation.

JSON reports use `schema_version: 1` and include a common `report`, `root`,
`root_identity`, `scope`, `coverage`, `warnings`, `timing` and
`numeric_semantics` contract while retaining earlier report-specific fields.
`coverage.complete` is false for unreadable/skipped data, utility failures,
unknown dependency counts, or incomplete/limited package footprints. Explicit
discovery depth and policy exclusions remain visible in scope metadata.
Byte counts are unsigned 64-bit JSON integers; use an integer-capable parser
when values may exceed JavaScript's safe-number range. Signed byte deltas are
decimal strings, timestamps are Unix seconds, and elapsed time is monotonic
milliseconds. `root_identity` is a reversible Unix path encoding; `root` is
the human-readable path.

Default report exit status remains 0 for usable partial reports and 1 for
errors. `--strict` prints the report, then exits 2 if incomplete, with its
diagnostic on stderr. It does not undo a requested `--save`. JSON stdout stays
parseable and contains no progress or error prose; closed output pipes still
finish normally.

`--min-size SIZE` filters allocated-size rows in `--top`, `--reclaim` and
`--packages`; `--class safe|regenerable|risky` filters reclaim categories.
Unknown, partial or limited-scope sizes are retained. Filters affect rows, not
scan totals or completeness; JSON includes filter settings and counts of removed rows.

Follow-up commands printed by reports quote paths for macOS `zsh`; paths
containing control or non-UTF-8 bytes use `$'\\xNN'` byte escapes.

```sh
diskr --top 20 ~/Downloads        # largest files by allocated size
diskr --reclaim ~                  # reclaimable caches and build artifacts
diskr --save ~/Downloads           # save a scan baseline
diskr --diff ~/Downloads           # what changed since the baseline
diskr --space ~                    # APFS free-space breakdown and snapshots
diskr --packages ~                 # system packages and project dependencies
diskr --thin-snapshots 10G ~       # preview snapshot thinning (dry run)
diskr --thin-snapshots 10G --yes ~ # execute it
```

## Keys

This table mirrors the in-app `?` overlay and `diskr --help`; an exact-key test
fails the build if the documented and runtime bindings drift. Destructive
keys (`d`, `E`, `x`) are highlighted in red in the TUI.

| Key | Action |
| --- | --- |
| ? | Show full keyboard help |
| q | Quit |
| Esc, Ctrl+C | Cancel a focused reclaim scan, focus Files, close a modal or clear a filter |
| Up/Down, j/k | Move selection, or scroll keyboard help and detail drawers |
| PageUp/PageDown | Move by a page, including in keyboard help and detail drawers |
| Home/End | Jump to first or last item in lists, help, and detail drawers |
| Enter | Open the selected directory, disk, package, or reclaim finding |
| Backspace | Go to parent directory |
| Left/Right, h/l | Switch pane or package view |
| Tab, Shift+Tab | Cycle panes |
| b | Toggle full-width active view and side panels |
| / | Search files in the current directory or filter packages; Enter keeps, Esc clears |
| Left/Right, Home/End (input) | Move the caret in rename, mkdir and search; Backspace/Delete erase a grapheme |
| i | Show details for the selected file, package, or disk |
| . | Toggle hidden files in the Files pane |
| o | Cycle sort mode in the Files pane |
| r | Refresh the current view and rescan all visible directory sizes |
| S | Scan every missing, stale, or incomplete visible directory size in the Files pane |
| B | Save a history baseline for the current Files directory |
| t | Toggle the full-width Top Files browser for the selection; Esc also returns |
| c | Rename the selected file |
| n | Create a directory |
| v | Toggle a mark on the selected file |
| a | Mark all visible files |
| d | Trash file/project dependency (or marks); uninstall installed package |
| f | Reveal selected item in Finder |
| O | Open selected item with the default app |
| y | Copy selected item path to clipboard |
| s | Open selected item location in Terminal |
| p | Open packages; press again while focused to switch package view |
| u | Toggle the dependency-leaf filter in the packages pane |
| x | Uninstall the selected package |
| R | Re-scan the Reclaim pane |
| E | Empty Trash from the Reclaim pane, when the report lists a Trash finding |
| s (disk details) | Retry local snapshot discovery without rerunning APFS discovery |
| a (disk details) | Retry APFS container discovery without rerunning snapshot discovery |
| Up/Down, j/k (details) | Scroll the detail body while actions remain visible |
| PageUp/PageDown (details) | Scroll detail body by ten lines |
| Home/End (details) | Jump to the start or end of detail content |

## How it works

For terminal accessibility, set `DISKR_THEME=dark` (default), `light`,
`high-contrast` or `mono`. A non-empty `NO_COLOR` selects monochrome, preserving
selection gutters and textual warning/coverage markers. `DISKR_REDUCED_MOTION=1`
replaces animated scan glyphs with a static `scan` label. `DISKR_ASCII=1` uses
ASCII alternatives for common TUI decorations and also disables animation;
Unicode filenames remain Unicode. These preferences affect terminal rendering,
not filesystem names, action targets or JSON data.

diskr uses `ratatui`, `crossterm`, `libc`, `anyhow`, `serde_json`, `toml`, and the
Unicode segmentation/width crates. The size cache and history baselines are
persisted as JSON through `serde_json`. The core scanner calls
`getattrlistbulk(2)` with a packed attribute list requesting name, object type,
logical size, and allocated size. The kernel fills a buffer with dozens of
entries per call, and diskr parses the packed binary layout directly without
allocating per entry on the hot path. A custom work-stealing thread pool (no
rayon) fans out across subdirectories so sizing is parallelized across cores.
Sizes count regular-file content only -- symlink targets and directories' own
metadata blocks are not added, so totals run slightly under `du`/Finder, and
directories or entries that cannot be read are flagged as lower bounds rather
than counted as zero. Human terminal output escapes control characters from
filenames and external tools; JSON retains normal structured escaping.

The reclaimable-space detector runs two passes: a fixed-location pass that checks well-known paths (`~/Library/Caches`, Xcode DerivedData, Homebrew, Docker, etc.) and a bounded recursive pass that finds repeated build-artifact directories (`node_modules`, `target`, `.venv`, `__pycache__`, `.next`, `.gradle`). Both use the same fast bulk scanner for sizing. Esc/Ctrl+C cancels a focused reclaim scan; quit also cancels its read-only work. Cancelled findings are not published as a complete report.

Independent scans of the same root, identity, policy, freshness and requested
Top Files capacity share in-flight work. Each subscriber retains its own
cancellation; one cancelled caller cannot cancel another. Completed broker
results are not retained as a cache, and refresh/mutation invalidates matching
in-flight registrations. History's shared hardlink-accounting scopes remain
separate. Interactive queued roots can be promoted without increasing worker
count; descendants retain the pool's locality-aware traversal.

Repeatable optimized performance checks (temporary fixtures only):

```sh
cargo test --release --locked benchmark_shared_scan_and_heap_paths -- --ignored --nocapture
cargo test --release --locked benchmark_package_viewport_frames -- --ignored --nocapture
```

The scan check creates 10,000 small files, warms the fixture, and reports
median/p95 wall time over 12 samples for direct versus brokered scans, four
independent versus shared requests, and eager versus size-gated Top Files path
construction. Its p95 is the largest sample (nearest-rank percentile). It is a
warm-cache local comparison, not a cold-disk or universal throughput claim.
The viewport check reports frame latency and allocation counts at a fixed
terminal size. Neither benchmark enforces machine-dependent timing thresholds.

Artifact classifications require supporting project/environment markers; a name
such as `target` alone is not a cleanup recommendation. Simulator devices and
deployment output can contain state you cannot recreate. Reports disclose their
depth, mount and hidden/cache exclusions. Candidate allocated bytes are not a
promise of freed space: APFS clones, snapshots and links can retain blocks, and
moving files to Trash does not free their storage until Trash is emptied.
Baseline and cleanup totals deduplicate hardlinks within their selected scope;
independently measured Files rows may share an inode and must not be summed as
an exact unique-root total. Use Top Files for a root-unique content scan.

APFS space reporting combines `statfs(2)` for mount-level stats, a narrow parser for `diskutil info -plist` output to get container-level free space, and `tmutil listlocalsnapshots` for snapshot discovery.

## Notes

diskr is macOS-only on Apple Silicon and Intel; the minimum supported Rust
version is 1.88.0. CI's maintained runtime floor is macOS 15, with native tests
on macOS 15 and 26 for both architectures. Older macOS source builds are
unverified, not deliberately blocked. The macOS-only target guard reports a
clear error on Linux, Windows and unsupported CPU targets.

The filesystem baseline is local APFS. Hardlinks, sparse files, inaccessible
entries, filesystem identity and tool-output variations have fixture coverage;
other filesystems/network volumes are best-effort and mount exclusions remain
visible. Snapshot/APFS utilities use the owning mount and preserve failures.
Container allocation is not exclusive per-file ownership or a freed-space
guarantee.

Use a UTF-8, VT-compatible terminal; 80×24 or larger is recommended. Headless
frame tests cover 40/80/120 columns and short/tall viewports; native PTY smoke
tests exercise keyboard input and clean shutdown. ASCII decoration preferences
do not transliterate Unicode filenames. No promise of identical glyph widths
across every terminal/font or multiplexor is made.

Planned work lives in [ROADMAP.md](ROADMAP.md), and release history lives in
[CHANGELOG.md](CHANGELOG.md). Private maintenance trackers, audit notes, and
agent workflow instructions are intentionally kept out of the public
repository.

## Release Checks

```sh
cargo fmt -- --check
cargo check --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
cargo package --locked
cargo audit
```

The opt-in rendering benchmark uses isolated fixtures, 100 and 10,000 packages,
120×40 frames, and 100,000 synthetic file rows. It reports frame p50/p95,
thread-local allocation events (alloc/realloc), and cold/cached aggregate time:

```sh
cargo test --release --locked benchmark_package_viewport_frames -- --ignored --nocapture
```

These are rendering measurements, not filesystem throughput claims. Record the
macOS version, CPU, terminal dimensions and build profile when comparing runs.

## Releases

Releases are automated through GitHub Actions and appear in the GitHub Releases tab.

One-time setup:

1. Add a repository secret named `CARGO_REGISTRY_TOKEN` with a crates.io API token that has publish access for `diskr`.

Release flow:

1. Move the `[Unreleased]` section of `CHANGELOG.md` into a new version
   section dated today (leave `[Unreleased]` present and empty).
2. Update `Cargo.toml` to the new crate version.
3. Refresh `Cargo.lock` if needed and push the version bump to `main`.
4. Create and push a matching tag like `v0.1.14`.

CI and release jobs reject private maintenance files if they are accidentally
tracked again.

```sh
git tag -a v0.1.14 -m "v0.1.14"
git push origin v0.1.14
```

When that tag is pushed, the `Release` workflow will:

1. Verify the tag matches the crate version.
2. Verify the tagged commit is reachable from `main`.
3. Run the full release validation set.
4. Publish the crate to crates.io if that version is not already published.
5. Download the published crate and verify it was built from the exact tag commit.
6. Create the matching GitHub release once, using generated notes.
