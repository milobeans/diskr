# Changelog

All notable changes to diskr are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

Issue references (`#N`) point to [docs/ISSUES.md](docs/ISSUES.md) or, for
historical entries, to the matching finding in [docs/AUDIT.md](docs/AUDIT.md).

## [Unreleased]

## [0.1.48] - 2026-06-12

### Changed

- Overhauled development docs: added this changelog and an issue tracker
  (`docs/ISSUES.md`), froze `docs/AUDIT.md` as a historical archive, and added
  an agent workflow protocol in `AGENTS.md`.

### Fixed

- Character keys carrying Control/Alt/Super no longer match plain-key
  actions: Ctrl+C cannot trigger rename, Ctrl+D cannot arm Trash deletion,
  and modified characters are not inserted into text inputs. Ctrl+C now
  cancels the active input mode, confirmation, or overlay like Esc; README
  and `--help` key lists updated. (#51)
- A panic now restores the terminal (raw mode off, alternate screen left)
  before the panic message prints, including release builds where
  `panic = "abort"` skips destructors. (#54)
- Reclaim reports no longer double-count nested fixed cache categories:
  parent categories containing other findings are marked as `[subtotal]`
  roll-up rows and excluded from the report total; JSON findings gain a
  `rollup` field. (#46)
- The package detail modal in the Projects view shows the selected project
  dependency row instead of an unrelated system package. (#60)
- `empty_trash` runs `osascript` through an injectable runner; the test suite
  can no longer touch the real Trash, and its doc comment no longer claims
  emptying is reversible. (#44)

## [0.1.47] - 2026-06-12

### Added

- File info popup: `i` in the Files pane opens a modal with full path, type,
  logical vs. allocated size (with APFS clone/sparse/compression note),
  created/modified/accessed timestamps, owner/group, permissions, hard-link
  count, and xattr count with quarantine flagging. Action keys for Quick Look,
  Finder reveal, Open, and Trash. (#20)

## [0.1.46] - 2026-06-12

### Fixed

- Homebrew cask rows now act on the real `.app` bundle: detail, Finder reveal,
  and Open follow the app bundle while uninstall keeps using the cask token.
  The Caskroom metadata path is shown separately when it differs. (#36)

## [0.1.45] - 2026-06-12

### Fixed

- Stabilized the package visibility cache regression test.

## [0.1.44] - 2026-06-12

### Fixed

- Package pane no longer rebuilds visibility indices on every rendered row
  (O(n^2) per frame); visible indices and lowercase search text are cached. (#32)
- npm global package sizing resolves from the active `npm root -g` first, so
  package lists and size lookups stay aligned under nvm/fnm. (#35)
- Top-files and reclaim-paths modals page by visible height and render their
  footers without clipping. (#37)

## [0.1.43] - 2026-06-12

### Fixed

- Size-sorted scan results no longer land on the wrong row: `apply_sort()`
  rebuilds the path-to-index map, so a directory size arriving after a
  mid-scan resort cannot be written onto an unrelated file. (#41)

## 0.1.34 - 0.1.42 (2026-06-11 - 2026-06-12)

Rapid audit-driven fix releases. Reconstructed summary; per-finding detail
lives in [docs/AUDIT.md](docs/AUDIT.md) and git history.

### Added

- Full-subtree scan mode: `S` scans every visible directory whose size is
  missing without invalidating known sizes (0.1.38). (#18)
- Clipboard copy (`y`) and open-in-Terminal (`s`) shortcuts (0.1.39). (#10)

### Fixed

- `Esc` no longer quits from the Files pane; `q` remains quit (0.1.36). (#24)
- Replaced the redundant outer scanner thread layer with a single rayon
  dispatch thread (0.1.37). (#22)
- Stale background package/reclaim results from a previous directory are
  discarded instead of being shown for the current one. (#26)
- Multi-select marks render a checkmark, clear on directory changes, and batch
  delete confirmations list the marked items. (#27)
- Rename follows the renamed entry after reload. (#28; later lost in a merge —
  reopened as #55)
- Empty Trash requires an explicit confirmation and runs on a background
  worker. (#29)
- Dependency-leaf graph covers casks, npm, cargo, and bun, not just brew
  formulae and pip. (#13)
- Baseline header chip formatting (duplicate "ago", missing separator). (#38)
- Reverse pane navigation from Files lands on Reclaim. (#39)

## 0.1.21 - 0.1.33 (2026-06-10 - 2026-06-11)

First audit wave. Highlights:

- Search-mode selection corruption fixes (#1), spinner animation during quiet
  scans (#2), permission failures surfaced as lower-bound sizes instead of
  silent 0 B (#3), scan-result salvage and cancellation (#4), `r` rescans all
  visible directories (#5), hard-link and firmlink double-counting fixes (#6),
  removed unused mouse capture (#7), accurate brew cask (#8) and pip (#9)
  sizing, mtime display (#11), project-deps rescan caching
  (#14), TUI surfaces for reclaim/top-files/disk-details/history-diff (#15),
  file operations: rename, mkdir, multi-select, batch trash (#16), per-row
  size-share bars (#17), persistent size cache (#19).

## 0.1.0 - 0.1.20 (2026-05-01 - 2026-06-10)

Initial development: TUI file browser with `getattrlistbulk(2)` bulk scanner,
disks and packages panes, CLI reports (`--top`, `--reclaim`, `--save`/`--diff`,
`--space`, `--packages`, `--thin-snapshots`), automated release workflow.
