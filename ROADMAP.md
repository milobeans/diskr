# diskr Roadmap

`diskr` should set itself apart as a fast, macOS-native terminal storage manager.
The strongest direction is to explain why space disappeared, what changed over
time, and what is actually reclaimable.

This file tracks product direction at the feature level. Shipped work is
recorded in [CHANGELOG.md](CHANGELOG.md). Detailed maintenance trackers, audit
notes, and workflow instructions are private project materials and are not
published in this repository.

## Highest-Leverage Features

- APFS phantom-space view: show local Time Machine snapshots, purgeable space,
  and other space that normal file trees do not explain. Use `tmutil
  listlocalsnapshots /` for discovery and consider an explicit, confirmed
  snapshot-thinning action. Implemented first CLI surface with
  `diskr --space [--json] [PATH]` and guarded `diskr --thin-snapshots SIZE
  [--yes] [PATH]`.
- Logical vs. physical size: track apparent file size and allocated on-disk
  size separately so APFS clones, sparse files, and compressed files do not
  mislead users. Implemented for scanner results, TUI sorting, selected-item
  status, and `--top` reports.
- Global largest-files view: provide a flat top-N list for the current subtree
  instead of forcing users to drill down one directory at a time. The scanner
  keeps a bounded heap of large file entries while it already walks the tree;
  `diskr --top N [--json] [PATH]` is the first shipped surface.
- Scan snapshots and diff mode: persist scan summaries and answer "what grew
  since the last scan?" This is the clearest path from one-off cleanup to storage
  observability. Implemented with `diskr --save [--json] [PATH]` and
  `diskr --diff [--json] [PATH]`; baselines are stored per path under
  `~/Library/Application Support/diskr/history.json` and `--diff` reports
  per-child growth, additions, and removals without changing the baseline.
- Reclaimability scoring: categorize large items by whether they are safe to
  delete, cheap to regenerate, expensive to rebuild, or user-owned and risky.
  Implemented with `diskr --reclaim [--json] [PATH]`, which tags every finding
  as safe, regenerable, or risky.

## macOS Cleanup Intelligence

- Local snapshots and purgeable storage: explain when `df`, Finder, and visible
  directory sizes disagree, then offer careful cleanup operations only after
  showing impact and risk. Implemented local snapshot listing, APFS container
  free-space reporting, user-available/free-block gap reporting, and dry-run
  snapshot thinning. Snapshot byte sizes remain unavailable because `tmutil`
  does not report them directly.
- Developer cache detector: identify common reclaimable storage such as Xcode
  `DerivedData`, Xcode archives, iOS `DeviceSupport`, CoreSimulator devices,
  Homebrew cache, Cargo cache, npm/yarn/pnpm caches, pip/uv caches, Docker
  images and volumes, language build artifacts, and repeated `node_modules`
  trees. Implemented in `diskr --reclaim`: a fixed-location pass for well-known
  caches plus a bounded recursive pass for repeated build-artifact directories
  (`node_modules`, `target`, `.venv`, `__pycache__`, `.next`, `.gradle`).
- Stale-file finder: combine size with macOS last-used metadata such as
  Spotlight `kMDItemLastUsedDate` or access-time metadata to surface large files
  that have not been opened recently.
- Trash and browser-cache awareness: include Trash, browser caches, downloads,
  and app caches in a separate "easy wins" view, with conservative defaults.

## Navigation and Action Improvements

- Quick Look was removed after proving redundant with the more useful Finder and
  default-app actions; the TUI keeps the smaller action set below.
- Reveal in Finder: open Finder with the selected path highlighted via `open
  -R`. Implemented with the `f` shortcut.
- Open selected item: launch the selected file or directory with the default
  macOS app using `open`. Implemented with the `O` shortcut.
- Duplicate finder: detect duplicate large files and consider APFS clone-based
  dedupe with `clonefile(2)` so duplicates can reclaim physical space without
  destroying logical copies.
- Saved roots and profiles: let users pin common scan targets and switch between
  named views such as `dev`, `media`, `downloads`, and external volumes.

