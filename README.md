# diskr

A fast, macOS-native terminal file explorer and disk analyzer built in Rust.

## Why diskr

Most disk usage tools show you a tree of sizes and leave you to figure out the rest. diskr goes further: it tells you *what changed*, *what's reclaimable*, and *why your free space doesn't match what Finder says*.

**Fast, lazy scanning.** Directory sizing uses `getattrlistbulk(2)`, a macOS syscall that returns attributes for many entries in a single kernel crossing. Where the typical `readdir` + `stat` pattern makes one syscall per file, diskr batches them -- 3-10x faster on directories with thousands of small files like `node_modules` or `~/Library/Caches`. Scans run on a thread pool and are started in small batches around the current selection, so opening `/` or another broad directory does not immediately walk every child subtree. Press `S` when you want to fill in every missing or stale visible directory size without refreshing the directory or invalidating known fresh sizes.

**Allocated vs. apparent size.** APFS clones, sparse files, and compressed files mean logical size and on-disk size often diverge. diskr tracks both, sorts by allocated size, and shows apparent size when they differ. This is the number you actually care about when reclaiming space.

**Reclaimable space detection.** `diskr --reclaim` scans for developer caches, build artifacts, package manager stores, and other space sinks. Every finding is tagged as *safe* (pure cache, auto-regenerated), *regenerable* (costs a rebuild or re-download), or *risky* (may contain irreplaceable data). diskr reports what it finds but never deletes anything in this mode.

**Scan baselines and diffing.** Save a snapshot with `diskr --save`, then run `diskr --diff` days or weeks later to see exactly which directories grew, shrank, appeared, or disappeared. Baselines are stored under `~/Library/Application Support/diskr/` and diffs don't overwrite them.

**APFS space accounting.** `diskr --space` explains the gap between `df` free space and what's actually available to you by surfacing APFS container info, local Time Machine snapshots, and the user-available vs. free-block distinction. It can also thin local snapshots on request (`--thin-snapshots`), with a dry-run by default.

**Package and dependency inspection.** The TUI has a dedicated packages pane showing Homebrew, Cargo, pip, npm, and other package managers with per-package sizes. It also finds project dependency directories (`node_modules`, `target`, `.venv`) under the current path and shows how much space each one takes.

**macOS-native integration.** Quick Look any file with `Space`, reveal in Finder with `f`, open with the default app with `O`, and delete to Trash (never permanent removal) with `d`.

## Install

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

| Key | Action |
| --- | --- |
| ? | Show full keyboard help |
| Up/Down, j/k | Move selection |
| PageUp/PageDown | Move by a page |
| Home/End | Jump to first or last item |
| Enter | Open selected directory, disk, or package path |
| i | Show details for the selected file, package, or disk |
| p | Open packages pane or switch package view |
| Backspace | Go to parent directory |
| / | Search files in the current directory or filter packages; Enter keeps, Esc clears |
| Left/Right, h/l | Switch pane or package view |
| Space | Quick Look selected item |
| f | Reveal selected item in Finder |
| O | Open selected item with the default app |
| y | Copy selected item path to clipboard |
| s | Open selected item location in Terminal |
| r | Refresh the current view and rescan all visible directory sizes |
| S | Scan every missing or stale visible directory size without refreshing |
| o | Cycle sort mode |
| . | Toggle hidden files |
| d | Move selected item to Trash |
| E | Empty Trash from the Reclaim pane, when the report lists a Trash finding |
| Tab | Switch files/disks/packages pane |
| q | Quit |
| Esc / Ctrl+C | Focus Files pane / cancel modals and clear search/filter |

## How it works

diskr is ~6,000 lines of Rust with no runtime beyond `ratatui`, `crossterm`, and `libc`. The core scanner calls `getattrlistbulk(2)` with a packed attribute list requesting name, object type, logical size, and allocated size. The kernel fills a buffer with dozens of entries per call, and diskr parses the packed binary layout directly -- no allocation per entry, no serde. A work-stealing thread pool fans out across subdirectories so sizing is parallelized across cores.

The reclaimable-space detector runs two passes: a fixed-location pass that checks well-known paths (`~/Library/Caches`, Xcode DerivedData, Homebrew, Docker, etc.) and a bounded recursive pass that finds repeated build-artifact directories (`node_modules`, `target`, `.venv`, `__pycache__`, `.next`, `.gradle`). Both use the same fast bulk scanner for sizing.

APFS space reporting combines `statfs(2)` for mount-level stats, a narrow parser for `diskutil info -plist` output to get container-level free space, and `tmutil listlocalsnapshots` for snapshot discovery.

## Notes

diskr is macOS-only. The minimum supported Rust version is 1.88.0.

Planned work lives in [ROADMAP.md](ROADMAP.md), known issues in
[docs/ISSUES.md](docs/ISSUES.md), and release history in
[CHANGELOG.md](CHANGELOG.md). Contributors (human or agent) should read
[AGENTS.md](AGENTS.md) for the development workflow.

## Release Checks

```sh
cargo fmt -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
cargo package --locked
```

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
