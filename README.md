<div align="center">

# diskr

**See where your disk space went, and what you can safely take back.**

A fast, macOS-native disk explorer for the terminal, written in Rust.

[![crates.io](https://img.shields.io/crates/v/diskr.svg)](https://crates.io/crates/diskr)
[![CI](https://github.com/milobeans/diskr/actions/workflows/ci.yml/badge.svg)](https://github.com/milobeans/diskr/actions/workflows/ci.yml)
[![macOS 15+](https://img.shields.io/badge/macOS-15%2B-111111?logo=apple)](#compatibility)
[![License: MIT](https://img.shields.io/crates/l/diskr.svg)](LICENSE)

<img src=".github/assets/demo.webp" width="100%" alt="diskr in a terminal: directory sizes stream in for a home folder, a project opens to reveal a 4.4 GiB build directory, the cleanup view lists safe, regenerable and risky findings, and Top Files ranks the largest files under Library">

**[Website & docs](https://milobeans.github.io/diskr/)** · [Install](#install) · [Tour](#a-quick-tour) · [Keys](#keys) · [Reports](#reports-and-scripting) · [How it works](#how-it-works)

</div>

## Why diskr

Finder sucks. diskr was built as a simpler and more direct way to manage the files and storage on your mac.

- **What is using the space?** Live directory sizes, sorted by what is really
  allocated on disk. APFS clones, sparse files and compression make apparent
  size misleading, so diskr tracks both and shows the difference.
- **What can I safely delete?** Caches, build output, toolchains and package
  stores are found and labeled *safe*, *regenerable* or *risky*, each with the
  reason why.
- **What changed?** Directories that changed since their last scan get a
  growth badge, and saved baselines show exactly what grew between two dates.
- **Why doesn't free space add up?** APFS container space, local snapshots and
  user-available versus free blocks, side by side.
- **What are my dev tools costing me?** Homebrew, Cargo, npm, pip and Bun
  packages, plus every project's `node_modules`, `target` and `.venv`, with
  per-item sizes.

It also stays out of your way:

- **Fast and lazy.** Sizing uses `getattrlistbulk(2)` to read many entries per
  syscall, and only scans around what you are looking at. Sizes are cached
  between runs and revalidated with FSEvents, so unchanged trees stay fresh
  without a rescan.
- **Careful by default.** `--reclaim` only reports; it never deletes. In the
  TUI, destructive keys are red and always ask for confirmation, and `d` moves
  items to the Trash. Unreadable or partial measurements are flagged as lower bounds, never
  silently counted as zero.
- **Native.** Reveal in Finder, open with the default app, copy a path or open
  Terminal at any item.
- **Scriptable.** Every report has a `--json` mode with a versioned schema, and
  `--strict` exits with status 2 when a report is incomplete.

## A quick tour

<sub>Screenshots and the demo above come from the real diskr binary running
against a demo home directory.</sub>

### Everything at a glance

<img src=".github/assets/files.png" width="100%" alt="The diskr Files pane listing a home directory by allocated size with share bars; .ollama shows an up arrow with 2.7 GiB of growth, .Trash and .npm are tagged as cache, and a side panel lists project dependency footprints for Cargo, pip and npm projects">

The Files pane sorts by allocated size with a share bar for every row. Inline
tags such as `[cache]` and `[build]` mark reclaimable directories as you
browse, and `↑2.7 GiB` shows what grew since the last scan. Press `.` to reveal
hidden directories, which is where `.ollama`, `.cargo`, `.npm` and friends
quietly accumulate. The side panels show your volumes and, with `p`, installed
packages or per-project dependency footprints.

### Know what is safe to delete

<img src=".github/assets/reclaim.png" width="100%" alt="The diskr cleanup view listing 18 findings totalling 33 GiB, including Xcode DerivedData marked safe, Rust target directories marked regenerable, simulator devices marked risky, and Docker data showing 1.5 GiB allocated of 64 GiB apparent">

Tab to the Reclaim pane (or run `diskr --reclaim ~`) to find caches, build
artifacts and package stores. Every finding explains itself:

| Class | Meaning | Examples |
| --- | --- | --- |
| **safe** | Pure cache, regenerated automatically | `~/Library/Caches`, Xcode DerivedData, Homebrew, npm and pip caches, Trash |
| **regenerable** | Costs a rebuild or re-download | `node_modules`, `target`, `.venv`, Cargo registry, iOS DeviceSupport |
| **risky** | May hold data you cannot recreate | Xcode Archives, simulator devices, Docker data, `.next` output |

Build directories are only flagged when their project markers are present
(`target` next to a `Cargo.toml`, `node_modules` next to a `package.json`), so a
folder named `target` alone is never a recommendation. Sizes are allocated
bytes, which is why Docker's sparse disk image shows `1.5 GiB / 64 GiB`.

### Find the biggest files anywhere

<img src=".github/assets/top-files.png" width="100%" alt="The diskr Top Files browser ranking the largest files under Library, led by a Docker.raw image at 1.5 GiB allocated of 64 GiB apparent, followed by iOS DeviceSupport caches and simulator images">

Press `t` on any directory to rank the largest files beneath it, however deep
they are. Reveal one in Finder, open it, or move it to the Trash without
leaving the list.

### See what changed

Save a baseline now and compare whenever you like. diskr keeps the latest
baseline per root, and named baselines let you pin comparison points such as
"before cleanup":

```console
$ diskr --save ~
$ diskr --diff ~
Diff for /Users/demo
Baseline captured 3d ago
Total: 44 GiB disk → 50 GiB disk  (+5.1 GiB)
    +2.7 GiB  grew       .ollama
    +1.5 GiB  grew       Downloads
    +960 MiB  grew       code
```

In the TUI, press `B` to save a baseline for the current directory; rows that
changed since their last scan show a `↑` or `↓` badge, and `o` can sort by growth.

### Explain your free space

`diskr --space` shows why `df`, Finder and your own math disagree: mount-level
totals, APFS container free space, what is actually available to you, and the
local snapshots that can pin deleted data.

```console
$ diskr --space ~
Space report for /Users/demo
Mount: /System/Volumes/Data (apfs, /dev/disk3s5)
Total: 460 GiB
Used: 446 GiB
Free blocks: 14 GiB
Available to user: 14 GiB
APFS container disk3: 460 GiB total / 14 GiB free
Local snapshots: 3
  com.apple.os.update-16E05F2FFE91F1A5…
  …
```

`diskr --thin-snapshots 10G ~` previews snapshot thinning as a dry run; add
`--yes` to request it.

### Packages and project dependencies

Press `p` for the packages pane. The System view lists Homebrew formulae and
casks, npm, pip, Cargo and Bun installs with per-package sizes; the Projects
view finds dependency directories under the current path and how much each
costs. `u` filters to dependency leaves, and `x` (or `d`) runs the owning
manager's uninstall flow after confirmation.

<details>
<summary>How package sizes and dependency counts are measured</summary>

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

</details>

## Install

With Rust 1.88 or newer:

```sh
cargo install diskr
```

Or grab a prebuilt binary for Apple Silicon (`aarch64-apple-darwin`) or Intel
(`x86_64-apple-darwin`) from [GitHub Releases](https://github.com/milobeans/diskr/releases).
Archives target macOS 15 or newer and include dependency license notices and
an inventory alongside the executable.

<details>
<summary>Verified install from GitHub Releases</summary>

In a new, empty working directory:

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
or recommended by these instructions.

</details>

To build from a local checkout, run `cargo install --path .`. A maintained
Homebrew formula is not currently provided.

## Usage

```sh
diskr               # browse your home directory
diskr ~/Downloads   # start somewhere specific
```

Your first minute:

- `S` sizes every visible directory; `.` shows hidden ones
- `Enter` opens a directory, `Backspace` goes up, `/` searches
- `Tab` cycles Files, Disks, Packages and Reclaim
- `t` ranks the largest files under the selection; `i` shows details
- `?` lists every key

## Keys

<details>
<summary>Full key reference</summary>

This table mirrors the in-app `?` overlay and `diskr --help`; an exact-key test
fails the build if the documented and runtime bindings drift. Destructive
keys (`d`, `E`, `x`) are highlighted in red in the TUI.

| Key | Action |
| --- | --- |
| ? | Show full keyboard help |
| q | Quit |
| Esc, Ctrl+C | Cancel a focused listing/inventory/reclaim scan, focus Files, close a modal or clear a filter |
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

</details>

## Reports and scripting

Everything the TUI knows is available as a report. All report modes support
`--json`.

```sh
diskr --top 20 ~/Downloads         # largest files by allocated size
diskr --reclaim ~                  # reclaimable caches and build artifacts
diskr --save ~/Downloads           # save a scan baseline
diskr --diff ~/Downloads           # what changed since the baseline
diskr --space ~                    # APFS free-space breakdown and snapshots
diskr --packages ~                 # system packages and project dependencies
diskr --packages --projects-only --max-depth 3 --exclude archive ~/code
diskr --packages --manager cargo --manager brew --no-projects ~
diskr --thin-snapshots 10G ~       # preview snapshot thinning (dry run)
diskr --thin-snapshots 10G --yes ~ # execute it
```

<details>
<summary>JSON output, exit codes and filters</summary>

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

Space discovery uses the owning volume mount point. APFS failures remain visible
in text and details; JSON retains the error text and an `apfs_diagnostic` object
whose `kind` is `timeout`, `tool_failure` or `malformed_output`.

Follow-up commands printed by reports quote paths for macOS `zsh`; paths
containing control or non-UTF-8 bytes use `$'\\xNN'` byte escapes.

</details>

<details>
<summary>Package scope controls</summary>

Package scope controls apply to `--packages` or `--save-profile`. `--projects-only` never
invokes package managers; `--manager` is repeatable and selects `brew`,
`brew-cask`, `npm`, `pip`, `cargo` or `bun`, with projects still included unless
`--no-projects` is set. `--max-depth` accepts 0 (the selected directory only)
through 64, default 5. Repeat `--exclude` for exact root-relative project
subtrees, not shell globs; absolute paths and `..` are rejected. Exclusions
affect discovery, not the footprint of an included project's dependency tree.
The selected scope is included in JSON and human output.

</details>

<details>
<summary>Named baselines and saved inventory profiles</summary>

The default `--save`/`--diff` pair keeps one latest baseline per root, retaining
up to 512 roots. Named captures are separate, immutable and never silently
replace that latest baseline. Names use 1-80 ASCII letters, digits, `.`, `_` or
`-`. Capture a new name for each comparison point:

```sh
diskr --save --baseline before-cleanup ~/code
diskr --diff --baseline before-cleanup --json
diskr --baselines list --json
diskr --baselines pin before-cleanup
diskr --baselines unpin before-cleanup
diskr --baselines prune 10         # preview: keep newest 10 unpinned, plus all pinned
diskr --baselines prune 10 --yes   # apply; prints a recoverable library backup path
diskr --baselines export /tmp/diskr-baselines.json
diskr --baselines import /tmp/diskr-baselines.json
diskr --save-profile work --projects-only --max-depth 3 --exclude archive ~/code
diskr --packages --profile work --json
diskr --baselines remove-profile work        # preview; add --yes to back up and remove
```

Without a path, a named diff uses its stored root. An explicit different root
is rejected, not retargeted. Baselines are stored under
`~/Library/Application Support/diskr/`, and diffs do not overwrite them. The library lives alongside history as
`baseline-library.json`, with a 16MiB bound, at most 2,048 named captures and
256 profiles. Retention/capacity and source identities are visible in the list.
Pruning requires `--yes`, retains all pinned records and writes a backup before
removal. Exports refuse to overwrite an existing file; imports validate before
atomically merging, reject conflicting names, preserve machine/raw-root
identity, and never replace latest-root history. Re-importing identical records
is harmless. A named cross-machine comparison is explicitly warned and partial
for `--strict`; imported foreign profiles cannot run until recreated locally
with an explicit root/scope and a new name.

Profiles save a package inventory root and scope without invoking managers.
`--packages --profile NAME` accepts report filters/JSON/strict options, but not
another path or scope overrides. Saving an existing name is an error.
Obsolete profiles can be removed explicitly with `--baselines remove-profile`;
like pruning, it previews by default and requires `--yes`, with a backup.

</details>

## Appearance and accessibility

| Variable | Effect |
| --- | --- |
| `DISKR_THEME` | `dark` (default), `light`, `high-contrast` or `mono` |
| `NO_COLOR` | Any non-empty value selects monochrome, keeping selection gutters and textual warning/coverage markers |
| `DISKR_REDUCED_MOTION=1` | Replaces animated scan glyphs with a static `scan` label |
| `DISKR_ASCII=1` | ASCII alternatives for common TUI decorations; also disables animation. Unicode filenames remain Unicode |

These preferences affect terminal rendering, not filesystem names, action
targets or JSON data.

## How it works

The core scanner calls `getattrlistbulk(2)` with a packed attribute list
requesting name, object type, logical size, and allocated size. The kernel
fills a buffer with dozens of entries per call, and diskr parses the packed
binary layout directly without allocating per entry on the hot path. A custom
work-stealing thread pool (no rayon) fans out across subdirectories. Scans
start in small batches around the current selection, so opening `/` does not
immediately walk every subtree. Performance depends on the filesystem, tree
shape and cache state; no fixed speedup is promised.

The size cache and history baselines are persisted as JSON through
`serde_json`. On relaunch, diskr replays macOS FSEvents since the previous
session to revalidate only the directories that actually changed.

<details>
<summary>Responsiveness, accuracy and cleanup detection</summary>

Startup renders before state loading, with q/Esc/Ctrl+C cancellation. Directory
enumeration runs off the UI loop in bounded batches; navigation and refresh
invalidate older generations. Esc/Ctrl+C in Files cancels enumeration and
labels the shown list partial; `r` retries. Ready inventory rows appear while
other managers/projects are still running. The inventory footer reports elapsed
time and ready stages; Esc/Ctrl+C cancels its read-only subprocesses/walks,
retaining ready rows with a partial marker. Quit cancels read-only discovery,
while pending mutations keep the existing graceful-shutdown policy.

Independent scans of the same root, identity, policy, freshness and requested
Top Files capacity share in-flight work. Each subscriber retains its own
cancellation; one cancelled caller cannot cancel another. Completed broker
results are not retained as a cache, and refresh/mutation invalidates matching
in-flight registrations. History's shared hardlink-accounting scopes remain
separate. Interactive queued roots can be promoted without increasing worker
count; descendants retain the pool's locality-aware traversal.

Sizes count regular-file content only -- symlink targets and directories' own
metadata blocks are not added, so totals run slightly under `du`/Finder, and
directories or entries that cannot be read are flagged as lower bounds rather
than counted as zero. Human terminal output escapes control characters from
filenames and external tools; JSON retains normal structured escaping.

The reclaimable-space detector runs two passes: a fixed-location pass that
checks well-known paths (`~/Library/Caches`, Xcode DerivedData, Homebrew,
Docker, etc.) and a bounded recursive pass that finds repeated build-artifact
directories (`node_modules`, `target`, `.venv`, `__pycache__`, `.next`,
`.gradle`). Both use the same fast bulk scanner for sizing. Esc/Ctrl+C cancels
a focused reclaim scan; quit also cancels its read-only work. Cancelled
findings are not published as a complete report.

Artifact classifications require supporting project/environment markers; a name
such as `target` alone is not a cleanup recommendation. Simulator devices and
deployment output can contain state you cannot recreate. Reports disclose their
depth, mount and hidden/cache exclusions. Candidate allocated bytes are not a
promise of freed space: APFS clones, snapshots and links can retain blocks, and
moving files to Trash does not free their storage until Trash is emptied.
Baseline and cleanup totals deduplicate hardlinks within their selected scope;
independently measured Files rows may share an inode and must not be summed as
an exact unique-root total. Use Top Files for a root-unique content scan.

APFS space reporting combines `statfs(2)` for mount-level stats, a narrow
parser for `diskutil info -plist` output to get container-level free space, and
`tmutil listlocalsnapshots` for snapshot discovery.

diskr uses `ratatui`, `crossterm`, `libc`, `anyhow`, `serde_json`, `toml`, and
the Unicode segmentation/width crates.

</details>

## Compatibility

diskr is macOS-only, on Apple Silicon and Intel; the minimum supported Rust
version is 1.88.0. CI's maintained runtime floor is macOS 15, with native tests
on macOS 15 and 26 for both architectures. Older macOS source builds are
unverified, not deliberately blocked. The macOS-only target guard reports a
clear error on Linux, Windows and unsupported CPU targets.

<details>
<summary>Filesystems and terminals</summary>

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

</details>

## Development

Planned work lives in [ROADMAP.md](ROADMAP.md), and release history lives in
[CHANGELOG.md](CHANGELOG.md).

<details>
<summary>Checks, benchmarks and releases</summary>

### Release checks

```sh
cargo fmt -- --check
cargo check --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
cargo package --locked
cargo audit
```

### Benchmarks

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
The viewport check uses isolated fixtures, 100 and 10,000 packages, 120×40
frames, and 100,000 synthetic file rows. It reports frame p50/p95, thread-local
allocation events (alloc/realloc), and cold/cached aggregate time. These are
rendering measurements, not filesystem throughput claims. Neither benchmark
enforces machine-dependent timing thresholds.

For the full fixture protocol, run benchmarks serially so scanner counters and
timings do not include another benchmark's work:

```sh
cargo test --release --locked benchmark_ -- --ignored --nocapture --test-threads=1
DISKR_BENCH_IDLE_SECONDS=5 cargo test --release --locked --test cli native_tui_restores -- --nocapture
```

Fixtures cover a flat 10,000-file listing, 10,000 files across 100 directories,
a 64-level chain, denied directories, hardlinks, a 1GiB sparse file, and an APFS
clone where supported. Normal tests also exercise an injected foreign device
and real read-only APFS helper-mount rejection before enumeration; no mount or
privileged tracing setup is performed. Listing checks report first-row and total
latency, p95 cursor/batch-drain time and harness RSS. Boundary scans report
direct scanner `openat`/`fstat`/`getattrlistbulk`/`close` counts from test-only
instrumentation, not system-wide or std-library syscall totals. The native
100×24 PTY check loads 10,002 files and measures first paint/row, idle CPU time
with Mach timebase conversion, and child-process RSS. Idle sampling is opt-in
(1-60 seconds), not a CI speed threshold. It also exercises retained filtering,
refresh, confirmation cancellation, help, quit and terminal restoration using
an isolated HOME; destructive confirmation is never accepted.

Record CPU, macOS/filesystem, terminal dimensions, build profile, warm/cold cache
state, sample count and tool versions when comparing results. These fixtures
are local warm-cache diagnostics, not a claim about every volume or workload.

### README images and website

The screenshots, demo animation and social preview in `.github/assets/`, and
the replay data behind the [website](https://milobeans.github.io/diskr/)
(`site/assets/replay.json`), are generated from the release binary, not drawn
by hand. After a UI change, regenerate them with:

```sh
python3 scripts/screenshots/generate.py
```

It needs macOS on APFS, `img2webp` (`brew install webp`) and about 4 GiB of
free space. It builds a synthetic home directory from APFS clones, drives diskr
in a pseudo-terminal, and installs its pinned Python packages into
`target/screenshots/venv`. Host paths are masked as `/Users/demo`; the disks
panel still shows the machine's real volumes.

The website in `site/` is static HTML, CSS and JavaScript with no build step;
preview it with `python3 -m http.server -d site` after copying
`.github/assets/*` into `site/assets/img/`. The Pages workflow publishes it on
every push to `main` that touches the site or its images. A test keeps the
site's key reference in sync with the keymap.

### Releases

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

</details>

## License

[MIT](LICENSE)
