# Changelog

All notable changes to diskr are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `diskr --packages --json` now includes the canonical project root as `path`,
  matching the other JSON reports.

### Changed

- The footer help strip is now context-sensitive: it lists the keys for the
  focused pane, truncates to fit narrow terminals, and always keeps `?` visible
  instead of rendering one long static line. Destructive keys (`d`, `E`, `x`)
  are highlighted in red in both the footer and the `?` overlay, and the README
  key table now documents every binding and is checked against the keymap by a
  test so the two cannot drift.

### Fixed

- `--thin-snapshots` now validates its path the same way `--space` does (it
  must exist and be a directory, and is canonicalized) and resolves the owning
  volume before invoking `tmutil`, so it no longer accepts regular files,
  relative paths, or symlinks, and the preview shows the resolved mount.
- `size-cache.json` and `history.json` are now written atomically (a temp file
  is fsynced and renamed over the target), so a crash or full disk during a
  save leaves the previous file intact instead of a truncated one. A corrupt or
  unreadable `history.json` now surfaces a startup status warning the way the
  size cache already does, instead of silently presenting "no baselines".
- `--save` and `--diff` now report unreadable directories: saved baselines
  record an unreadable-directory count, and both the text and JSON output warn
  when permission errors limited the scan, matching the TUI, `--top`, and
  `--reclaim`. Baselines saved by older versions load unchanged.
- pip package sizing now derives the package list and the site-packages
  directories from the same Python interpreter and also searches the per-user
  site-packages, so `pip install --user` packages are sized and Homebrew, CLT,
  and pyenv interpreter splits no longer mismatch. dist-info matching now
  requires a version digit after the package name, so `sentry` is no longer
  mis-sized from `sentry-sdk`'s metadata.
- Directory scans now flag more unreadable cases instead of silently
  undercounting: a subtree whose path grows past the platform length limit
  during the walk, and individual entries whose attributes the kernel cannot
  read, are counted toward the unreadable total so reported sizes stay honest
  lower bounds.
- Mount-boundary skipping now covers volumes outside `/Volumes`. Scanning `/`
  no longer folds the APFS Preboot/VM/Update helper volumes into the system
  total, and network or FUSE mounts at custom paths are skipped rather than
  walked as local data; the firmlinked Data volume is still counted. Skipped
  mounts are now described as "mounted volumes on other devices".

## [0.1.64] - 2026-06-15

### Fixed

- TUI history baseline saves now persist only after the background scan result
  is validated as current, so a stale save worker can no longer overwrite a
  newer saved baseline on disk after the UI has discarded its result.

## [0.1.63] - 2026-06-13

### Changed

- Relaxed the `ratatui-core` and `ratatui-widgets` dependency pins from exact
  patch versions to caret ranges, so diskr can pick up compatible upstream
  fixes without a manifest edit while keeping the existing custom crossterm
  backend path.

## [0.1.62] - 2026-06-13

### Changed

- Saved history baselines now retain only the newest 512 directories on disk
  and in memory, so `history.json` and the TUI baseline cache cannot grow
  without bound as more paths are saved.

## [0.1.61] - 2026-06-13

### Fixed

- Reclaim findings now reserve separate list and detail regions, so the
  selected finding details no longer cover the middle of the findings list.

### Changed

- Moved private maintenance materials out of the public repository, including
  issue tracking, audit notes, and agent workflow instructions. CI and release
  jobs now reject those files if they are accidentally tracked again.

## [0.1.60] - 2026-06-13

### Fixed

- History baselines now stay parsed in memory for the TUI session, and
  baseline diffs index the prior child list once instead of linearly
  rescanning it per entry, which removes repeated `history.json` parses on
  navigation and avoids quadratic diff time on large directories.

## [0.1.59] - 2026-06-13

### Fixed

- Package-manager command timeouts now start each manager probe in its own
  process group and kill the whole group on deadline, so a helper process that
  inherits stdout/stderr can no longer keep `--packages` or the TUI package
  scan hung past the timeout. Regression coverage locks the shell-background
  case that previously blocked forever.

## [0.1.58] - 2026-06-13

### Fixed

- Package-manager scans now time out after 10 seconds (30 seconds for slow
  `brew cask` metadata) instead of blocking forever, and failed or timed-out
  managers surface a diagnostic warning in the TUI status line, CLI text output,
  and JSON reports instead of silently showing "0 packages."

