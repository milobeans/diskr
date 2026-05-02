# diskr

`diskr` is a lightweight terminal file explorer and disk/storage manager for macOS.

It shows a navigable file list, recursive directory sizes, disk usage gauges, hidden-file toggling, sorting, and safe deletion through the macOS Trash.

## Install

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
| Enter | Open selected directory |
| Backspace | Go to parent directory |
| r | Rescan directory sizes |
| o | Cycle sort mode |
| . | Toggle hidden files |
| d | Move selected item to Trash |
| Tab | Switch panes |
| q, Esc | Quit |

## Notes

`diskr` is macOS-only. Directory sizing uses `getattrlistbulk(2)` for fast local scans, and deletion uses the macOS Trash rather than permanent removal.

## Release Checks

```sh
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo package
```
