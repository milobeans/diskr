# Changelog

All notable changes to diskr are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Package report scope controls select projects only, specific managers, no
  projects, discovery depth and root-relative subtree exclusions; JSON and text
  disclose the requested inventory scope.
- Immutable named baselines support listing, pinning, explicit prune previews,
  recoverable prune backups and bounded export/import without replacing latest
  root baselines. Saved package profiles retain roots and inventory scopes.
  Imported records retain source machine/raw-root identities; foreign profile
  execution requires creating an explicit local profile.

### Changed

- The TUI streams ready manager and project results while remaining inventory
  work continues, showing elapsed/stage progress and partial-state labels.
  Esc/Ctrl+C cancels read-only inventory and retains ready results as partial.
- Startup paints before state loading and supports immediate cancellation.
  Directory listings arrive in bounded, generation-tagged batches off the UI
  loop; stale batches cannot replace a refreshed or navigated listing.

### Fixed

- Failed or partially unreadable directory enumeration keeps its uncertainty
  visible instead of reporting an empty, fully refreshed directory.
- A project-only refresh cannot clear the partial marker left by a cancelled
  manager inventory.

## [0.1.82] - 2026-10-22

### Added

- JSON reports share a versioned schema with canonical root/scope, completeness,
  warnings, elapsed time and numeric semantics while retaining report fields.
  `--strict` exits 2 after printing partial reports; default status is unchanged.
- `--min-size` filters Top Files, reclaim and package rows; `--class` filters
  reclaim categories. Unknown or limited footprints remain visible and filters
  do not change underlying totals or completeness.

### Changed

- Compatible independent scans share bounded in-flight work with per-caller
  cancellation and fresh root/identity/policy checks. Interactive roots can be
  promoted ahead of background work without adding scanner workers; shared
  hardlink-accounting scopes remain isolated.
- Top Files avoids constructing full paths for size-rejected bulk records.
  Reclaim discovery and sizing are cancellable with Esc/Ctrl+C and on quit;
  cancelled scans do not publish confident empty results.

## [0.1.81] - 2026-10-22

### Added

- Releases provide native Apple Silicon and Intel macOS executables with
  SHA-256 checksums and dependency license notices, built and smoke-tested on
  their matching architectures.
- Compatibility CI exercises macOS 15 and 26 on both architectures, read-only
  volume discovery, and an explicit unsupported-platform build diagnostic.

### Changed

- GitHub release notes use the released changelog section rather than generated
  commit summaries.

## [0.1.80] - 2026-10-22

### Added

- Rename, mkdir and file/package search share grapheme-aware caret editing:
  Left/Right, Home/End, Backspace and Delete. Long input scrolls horizontally
  to keep its caret visible without splitting wide characters.
- `b` toggles full-width active views and side panels. Top Files is a full-width
  browser toggled with `t`; compact disks use a one-line-per-volume row budget.
  Project and Top Files rows show root-relative paths while retaining full paths.
- Terminal appearance supports dark, light, high-contrast and monochrome palettes,
  `NO_COLOR`, static scan indicators and ASCII decoration fallbacks through
  documented environment preferences. Textual selection and warning markers remain.

### Fixed

- Rename and mkdir preserve intentional leading/trailing spaces. Accepting an
  unchanged rename preserves the original raw filename, including names that
  cannot be represented as UTF-8.

### Changed

- Package lists format only viewport rows, and Files reuses measured totals
  between data/filter changes.

## [0.1.79] - 2026-10-22

### Fixed

- Project dependency counts deduplicate declared names across dependency groups
  and shared manifests. Cargo target tables, Python optional/build/Poetry groups
  and bounded local requirements includes are recognized. Malformed, unreadable,
  dynamic and unsupported manifests retain warnings and unknown or lower-bound
  counts in text, details and JSON instead of reporting a confident zero.
- APFS discovery distinguishes timeouts, tool failures and malformed output,
  retaining a machine-readable diagnostic in space reports. Discovery and
  retries use the owning mount point rather than the selected subdirectory.

