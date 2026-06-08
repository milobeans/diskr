# diskr

`diskr` is a lightweight terminal file explorer and disk/storage manager for macOS.

It shows a navigable file list, recursive directory sizes, allocated-vs-apparent size details, disk usage gauges, hidden-file toggling, sorting, largest-file reports, package/dependency inspection, reclaimable-space detection, scan baselines with diffing, and safe deletion through the macOS Trash. The disk and packages panes are selectable, so you can jump directly to mounted volumes and package paths.

## Install

From crates.io:

```sh
cargo install diskr
```

From a local checkout:

```sh
cargo install --path .
```

Then run:

```sh
diskr
```

To start in a specific directory:

```sh
diskr ~/Downloads
```

If the path starts with `-`, use the conventional argument separator:

```sh
diskr -- -scratch
```

To print the largest files under a directory:

```sh
diskr --top 20 ~/Downloads
```

To find reclaimable space (developer/system caches and repeated build artifacts), each tagged with how safe it is to delete:

```sh
diskr --reclaim ~
```

To record a scan baseline and later see what changed:

```sh
diskr --save ~/Downloads
# ...time passes...
diskr --diff ~/Downloads
```

To explain APFS/free-space mismatches and local snapshots:

```sh
diskr --space ~
```

For scripts and reports:

```sh
diskr --top 20 --json ~/Downloads
diskr --reclaim --json ~
diskr --diff --json ~/Downloads
diskr --space --json ~
diskr --packages --json ~
```

To preview a Time Machine local snapshot thinning request:

```sh
diskr --thin-snapshots 10G ~
```

To execute it, add the explicit confirmation flag:

```sh
diskr --thin-snapshots 10G --yes ~
```

## Keys

| Key | Action |
| --- | --- |
| Up/Down, j/k | Move selection |
| Enter | Open selected directory, disk, or package path |
| p | Open packages pane or switch package view |
| Backspace | Go to parent directory |
| Space | Quick Look selected item |
| f | Reveal selected item in Finder |
| O | Open selected item with the default app |
| r | Refresh the current view and rescan directory sizes |
| o | Cycle sort mode |
| . | Toggle hidden files |
| d | Move selected item to Trash |
| Tab | Switch files/disks/packages pane |
| q, Esc | Quit |

## Notes

`diskr` is macOS-only. Directory sizing uses `getattrlistbulk(2)` for fast local scans. The TUI sorts by allocated on-disk size and shows apparent size in the status line when it differs. The reclaimable-space report classifies each item as safe, regenerable, or risky to delete, and never deletes anything itself. Scan baselines are stored under `~/Library/Application Support/diskr/` and `--diff` reports per-child growth without changing the baseline. The APFS space report uses `statfs(2)`, `diskutil info -plist`, and `tmutil listlocalsnapshots` to surface free-space gaps and local snapshots. Deletion uses the macOS Trash rather than permanent removal.

Planned differentiators and cleanup ideas live in [ROADMAP.md](ROADMAP.md).

The minimum supported Rust version is 1.88.0.

## Release Checks

```sh
cargo fmt -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
cargo package --locked
```