- Resolved the audit paper-cut grab bag covering Reclaim refresh routing,
  package-pane focus behavior, diff labels, file-row alignment/truncation,
  disk labels, Full Disk Access rechecks, Empty Trash duplicate-request
  status, input cursor visibility, background batch delete, stale dead-code
  allowances, `HOMEBREW_PREFIX`, and PEP 621 dependency arrays.

## [0.1.57] - 2026-06-13

### Fixed

- Cache invalidation now drops cached sizes, stale markers, inaccessible
  counts, and cache ages for the changed path and every cached descendant, so
  deleting and recreating a directory cannot resurrect stale child sizes from
  the old tree.

## [0.1.56] - 2026-06-12

### Fixed

- Project dependency reports now merge multiple manifests that point at the
  same dependency directory, so Python projects with both `requirements.txt`
  and `pyproject.toml` count one `.venv` once while preserving both manifest
  labels.
- Size bars and the `%` column in the files pane now show each entry's share
  of the full directory rather than the scroll window, so percentages no longer
  change as you scroll and the largest item in view is no longer always
  full-width.

## [0.1.54] - 2026-06-12

### Fixed

- Package filter mode now treats plain `j` and `k` as query text, so package
  names like `jq` and `kubectl` can be typed while arrow keys still navigate.

## [0.1.55] - 2026-06-12

### Fixed

- PageUp/PageDown now jump by the active pane's visible rows in Files,
  Disks, Packages, and Reclaim, and page moves clamp at the first/last row
  instead of wrapping unexpectedly through the list.

## [0.1.53] - 2026-06-12

### Fixed

- Search and package-filter Enter now keeps the narrowed view active while
  leaving input mode; Esc and Ctrl+C clear the kept filter.
- `S` and the selected-directory scan path now rescan stale cached directory
  sizes instead of only directories with no cached size, so one suspicious
  row can be verified without invalidating the whole view.

## [0.1.52] - 2026-06-12

### Fixed

- `E` in the Reclaim pane only arms the Empty Trash confirmation when the
  loaded reclaim report actually lists a Trash finding, and the modal now
  shows that finding's path alongside its size. Without a Trash finding the
  status reports "Trash is not in this reclaim report" instead of arming a
  global Finder Empty Trash detached from the visible report.

## [0.1.51] - 2026-06-12

### Added

- Added a `?` keyboard help overlay backed by the shared TUI keymap, and
  shortened the footer to advertise `? help` instead of trying to fit every
  shortcut on one line.

## [0.1.50] - 2026-06-12

### Changed

- Replaced rayon with a purpose-built std-only work-stealing pool
  (`src/pool.rs`): per-worker deques with steal-oldest balancing, batched
  task spawns, idle-gated wakeups, and sync waiters that help drain the
  queue. Scan results and wall-clock throughput are unchanged (verified
  against the rayon build on wide, deep, and /Applications-shaped trees);
  rayon, rayon-core, crossbeam-deque, crossbeam-epoch, and crossbeam-utils
  leave the dependency graph (81 -> 76 locked crates).
- The walker now descends directory chains on one worker, accumulating
  results locally and merging into shared scan state once per chain instead
  of locking a global aggregate once per directory; chain descent opens
  children with `openat(2)` relative to the held parent fd, resolving one
  path component instead of re-walking the full path.
- Release builds use fat LTO instead of thin; with the dependency removal
  the binary shrinks ~8% (1,449,840 -> 1,333,392 bytes on Apple Silicon).

## [0.1.49] - 2026-06-12

### Fixed

- Rename keeps the cursor on the renamed entry after reloads in sorted views,
  instead of falling back to the old row index; restored the missing
  regression test.

## [0.1.48] - 2026-06-12

### Changed

- Overhauled development docs: added this changelog, structured private
  maintenance tracking, and a repeatable agent workflow protocol.

### Fixed

- Character keys carrying Control/Alt/Super no longer match plain-key
  actions: Ctrl+C cannot trigger rename, Ctrl+D cannot arm Trash deletion,
  and modified characters are not inserted into text inputs. Ctrl+C now
  cancels the active input mode, confirmation, or overlay like Esc; README
  and `--help` key lists updated.