## [0.1.78] - 2026-10-22

### Fixed

- Package and project footprints retain unreadable-path, skipped-mount and
  missing-file counts through lists, totals, details and JSON. Reports label
  directory trees, installed binaries, RECORD files and installer metadata
  explicitly, and describe totals as known allocation.
- Package scans retain the selected manager executable and effective roots.
  Cargo honors configured install roots, Homebrew and Bun report their active
  locations, and Python sizing follows each distribution's reported location.
  Uninstall uses the same retained manager and installation scope.

## [0.1.77] - 2026-10-22

### Fixed

- Destructive-action confirmations now keep confirm/cancel hints visible, show
  a tail-preserving target path, and identify long file or package names in
  narrow terminals.
- Package reports now distinguish absent, empty, failed, malformed and partial
  manager scans. Dependency-graph failures remain visible beside partial
  results in the TUI and JSON output instead of appearing as confident leaves.

## [0.1.76] - 2026-10-22

### Fixed

- Disk, file, package and project detail drawers now scroll their full content
  while keeping actions and the visible range pinned below the viewport.
  Disk details no longer cap the snapshot list at six entries.

- Python package apparent size now uses live file metadata for the same RECORD
  files as allocated size. Missing files and unreadable manifests are reported
  as incomplete instead of contributing stale declared bytes.
- Package-report warnings no longer hide the successfully measured package
  rows, and TUI package-scan warnings survive dependency-graph completion.
- Package and project labels now use terminal-cell width, including the list
  selection gutter, so wide Unicode names do not clip their size column.
- Files rows now reserve the selection gutter, percentage and intelligence
  badges before allocating name and optional columns, keeping visible status
  at narrow terminal widths.
- Disk details now let snapshot and APFS discovery be retried independently
  after a utility failure or timeout.
- Quit waits for in-flight Trash, uninstall, baseline-save and external-action
  jobs, cancels read-only scans, and prints their final outcomes after the
  terminal is restored.

### Changed

- Python distribution metadata is indexed once per site-packages root and
  matched by normalized name and installed version, avoiding a directory
  enumeration for every installed package.

## [0.1.74] - 2026-10-22

### Fixed

- Package-manager and macOS utility commands keep their deadline through
  output draining, limit captured output, and clean up process groups after
  a timeout. Snapshot thinning reports uncertain completion so it can be
  checked before retrying.
- Space reports query snapshots at the owning mount and expose APFS discovery
  failures instead of silently omitting container information.
- Unknown command-line options now report a usage hint; paths beginning with
  a dash remain available after `--`.
- CLI reports and help treat a closed stdout pipe as normal completion.
- Suggested follow-up commands quote paths for macOS shells, including spaces,
  apostrophes, control characters and non-UTF-8 names.
- `d` on an installed package opens its native uninstall confirmation, which
  covers multi-file installs; `d` still moves project dependency directories
  and file selections to Trash.
- Finder reveal and Open actions now report their eventual utility exit status
  through managed background jobs instead of reporting success on spawn.

## [0.1.73] - 2026-10-22

### Fixed

- Directory scans parse the native variable attribute layout consistently and
  report absent required attributes as incomplete. Mount policy is checked on
  each opened descriptor before file enumeration, including APFS helper mounts.
- Cleanup detection requires project/environment evidence, recognizes the
  selected artifact root and cache descendants, retains unknown-size findings,
  and treats simulator state and deployment output conservatively.
- Baselines and cleanup totals deduplicate hardlinks across sibling trees.
  Discovery reports disclose depth, mount and policy exclusions; Top Files
  exposes incomplete totals and empty states. Cleanup allocated bytes are
  explicitly candidates, not guaranteed space savings.
- Update epochs use an atomic operation supported by both the MSRV and current
  stable toolchain. Publication of the preceding state batch stopped during
  validation; its changes are included in this release.

- Cache freshness survives interrupted launches and changes of watched root.
  Nanosecond update epochs prevent equal-time stale writers from resurrecting
  invalidated sizes, while cached mount exclusions and scan-policy identity
  remain visible after navigation and relaunch. Incomplete sizes remain
  retryable with `S` (or `r` to refresh).