## TUI Density and Declutter

Shipped behavior:

- Size-share denominators cover the filtered directory, not its scroll window.
- Context-sensitive footer and scrollable `?` help use a shared keymap table.
- Files has magnitude-coded sizes, a sort/coverage summary and mark totals.
  Modified time appears when sorting by time or when width permits; there is
  no separate column-header row and no always-visible modified column.
- Reclaim classification and growth chips are inline in Files. Independently
  measured child sizes remain distinct from a unique-inode root total.
- `b` toggles full-width active views and side panels. Compact disks use one
  line per volume; package lists format only their viewport. Root-relative
  project/Top Files rows retain full paths in details or the selected footer.
- `t` toggles the full-width Top Files browser; `t` or Esc returns. Reclaim
  retains a selectable panel and a separate paths view, not a second Files mode.
- File, disk and package details share scrolling with pinned action footers.
  Confirmations remain separate, target-bound overlays. The product does not
  have only two internal modal types.
- Rename/mkdir/search use a shared grapheme-aware caret editor. Terminal
  palettes, monochrome/ASCII decorations and static activity indicators are
  configurable without altering filesystem names.

Remaining design choices include a dedicated Files column-header row and a
uniform full-screen reclaim-paths browser. These are not completed promises;
responsive column visibility remains intentional. Correctness and safe action
targets continue to take precedence over additional chrome.

## Scriptability and Reports

- Non-interactive top-N mode: support commands such as `diskr --top 20 --json
  ~/` for shell scripts, dashboards, and CI-style checks. Implemented for
  largest-file reports.
- Machine-readable output: expose JSON summaries for disks, largest files,
  reclaimable categories, and scan diffs. Largest-file, APFS space,
  reclaimability, and save/diff JSON are implemented.
- Monitor mode: warn when a watched path crosses a size threshold or grows
  quickly between snapshots.
- Import/export scan data: make snapshots portable and diffable so users can
  compare machines or hand off diagnostics.

## Product Guardrails

- Keep the default TUI fast and simple. New intelligence should be discoverable
  from the existing file and disk panes, not require a new heavy workflow.
- Prefer explain-first cleanup. Any destructive or semi-destructive action should
  show what will happen, how much space it should reclaim, and what the user may
  lose.
- Stay macOS-specific where it creates leverage. APFS, Time Machine snapshots,
  FSEvents, Finder, and clone files are where `diskr` can beat generic tools.
- Keep automation useful. Every major insight in the TUI should eventually have
  a scriptable equivalent.
- Chrome must earn its pixels. Prefer densifying the files pane over adding
  panes or modals, show deviations rather than defaults, and do not repeat
  information that is already visible in another region of the screen.

## Suggested Build Order

1. Done: add logical-vs-physical size to the scanner and UI.
2. Done: add global top-N largest files for the current subtree.
3. Done: add reveal-in-Finder and open-with-default-app shortcuts; remove the
   redundant Quick Look action after use showed it added little value.
4. Done: add APFS snapshot and purgeable-space reporting.
5. Done: add persisted scan snapshots and diff mode (`--save`/`--diff`).
6. Done: add developer-cache detection and reclaimability scoring (`--reclaim`).
7. Done: add JSON/reporting commands for `--top`, `--reclaim`, `--save`,
   `--diff`, and `--space`.
8. Done: bring the intelligence into the TUI — reclaim pane, Top Files browser,
   disk details, history diff header, file operations, size-share bars,
   persistent size cache.
9. Done: correctness/safety burn-down covering state integrity,
   destructive-action responsiveness, traversal accuracy, terminal safety, and
   recursive cache invalidation.
10. Shipped density improvements: corrected denominators, context-sensitive
    and scrollable help, inline reclaim classification, full-width views and
    shared scrolling details. Remaining layout choices are listed above.
11. Next: stale-file finder (size combined with last-used metadata); pairs
    with age-dimming and reclaim classification chips.
12. Next: duplicate finder with APFS clone-based dedupe (`clonefile(2)`).