- A panic now restores the terminal (raw mode off, alternate screen left)
  before the panic message prints, including release builds where
  `panic = "abort"` skips destructors.
- Reclaim reports no longer double-count nested fixed cache categories:
  parent categories containing other findings are marked as `[subtotal]`
  roll-up rows and excluded from the report total; JSON findings gain a
  `rollup` field.
- The package detail modal in the Projects view shows the selected project
  dependency row instead of an unrelated system package.
- `empty_trash` runs `osascript` through an injectable runner; the test suite
  can no longer touch the real Trash, and its doc comment no longer claims
  emptying is reversible.

## [0.1.47] - 2026-06-12

### Added

- File info popup: `i` in the Files pane opens a modal with full path, type,
  logical vs. allocated size (with APFS clone/sparse/compression note),
  created/modified/accessed timestamps, owner/group, permissions, hard-link
  count, and xattr count with quarantine flagging. Action keys for Quick Look,
  Finder reveal, Open, and Trash.

## [0.1.46] - 2026-06-12

### Fixed

- Homebrew cask rows now act on the real `.app` bundle: detail, Finder reveal,
  and Open follow the app bundle while uninstall keeps using the cask token.
  The Caskroom metadata path is shown separately when it differs.

## [0.1.45] - 2026-06-12

### Fixed

- Stabilized the package visibility cache regression test.

## [0.1.44] - 2026-06-12

### Fixed

- Package pane no longer rebuilds visibility indices on every rendered row
  (O(n^2) per frame); visible indices and lowercase search text are cached.
- npm global package sizing resolves from the active `npm root -g` first, so
  package lists and size lookups stay aligned under nvm/fnm.
- Top-files and reclaim-paths modals page by visible height and render their
  footers without clipping.

## [0.1.43] - 2026-06-12

### Fixed

- Size-sorted scan results no longer land on the wrong row: `apply_sort()`
  rebuilds the path-to-index map, so a directory size arriving after a
  mid-scan resort cannot be written onto an unrelated file.

## 0.1.34 - 0.1.42 (2026-06-11 - 2026-06-12)

Rapid audit-driven fix releases. Reconstructed summary; detailed provenance
lives in git history.

### Added

- Full-subtree scan mode: `S` scans every visible directory whose size is
  missing without invalidating known sizes (0.1.38).
- Clipboard copy (`y`) and open-in-Terminal (`s`) shortcuts (0.1.39).

### Fixed

- `Esc` no longer quits from the Files pane; `q` remains quit (0.1.36).
- Replaced the redundant outer scanner thread layer with a single rayon
  dispatch thread (0.1.37).
- Stale background package/reclaim results from a previous directory are
  discarded instead of being shown for the current one.
- Multi-select marks render a checkmark, clear on directory changes, and batch
  delete confirmations list the marked items.
- Rename follows the renamed entry after reload. This later regressed and was
  restored in 0.1.49.
- Empty Trash requires an explicit confirmation and runs on a background
  worker.
- Dependency-leaf graph covers casks, npm, cargo, and bun, not just brew
  formulae and pip.
- Baseline header chip formatting (duplicate "ago", missing separator).
- Reverse pane navigation from Files lands on Reclaim.

## 0.1.21 - 0.1.33 (2026-06-10 - 2026-06-11)

First audit wave. Highlights:

- Search-mode selection corruption fixes, spinner animation during quiet
  scans, permission failures surfaced as lower-bound sizes instead of
  silent 0 B, scan-result salvage and cancellation, `r` rescans all
  visible directories, hard-link and firmlink double-counting fixes,
  removed unused mouse capture, accurate brew cask and pip
  sizing, mtime display, project-deps rescan caching,
  TUI surfaces for reclaim/top-files/disk-details/history-diff,
  file operations: rename, mkdir, multi-select, batch trash, per-row
  size-share bars, persistent size cache.

## 0.1.0 - 0.1.20 (2026-05-01 - 2026-06-10)

Initial development: TUI file browser with `getattrlistbulk(2)` bulk scanner,
disks and packages panes, CLI reports (`--top`, `--reclaim`, `--save`/`--diff`,
`--space`, `--packages`, `--thin-snapshots`), automated release workflow.