- Baseline saves cancel abandoned walks, reject older same-path writes under
  the state lock, and commit on background workers. Cache writes also run off
  the event loop with coalescing and retry backoff; damaged state is backed up
  before recovery.
- History validates its versioned records, retains unaffected baselines when
  individual records are invalid, reports empty additions and type changes,
  and retains per-child uncertainty instead of claiming reliable growth from
  incomplete scans. Persisted paths and FSEvents retain raw Unix path identity;
  the Trash bridge uses filesystem representations instead of UTF-8 conversion.

## [0.1.71] - 2026-10-22

### Fixed

- File details open a cancellable loading panel and retain their original action
  target. Delayed metadata can no longer cover a confirmation or reopen after
  cancellation or navigation; drawing and keyboard dispatch share modal priority.
- Kept filters survive refresh with the correct rows and selected path; refresh
  and scan-all honor the visible filter. Esc clears the active filter, Growth
  sorting updates as scans complete, and repeated `p` switches package views.
- Failed Trash batches no longer restore invisible marks from another directory,
  and their results retain failure reasons. Action feedback remains visible under
  kept filters; unreadable and hidden-only directories have distinct states.

## [0.1.70] - 2026-08-06

### Added

- The keyboard-help overlay now scrolls on short terminals, balances sections
  across wide columns by rendered height, and shows the visible line range.
- Package-report JSON now includes `project_inaccessible`, and file, history,
  reclaim, and project-dependency reports surface enumeration failures instead
  of silently presenting partial results as complete.

### Changed

- Filenames, paths, and external-command diagnostics are now escaped before
  human terminal rendering, while JSON continues to preserve machine-readable
  values.
- File-info collection and every move-to-Trash operation now run on guarded
  background workers; package uninstall also has a five-minute process-group
  timeout, keeping the TUI responsive on slow volumes and package managers.
- Persistent history and size-cache writers now coordinate across processes,
  merge updates and invalidations under a lock, create state files with
  owner-only permissions, and sync the containing directory after atomic
  replacement.
- File-only and Reclaim-only keys now act only in the pane where their help is
  shown. Unicode layout and truncation are grapheme-aware, including combining
  characters and emoji.
- Updated compatible dependencies, including crossterm 0.29, anyhow 1.0.104,
  ratatui-core 0.1.2, and ratatui-widgets 0.3.2.

### Fixed

- Reclaim totals now aggregate the union of finding paths: nested categories
  are counted once without dropping unrelated bytes from a parent category.
- FSEvents `MustScanSubDirs` replays now fall back to full cache revalidation,
  so cached descendants cannot remain falsely fresh after a recursive event.
- Snapshot-thinning sizes are parsed with checked decimal arithmetic, rejecting
  values above `u64::MAX` instead of rounding and clamping through `f64`.
- Scanner syscall, malformed-record, open, and directory-entry failures now
  make totals explicit lower bounds instead of being treated as successful EOF.
- Filesystems that do not support the native bulk-attribute syscall now fall
  back to compatible directory enumeration instead of returning an empty scan.
- Package warning truncation no longer splits UTF-8 and aborts release builds.
- Repeated baseline saves and overlapping Trash operations are coalesced or
  rejected without discarding marks, and disconnected workers report failure.

## [0.1.69] - 2026-06-15

### Removed

- Removed the `Space` / Quick Look preview action, which added little over
  revealing the file. Reveal in Finder (`f`) — now also shown in the footer —
  Open (`O`), copy path (`y`), and open in Terminal (`s`) are unchanged.

## [0.1.68] - 2026-06-15

### Changed

- The in-progress scanning indicator is now a four-cell braille "beam" that
  sweeps left to right (the "scan" preset from the rattles spinner library) in
  place of the single spinning braille dot. It appears wherever a scan or load
  is running: the files-pane size cell, the reclaim, top-files, disk-info,
  package, and dependency loading labels, and the per-selection status line. The
  larger centered package-data splash spinner is unchanged.

