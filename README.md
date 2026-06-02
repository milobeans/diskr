# diskr

`diskr` is a lightweight terminal file explorer and disk/storage manager for macOS.

It shows a navigable file list, recursive directory sizes, disk usage gauges, hidden-file toggling, sorting, and safe deletion through the macOS Trash. The disk pane is selectable, so you can jump directly to mounted volumes.

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

## Keys

| Key | Action |
| --- | --- |
| Up/Down, j/k | Move selection |
| Enter | Open selected directory or disk |
| Backspace | Go to parent directory |
| r | Rescan directory sizes |
| o | Cycle sort mode |
| . | Toggle hidden files |
| d | Move selected item to Trash |
| Tab | Switch files/disks pane |
| q, Esc | Quit |

## Notes

`diskr` is macOS-only. Directory sizing uses `getattrlistbulk(2)` for fast local scans, and deletion uses the macOS Trash rather than permanent removal.

The minimum supported Rust version is 1.86.0.

## Release Checks

```sh
cargo fmt -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
cargo package --locked
```