## [0.1.67] - 2026-06-15

### Changed

- The disk-info, file-info, package-detail, and project-deps detail modals now
  share one centered placement and chrome, so they open at a consistent size
  with consistent wrapping instead of four slightly different dimensions.

## [0.1.66] - 2026-06-15

### Added

- On launch, diskr now consults macOS FSEvents to learn which directories
  changed since the previous session and revalidates only those, so unchanged
  directory trees keep their cached sizes with no rescan. The last event id and
  volume UUID are persisted next to the size cache; the replay falls back to the
  age-based revalidation on first run, a different or reformatted volume, or
  dropped/coalesced events.

## [0.1.65] - 2026-06-15

### Added

- The file browser now tags directories the reclaim engine recognizes
  (`node_modules`, `target`, `.venv`, `__pycache__`, `~/Library/Caches`, …) with
  a colored class chip (`cache`/`build`/`keep`), surfacing reclaim knowledge on
  the main surface — name/path matching only, with no extra I/O while rendering.
- A growth sort mode (in the `o` sort cycle) and a per-row `↑`/`↓` badge showing
  how much a directory changed since its previous scan, so revalidation surfaces
  what actually grew instead of the same familiar top-N.
- `diskr --packages --json` now includes the canonical project root as `path`,
  matching the other JSON reports.

### Changed

- The top-files and reclaim-paths overlays now share one list-modal component,
  so they have identical placement, chrome, and footer conventions.
- The side column now collapses to a compact one-line-per-disk summary while
  browsing files, giving the files pane the reclaimed width, and expands back to
  full disk gauges when the Disks or Packages pane is focused or packages load.
- Files-pane density pass: the pane title now summarizes item count, total size,
  scan coverage, the active sort, and any marked-items total; sizes are colored
  by magnitude (dim sub-MiB, green MiB, bold amber GiB+); the modified column
  shows whenever the pane is wide enough; marked rows keep their type glyph next
  to the check; the app header drops the always-on "sort/hidden off" defaults;
  and the `?` overlay gained a size-marker legend.

- The footer help strip is now context-sensitive: it lists the keys for the
  focused pane, truncates to fit narrow terminals, and always keeps `?` visible
  instead of rendering one long static line. Destructive keys (`d`, `E`, `x`)
  are highlighted in red in both the footer and the `?` overlay, and the README
  key table now documents every binding and is checked against the keymap by a
  test so the two cannot drift.

### Fixed

- Background history-diff and top-files scans now stop early when they are
  superseded or closed — navigating between baselined directories, or pressing
  Esc on the top-files modal — instead of running a full directory walk to
  completion only to discard the result, so redundant walks no longer pile up.
- Moving the cursor onto an unsized directory while a scan is running no longer
  cancels the in-flight batch: such directories are queued and scanned once the
  batch finishes, so spinners and progress are not wiped out by a premature
  "scan complete".
- A fully-cached directory view is no longer a frozen snapshot. When nothing is
  missing, diskr now revalidates the oldest stale directories in the background
  (bounded, oldest first) and reports honest status ("sizes from cache (oldest
  …) · verifying N") instead of "cache hit · all sizes known", so a folder that
  grew since the last scan stops sitting at its old rank.
- The Reclaim pane's status line and the Space/f/O/y/s actions now target the
  highlighted finding once the paths modal is closed, instead of a path from a
  previously-opened finding.
- The batch-delete confirmation now counts marked files, not just cached
  directories, and shows the total as a lower bound (`≥`) when a marked
  directory has no cached size, so the common "mark some files" case no longer
  reads as zero bytes.
- Package navigation and actions can no longer reach hidden packages through a
  not-yet-built visibility cache: an empty or unbuilt package filter now
  consistently exposes zero rows everywhere.
- On short terminals the packages pane is given rows whenever it is focused or
  loaded, so `Tab`/`p` can no longer move focus onto an invisible, zero-height
  packages pane.
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
