mod app;
mod bulkstat;
mod fs_ops;
mod fsevents;
mod history;
mod keymap;
mod packages;
mod pool;
mod reclaim;
mod scanner;
mod space;
mod state;
mod terminal_backend;
mod terminal_text;
mod ui;

use anyhow::{bail, Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui_core::terminal::Terminal;
use std::{
    cell::RefCell,
    ffi::OsString,
    fmt,
    io::{self, IsTerminal, Write as _},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::Duration,
};

use app::{format_elapsed, human, ActiveModal, App, Focus};
use terminal_backend::CrosstermBackend;
use terminal_text::SafePath;

thread_local! {
    static CLI_OUTPUT: RefCell<io::BufWriter<io::Stdout>> =
        RefCell::new(io::BufWriter::new(io::stdout()));
}

fn write_cli(args: fmt::Arguments<'_>) -> io::Result<()> {
    CLI_OUTPUT.with(|output| output.borrow_mut().write_fmt(args))
}

fn flush_cli() -> io::Result<()> {
    CLI_OUTPUT.with(|output| output.borrow_mut().flush())
}

macro_rules! cli_print {
    ($($arg:tt)*) => {{
        if let Err(error) = write_cli(format_args!($($arg)*)) {
            return Err(error.into());
        }
    }};
}

macro_rules! cli_println {
    () => {{
        cli_print!("\n");
    }};
    ($($arg:tt)*) => {{
        if let Err(error) = write_cli(format_args!("{}\n", format_args!($($arg)*))) {
            return Err(error.into());
        }
    }};
}

fn main() {
    if let Err(err) = run_main() {
        if err
            .chain()
            .filter_map(|cause| cause.downcast_ref::<io::Error>())
            .any(|cause| cause.kind() == io::ErrorKind::BrokenPipe)
        {
            return;
        }
        eprintln!("error: {}", terminal_text::sanitize(&format!("{err:#}")));
        std::process::exit(1);
    }
}

fn run_main() -> Result<()> {
    let result = match parse_args(std::env::args_os().skip(1))? {
        CliAction::Help => print_help(),
        CliAction::Version => {
            cli_println!("diskr {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        CliAction::Top { path, limit, json } => print_top(path, limit, json),
        CliAction::Reclaim { path, json } => print_reclaim(path, json),
        CliAction::Save { path, json } => save_baseline(path, json),
        CliAction::Diff { path, json } => print_diff(path, json),
        CliAction::Space { path, json } => print_space(path, json),
        CliAction::Packages { path, json } => print_packages(path, json),
        CliAction::ThinSnapshots {
            path,
            bytes,
            confirmed,
        } => thin_snapshots(path, bytes, confirmed),
        CliAction::Run(start) => run_app(start),
    };
    result?;
    flush_cli()?;
    Ok(())
}

fn run_app(start: PathBuf) -> Result<()> {
    let start = canonical_dir(start)?;
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("diskr requires an interactive terminal");
    }

    let mut app = App::new(start)?;

    // Install the panic hook before entering the TUI. The hook restores the
    // terminal first so the panic message lands on a sane screen, then
    // invokes the previous hook (which prints the message). Panic hooks run
    // before abort, so this also covers `panic = "abort"` in release builds
    // where Drop never executes.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        prev_hook(info);
    }));

    let _terminal_guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let res = run(&mut terminal, &mut app);
    let cache_res = app.save_size_cache();
    let cursor_res = terminal.show_cursor();

    res?;
    cache_res?;
    cursor_res?;
    let shutdown_summary = app.shutdown_summary();
    drop(terminal);
    drop(_terminal_guard);
    if let Some(summary) = shutdown_summary {
        eprintln!("diskr: {}", terminal_text::sanitize(&summary));
    }
    Ok(())
}

fn canonical_dir(path: PathBuf) -> Result<PathBuf> {
    if !path.exists() {
        bail!("path does not exist: {}", path.display());
    }
    if !path.is_dir() {
        bail!("path is not a directory: {}", path.display());
    }
    path.canonicalize()
        .with_context(|| format!("resolve {}", path.display()))
}

fn shell_quote_path(path: &Path) -> String {
    let bytes = path.as_os_str().as_bytes();
    if let Ok(utf8) = std::str::from_utf8(bytes) {
        if !utf8.chars().any(char::is_control) {
            return format!("'{}'", utf8.replace('\'', "'\\''"));
        }
    }
    let mut quoted = String::from("$'");
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(quoted, "\\x{byte:02x}");
    }
    quoted.push('\'');
    quoted
}

enum CliAction {
    Run(PathBuf),
    Top {
        path: PathBuf,
        limit: usize,
        json: bool,
    },
    Reclaim {
        path: PathBuf,
        json: bool,
    },
    Save {
        path: PathBuf,
        json: bool,
    },
    Diff {
        path: PathBuf,
        json: bool,
    },
    Space {
        path: PathBuf,
        json: bool,
    },
    Packages {
        path: PathBuf,
        json: bool,
    },
    ThinSnapshots {
        path: PathBuf,
        bytes: u64,
        confirmed: bool,
    },
    Help,
    Version,
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<CliAction> {
    let mut args = args.into_iter();
    let mut path: Option<PathBuf> = None;
    let mut top_limit: Option<usize> = None;
    let mut reclaim_report = false;
    let mut save_baseline = false;
    let mut diff_baseline = false;
    let mut space_report = false;
    let mut packages_report = false;
    let mut thin_snapshots: Option<u64> = None;
    let mut confirmed = false;
    let mut json = false;
    let mut separator_seen = false;

    while let Some(arg) = args.next() {
        if separator_seen {
            set_cli_path(&mut path, arg)?;
            continue;
        }

        let arg_text = arg.to_string_lossy().into_owned();
        match arg_text.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            "--" => {
                separator_seen = true;
            }
            "--json" => {
                json = true;
            }
            "--yes" => {
                confirmed = true;
            }
            "--reclaim" => {
                reclaim_report = true;
            }
            "--save" => {
                save_baseline = true;
            }
            "--diff" => {
                diff_baseline = true;
            }
            "--space" => {
                space_report = true;
            }
            "--packages" => {
                packages_report = true;
            }
            "--thin-snapshots" => {
                let Some(size) = args.next() else {
                    bail!("usage: diskr --thin-snapshots SIZE [--yes] [PATH]");
                };
                thin_snapshots = Some(space::parse_byte_size(&size.to_string_lossy())?);
            }
            _ if arg_text.starts_with("--thin-snapshots=") => {
                let size = arg_text.trim_start_matches("--thin-snapshots=");
                thin_snapshots = Some(space::parse_byte_size(size)?);
            }
            "--top" => {
                let Some(limit) = args.next() else {
                    bail!("usage: diskr --top N [--json] [PATH]");
                };
                top_limit = Some(parse_top_limit(&limit.to_string_lossy())?);
            }
            _ if arg_text.starts_with("--top=") => {
                let limit = arg_text.trim_start_matches("--top=");
                top_limit = Some(parse_top_limit(limit)?);
            }
            _ if arg_text.starts_with('-') => {
                bail!("unknown option {arg_text}; use -- before a path beginning with -");
            }
            _ => {
                set_cli_path(&mut path, arg)?;
            }
        }
    }

    if json
        && top_limit.is_none()
        && !reclaim_report
        && !save_baseline
        && !diff_baseline
        && !space_report
        && !packages_report
    {
        bail!("--json requires --top, --reclaim, --save, --diff, --space, or --packages");
    }
    if confirmed && thin_snapshots.is_none() {
        bail!("--yes requires --thin-snapshots");
    }
    if separator_seen && path.is_none() {
        bail!("usage: diskr [PATH]");
    }
    let mode_count = usize::from(top_limit.is_some())
        + usize::from(reclaim_report)
        + usize::from(save_baseline)
        + usize::from(diff_baseline)
        + usize::from(space_report)
        + usize::from(packages_report)
        + usize::from(thin_snapshots.is_some());
    if mode_count > 1 {
        bail!("choose only one of --top, --reclaim, --save, --diff, --space, --packages, or --thin-snapshots");
    }

    match top_limit {
        Some(limit) => Ok(CliAction::Top {
            path: path.unwrap_or_else(dirs_home),
            limit,
            json,
        }),
        None if reclaim_report => Ok(CliAction::Reclaim {
            path: path.unwrap_or_else(dirs_home),
            json,
        }),
        None if save_baseline => Ok(CliAction::Save {
            path: path.unwrap_or_else(dirs_home),
            json,
        }),
        None if diff_baseline => Ok(CliAction::Diff {
            path: path.unwrap_or_else(dirs_home),
            json,
        }),
        None if space_report => Ok(CliAction::Space {
            path: path.unwrap_or_else(dirs_home),
            json,
        }),
        None if packages_report => Ok(CliAction::Packages {
            path: path.unwrap_or_else(dirs_home),
            json,
        }),
        None if thin_snapshots.is_some() => Ok(CliAction::ThinSnapshots {
            path: path.unwrap_or_else(dirs_home),
            bytes: thin_snapshots.unwrap_or_default(),
            confirmed,
        }),
        None => Ok(CliAction::Run(path.unwrap_or_else(dirs_home))),
    }
}

fn set_cli_path(path: &mut Option<PathBuf>, value: OsString) -> Result<()> {
    if path.replace(PathBuf::from(value)).is_some() {
        bail!("usage: diskr [PATH]");
    }
    Ok(())
}

fn parse_top_limit(value: &str) -> Result<usize> {
    let limit = value
        .parse::<usize>()
        .with_context(|| format!("invalid --top value: {value}"))?;
    if limit == 0 {
        bail!("--top must be greater than zero");
    }
    Ok(limit)
}

fn print_help() -> Result<()> {
    cli_print!(
        "\
diskr {}

Lightweight terminal file explorer and disk/storage manager for macOS.

Usage:
  diskr [PATH]
  diskr -- PATH
  diskr --top N [--json] [PATH]
  diskr --reclaim [--json] [PATH]
  diskr --save [--json] [PATH]
  diskr --diff [--json] [PATH]
  diskr --space [--json] [PATH]
  diskr --packages [--json] [PATH]
  diskr --thin-snapshots SIZE [--yes] [PATH]

Keys:
",
        env!("CARGO_PKG_VERSION")
    );
    print_key_help()
}

fn print_key_help() -> Result<()> {
    for section in keymap::HELP_SECTIONS {
        cli_println!("\n{}:", section.title);
        for binding in section.bindings {
            cli_println!("  {:<16} {}", binding.key, binding.action);
        }
    }
    Ok(())
}

fn print_top(path: PathBuf, limit: usize, json: bool) -> Result<()> {
    let path = canonical_dir(path)?;

    let scan = bulkstat::scan_dir(&path, limit);
    if json {
        let files: Vec<_> = scan
            .largest_files
            .iter()
            .map(|file| {
                serde_json::json!({
                    "path": file.path.to_string_lossy(),
                    "logical": file.size.logical,
                    "allocated": file.size.allocated,
                })
            })
            .collect();
        let report = serde_json::json!({
            "path": path.to_string_lossy(),
            "limit": limit,
            "total_logical": scan.size.logical,
            "total_allocated": scan.size.allocated,
            "inaccessible": scan.inaccessible,
            "skipped_mounts": scan.skipped_mounts,
            "files": files,
        });
        cli_println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    cli_println!(
        "Top {} files by allocated size under {}",
        limit,
        SafePath(&path)
    );
    cli_println!(
        "Total: {} disk / {} apparent",
        human(scan.size.allocated),
        human(scan.size.logical)
    );
    if scan.inaccessible > 0 {
        cli_println!(
            "Warning: {} directories were unreadable; totals are lower bounds.",
            scan.inaccessible
        );
    }
    if scan.skipped_mounts > 0 {
        cli_println!(
            "Note: skipped {} mounted volumes on other devices.",
            scan.skipped_mounts
        );
    }
    if scan.largest_files.is_empty() {
        cli_println!("No regular files found.");
        return Ok(());
    }
    for (index, file) in scan.largest_files.iter().enumerate() {
        cli_println!(
            "{:>2}. {:>22}  {}",
            index + 1,
            top_size_label(file.size),
            SafePath(&file.path)
        );
    }
    Ok(())
}

fn top_size_label(size: bulkstat::SizeInfo) -> String {
    if size.allocated == size.logical {
        human(size.logical)
    } else {
        format!(
            "{} disk / {} apparent",
            human(size.allocated),
            human(size.logical)
        )
    }
}

fn reclaim_size_label(finding: &reclaim::Finding) -> String {
    let label = top_size_label(finding.size);
    if finding.inaccessible > 0 {
        format!("≥{label}")
    } else {
        label
    }
}

fn print_reclaim(path: PathBuf, json: bool) -> Result<()> {
    let path = canonical_dir(path)?;

    let report = reclaim::report(&path);
    if json {
        let findings: Vec<_> = report
            .findings
            .iter()
            .map(|finding| {
                serde_json::json!({
                    "label": finding.label,
                    "class": finding.class.label(),
                    "count": finding.count,
                    "logical": finding.size.logical,
                    "allocated": finding.size.allocated,
                    "inaccessible": finding.inaccessible,
                    "skipped_mounts": finding.skipped_mounts,
                    "rollup": finding.rollup,
                    "note": finding.note,
                    "paths": finding
                        .paths
                        .iter()
                        .map(|p| p.to_string_lossy())
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        let value = serde_json::json!({
            "root": report.root.to_string_lossy(),
            "total_logical": report.total.logical,
            "total_allocated": report.total.allocated,
            "inaccessible": report.inaccessible,
            "skipped_mounts": report.skipped_mounts,
            "findings": findings,
            "scope": report.scope.json(),
            "measurement": "candidate allocated bytes, not guaranteed freed space",
            "estimated_freed_bytes": null,
        });
        cli_println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    cli_println!("Cleanup candidates under {}", SafePath(&report.root));
    cli_println!(
        "Total: {} disk / {} apparent",
        human(report.total.allocated),
        human(report.total.logical)
    );
    if report.inaccessible > 0 {
        cli_println!(
            "Warning: {} directories were unreadable; totals are lower bounds.",
            report.inaccessible
        );
    }
    if report.skipped_mounts > 0 {
        cli_println!(
            "Note: skipped {} mounted volumes on other devices.",
            report.skipped_mounts
        );
    }
    cli_println!("{}", report.scope.summary());
    if report.findings.is_empty() {
        cli_println!("No known cleanup candidates found within the searched scope.");
        return Ok(());
    }
    cli_println!("Allocated candidate bytes are not guaranteed freed space: hardlinks, APFS clones and snapshots can retain blocks. Moving items to Trash does not free space until Trash is emptied.");
    for finding in &report.findings {
        let count = if finding.count > 1 {
            format!(" (x{})", finding.count)
        } else {
            String::new()
        };
        // Roll-up rows are shown for context but their bytes are already
        // counted inside the child findings; mark them so readers are not
        // misled into thinking they add to the total.
        let rollup_suffix = if finding.rollup { " [subtotal]" } else { "" };
        cli_println!(
            "{:>22}  [{:^11}]  {}{}{}",
            reclaim_size_label(finding),
            finding.class.label(),
            finding.label,
            count,
            rollup_suffix
        );
        cli_println!("                          {}", finding.note);
    }
    cli_println!("\nThis is a report only; diskr does not delete anything here.");
    Ok(())
}

fn save_baseline(path: PathBuf, json: bool) -> Result<()> {
    let path = canonical_dir(path)?;
    let record = history::save(&path)?;
    let total = record.total();
    if json {
        let children: Vec<_> = record
            .children
            .iter()
            .map(|child| {
                serde_json::json!({
                    "name": child.name,
                    "is_dir": child.is_dir,
                    "logical": child.size.logical,
                    "allocated": child.size.allocated,
                })
            })
            .collect();
        let value = serde_json::json!({
            "path": record.path.to_string_lossy(),
            "timestamp": record.timestamp,
            "total_logical": total.logical,
            "total_allocated": total.allocated,
            "inaccessible": record.inaccessible(),
            "children": children,
        });
        cli_println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    cli_println!("Saved baseline for {}", SafePath(&record.path));
    cli_println!(
        "Total: {} disk / {} apparent across {} entries",
        human(total.allocated),
        human(total.logical),
        record.children.len()
    );
    if record.inaccessible() > 0 {
        cli_println!(
            "Warning: {} directories were unreadable; totals are lower bounds.",
            record.inaccessible()
        );
    }
    cli_println!(
        "Compare later with: diskr --diff {}",
        shell_quote_path(&record.path)
    );
    Ok(())
}

fn print_diff(path: PathBuf, json: bool) -> Result<()> {
    let path = canonical_dir(path)?;
    let report = history::diff(&path)?;
    if json {
        let changes: Vec<_> = report
            .changes
            .iter()
            .map(|change| {
                serde_json::json!({
                    "name": change.name,
                    "status": change_status(change),
                    "before_logical": change.before.map(|s| s.logical),
                    "before_allocated": change.before.map(|s| s.allocated),
                    "after_logical": change.after.map(|s| s.logical),
                    "after_allocated": change.after.map(|s| s.allocated),
                    "delta_logical": change.delta_logical().to_string(),
                    "delta_allocated": change.delta_allocated().to_string(),
                    "before_inaccessible": change.before_inaccessible,
                    "after_inaccessible": change.after_inaccessible,
                    "before_skipped_mounts": change.before_skipped_mounts,
                    "after_skipped_mounts": change.after_skipped_mounts,
                    "before_is_dir": change.before_is_dir,
                    "after_is_dir": change.after_is_dir,
                    "delta_complete": change.complete(),
                })
            })
            .collect();
        let value = serde_json::json!({
            "path": report.path.to_string_lossy(),
            "baseline_timestamp": report.baseline_timestamp,
            "current_timestamp": report.current_timestamp,
            "before_total_logical": report.before_total.logical,
            "before_total_allocated": report.before_total.allocated,
            "after_total_logical": report.after_total.logical,
            "after_total_allocated": report.after_total.allocated,
            "baseline_inaccessible": report.baseline_inaccessible,
            "current_inaccessible": report.current_inaccessible,
            "delta_complete": report.complete(),
            "baseline_skipped_mounts": report.baseline_skipped_mounts,
            "current_skipped_mounts": report.current_skipped_mounts,
            "total_delta_logical": report.total_delta_logical().to_string(),
            "total_delta_allocated": report.total_delta_allocated().to_string(),
            "changes": changes,
        });
        cli_println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    cli_println!("Diff for {}", SafePath(&report.path));
    cli_println!(
        "Baseline captured {}",
        format_elapsed(
            report
                .current_timestamp
                .saturating_sub(report.baseline_timestamp)
        )
    );
    cli_println!(
        "Total: {} disk → {} disk  ({})",
        human(report.before_total.allocated),
        human(report.after_total.allocated),
        format_signed_bytes(report.total_delta_allocated())
    );
    if !report.complete() {
        cli_println!(
            "Warning: unreadable paths (baseline {}, current {}), skipped mounts (baseline {}, current {}); totals are lower bounds and deltas are indeterminate.",
            report.baseline_inaccessible, report.current_inaccessible, report.baseline_skipped_mounts, report.current_skipped_mounts
        );
    }
    if report.changes.is_empty() {
        if !report.complete() {
            cli_println!("No observed changes; comparison incomplete.");
        } else {
            cli_println!("No changes since the baseline.");
        }
        return Ok(());
    }
    for change in &report.changes {
        cli_println!(
            "{:>12}  {:<9}  {}",
            format_signed_bytes(change.delta_allocated()),
            change_status(change),
            terminal_text::sanitize(&change.name)
        );
    }
    Ok(())
}

fn change_status(change: &history::ChildChange) -> &'static str {
    if !change.complete() {
        return "uncertain";
    }
    if change.before_is_dir != change.after_is_dir
        && change.before.is_some()
        && change.after.is_some()
    {
        return "type changed";
    }
    match (change.before, change.after) {
        (None, Some(_)) => "new",
        (Some(_), None) => "removed",
        _ if change.delta_allocated() > 0
            || (change.delta_allocated() == 0 && change.delta_logical() > 0) =>
        {
            "grew"
        }
        _ => "shrank",
    }
}

fn format_signed_bytes(delta: i128) -> String {
    let magnitude = delta.unsigned_abs().min(u128::from(u64::MAX)) as u64;
    let sign = if delta < 0 { "-" } else { "+" };
    format!("{sign}{}", human(magnitude))
}

fn print_space(path: PathBuf, json: bool) -> Result<()> {
    let path = canonical_dir(path)?;
    let report = space::report_for_path(&path)?;
    if json {
        let snapshots: Vec<_> = report
            .local_snapshots
            .names
            .iter()
            .map(|name| serde_json::json!({ "name": name }))
            .collect();
        let apfs_container = report.apfs_container.as_ref().map(|container| {
            serde_json::json!({
                "reference": container.reference,
                "size": container.size,
                "free": container.free,
            })
        });
        let value = serde_json::json!({
            "path": report.path.to_string_lossy(),
            "mount": report.mount.to_string_lossy(),
            "device": report.device,
            "filesystem": report.fs_type,
            "total": report.total,
            "used": report.used,
            "free": report.free,
            "available": report.available,
            "free_not_user_available": report.unavailable_free(),
            "apfs_container": apfs_container,
            "apfs_error": report.apfs_error,
            "local_snapshots": snapshots,
            "local_snapshot_error": report.local_snapshots.error,
        });
        cli_println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    cli_println!("Space report for {}", SafePath(&report.path));
    cli_println!(
        "Mount: {} ({}, {})",
        SafePath(&report.mount),
        terminal_text::sanitize(&report.fs_type),
        terminal_text::sanitize(&report.device)
    );
    cli_println!("Total: {}", human(report.total));
    cli_println!("Used: {}", human(report.used));
    cli_println!("Free blocks: {}", human(report.free));
    cli_println!("Available to user: {}", human(report.available));
    let unavailable = report.unavailable_free();
    if unavailable > 0 {
        cli_println!("Free but not user-available: {}", human(unavailable));
    }
    if let Some(container) = &report.apfs_container {
        cli_println!(
            "APFS container {}: {} total / {} free",
            terminal_text::sanitize(&container.reference),
            human(container.size),
            human(container.free)
        );
    }
    if let Some(error) = &report.apfs_error {
        cli_println!(
            "APFS container: unavailable ({})",
            terminal_text::sanitize(error)
        );
    }
    match &report.local_snapshots.error {
        Some(error) if !error.is_empty() => {
            cli_println!(
                "Local snapshots: unavailable ({})",
                terminal_text::sanitize(error)
            );
        }
        _ if report.local_snapshots.names.is_empty() => {
            cli_println!("Local snapshots: none reported by tmutil");
        }
        _ => {
            cli_println!("Local snapshots: {}", report.local_snapshots.names.len());
            for name in &report.local_snapshots.names {
                cli_println!("  {}", terminal_text::sanitize(name));
            }
            cli_println!(
                "Snapshot sizes are not reported by tmutil. To request reclamation, first preview with: diskr --thin-snapshots 10G {}",
                shell_quote_path(&report.path)
            );
        }
    }
    Ok(())
}

fn print_packages(path: PathBuf, json: bool) -> Result<()> {
    let path = canonical_dir(path)?;
    let reports = packages::scan_managers();
    let project_scan = packages::scan_project_deps(&path, 5);
    let project_deps = &project_scan.projects;
    let graph = packages::scan_dep_graph(&reports);

    if json {
        let managers: Vec<_> = reports
            .iter()
            .map(|r| {
                let pkgs: Vec<_> = r
                    .packages
                    .iter()
                    .map(|p| {
                        let dep_info = graph.get(r.manager, &p.name);
                        let evidence = dep_info
                            .map(|info| info.evidence.label())
                            .unwrap_or("untracked");
                        serde_json::json!({
                            "name": p.name,
                            "version": p.version,
                            "logical": p.size.map(|s| s.logical),
                            "allocated": p.size.map(|s| s.allocated),
                            "footprint": p.coverage.json(),
                            "path": p.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                            "metadata_path": p.metadata_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                            "dependency_evidence": evidence,
                            "dependencies": dep_info.map(|info| &info.dependencies),
                            "dependents": dep_info.map(|info| &info.dependents),
                        })
                    })
                    .collect();
                let mut obj = serde_json::json!({
                    "manager": r.manager.label(),
                    "available": r.available,
                    "scan_state": r.state.label(),
                    "environment": r.environment.as_ref().map(|environment| environment.json()),
                    "count": r.packages.len(),
                    "total_logical": r.total_size.logical,
                    "total_allocated": r.total_size.allocated,
                    "total_basis": "sum of known package footprints",
                    "size_coverage": {"unknown": r.unknown_sizes(), "partial": r.partial_sizes(), "scope_limited": r.limited_sizes()},
                    "packages": pkgs,
                });
                if let Some(w) = &r.warning {
                    obj["warning"] = serde_json::json!(w);
                }
                obj
            })
            .collect();
        let projects: Vec<_> = project_deps
            .iter()
            .map(|d| {
                serde_json::json!({
                    "path": d.path.to_string_lossy(),
                    "manager": d.manager_label,
                    "manifest": d.manifest,
                    "dep_count": d.dep_count,
                    "deps_logical": d.deps_size.map(|s| s.logical),
                    "deps_allocated": d.deps_size.map(|s| s.allocated),
                    "footprint": d.coverage.json(),
                })
            })
            .collect();
        let value = serde_json::json!({
            "path": path.to_string_lossy(),
            "project_inaccessible": project_scan.inaccessible,
            "project_scope": project_scan.scope.json(),
            "system_packages": managers,
            "project_dependencies": projects,
            "dependency_graph_warnings": graph.warnings().iter().map(|warning| serde_json::json!({
                "manager": warning.manager.label(),
                "message": warning.message,
            })).collect::<Vec<_>>(),
        });
        cli_println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    cli_println!("Package report");
    cli_println!();

    let mut any_manager = false;
    for report in &reports {
        if !report.available {
            cli_println!("{}: {}", report.manager.label(), report.state.label());
            if let Some(warning) = &report.warning {
                cli_println!("  Warning: {}", terminal_text::sanitize(warning));
            }
            continue;
        }
        any_manager = true;
        cli_println!(
            "{}: {} packages · {} · {} known allocated · {} unknown, {} partial, {} scoped footprints",
            report.manager.label(),
            report.packages.len(),
            report.state.label(),
            human(report.total_size.allocated),
            report.unknown_sizes(),
            report.partial_sizes(),
            report.limited_sizes(),
        );
        if let Some(w) = &report.warning {
            cli_println!("  Warning: {}", terminal_text::sanitize(w));
        }
        let mut sorted: Vec<_> = report.packages.iter().collect();
        sorted.sort_by(|a, b| {
            let a_size = a.size.map(|s| s.allocated).unwrap_or(0);
            let b_size = b.size.map(|s| s.allocated).unwrap_or(0);
            b_size.cmp(&a_size)
        });
        for pkg in sorted.iter().take(10) {
            let size_str = pkg
                .size
                .map(|s| {
                    format!(
                        "{}{}",
                        if pkg.coverage.is_partial() { "≥" } else { "" },
                        human(s.allocated)
                    )
                })
                .unwrap_or_else(|| String::from("?"));
            cli_println!(
                "  {:>10}  {} {} [{}]",
                size_str,
                terminal_text::sanitize(&pkg.name),
                terminal_text::sanitize(&pkg.version),
                pkg.coverage.summary(),
            );
        }
        if report.packages.len() > 10 {
            cli_println!("  … and {} more", report.packages.len() - 10);
        }
        cli_println!();
    }

    if !any_manager {
        cli_println!("No supported package managers found.");
        cli_println!();
    }

    for warning in graph.warnings() {
        cli_println!(
            "Dependency graph warning ({}): {}",
            warning.manager.label(),
            terminal_text::sanitize(&warning.message)
        );
    }

    if !project_deps.is_empty() {
        cli_println!("Project dependencies under {}", SafePath(&path));
        for dep in project_deps {
            let size_str = dep
                .deps_size
                .map(|s| {
                    format!(
                        "{}{}",
                        if dep.coverage.is_partial() { "≥" } else { "" },
                        human(s.allocated)
                    )
                })
                .unwrap_or_else(|| String::from("—"));
            cli_println!(
                "  {:>10}  {} ({}, {} deps) [{}]",
                size_str,
                SafePath(&dep.path),
                dep.manager_label,
                dep.dep_count,
                dep.coverage.summary(),
            );
        }
    }
    cli_println!("{}", project_scan.scope.summary());
    if project_scan.inaccessible > 0 {
        cli_println!(
            "Warning: {} project paths were unreadable; dependency results are incomplete.",
            project_scan.inaccessible
        );
    }

    Ok(())
}

fn thin_snapshots(path: PathBuf, bytes: u64, confirmed: bool) -> Result<()> {
    // Validate like `--space` (exists, is a directory, canonicalized) and
    // resolve the owning volume, so `tmutil` is handed a real mount instead of
    // a raw argument that could be a file, a relative path, or a symlink.
    let path = canonical_dir(path)?;
    let mount = space::report_for_path(&path)?.mount;
    if !confirmed {
        cli_println!(
            "Would run: tmutil thinlocalsnapshots {} {} 4",
            shell_quote_path(&mount),
            bytes
        );
        cli_println!(
            "Re-run with --yes to request {} of local snapshot reclamation.",
            human(bytes)
        );
        return Ok(());
    }

    let result = space::thin_local_snapshots(&mount, bytes)?;
    cli_println!(
        "Requested {} of local snapshot reclamation for {}.",
        human(result.requested_bytes),
        SafePath(&result.path)
    );
    if !result.stdout.is_empty() {
        cli_println!("{}", terminal_text::sanitize(&result.stdout));
    }
    if !result.stderr.is_empty() {
        eprintln!("{}", terminal_text::sanitize(&result.stderr));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ExternalAction {
    RevealInFinder,
    Open,
}

fn launch_external_action(app: &mut App, action: ExternalAction) {
    let Some((path, label)) = selected_action_target(app) else {
        app.status = String::from("nothing selected");
        return;
    };

    launch_external_action_at(app, action, path, label);
}

fn launch_external_action_at(app: &mut App, action: ExternalAction, path: PathBuf, label: String) {
    app.start_external_open(
        path,
        label,
        matches!(action, ExternalAction::RevealInFinder),
    );
}

fn selected_action_target(app: &App) -> Option<(PathBuf, String)> {
    match app.focus {
        Focus::Files => app
            .visible_entry(app.selected)
            .map(|entry| (entry.path.clone(), entry.name.clone())),
        Focus::Disks => app.disks.get(app.selected_disk).map(|disk| {
            let label = if disk.name.is_empty() {
                disk.mount.display().to_string()
            } else {
                disk.name.clone()
            };
            (disk.mount.clone(), label)
        }),
        Focus::Packages => {
            let real_idx = app.pkg_visible_index(app.selected_pkg)?;
            match app.pkg_view {
                app::PkgView::SystemManagers => app.flat_packages().get(real_idx).and_then(
                    |(package, manager): &(packages::Package, packages::Manager)| {
                        package.path.as_ref().map(|path| {
                            (
                                path.clone(),
                                format!("{} {}", manager.label(), package.name),
                            )
                        })
                    },
                ),
                app::PkgView::ProjectDeps => app.project_deps.get(real_idx).map(|dep| {
                    let path = dep.deps_dir.as_ref().unwrap_or(&dep.path).clone();
                    (
                        path,
                        format!("{} {}", dep.manager_label, dep.path.display()),
                    )
                }),
            }
        }
        Focus::Reclaim => app.selected_reclaim_path().map(|(name, path)| (path, name)),
    }
}

/// Restore the terminal to its normal state. Safe to call more than once:
/// both operations are idempotent and errors are intentionally ignored.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(err.into());
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/"))
}

fn run<B>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()>
where
    B: ratatui_core::backend::Backend<Error = io::Error>,
{
    let mut needs_draw = true;
    let mut shutting_down = false;

    loop {
        if shutting_down {
            if app.drain_shutdown_results() {
                needs_draw = true;
            }
            if !app.has_pending_shutdown_work() {
                return Ok(());
            }
        } else if app.drain_scan_results() {
            needs_draw = true;
        }

        if needs_draw {
            terminal.draw(|f| ui::draw(f, app))?;
            needs_draw = false;
        }

        let timeout = if shutting_down || app.has_pending_scan_work() {
            Duration::from_millis(50)
        } else {
            Duration::from_secs(1)
        };

        if event::poll(timeout)? {
            let input = event::read()?;
            if shutting_down {
                if matches!(input, Event::Resize(_, _)) {
                    needs_draw = true;
                }
                continue;
            }
            match input {
                Event::Resize(_, _) => {
                    needs_draw = true;
                }
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    // Ctrl+C cancels the active mode/modal like Esc in every state.
                    if handle_cancel_key(app, &key) {
                        needs_draw = true;
                        continue;
                    }
                    // Ignore character keys carrying CONTROL, ALT, or SUPER (keep SHIFT,
                    // which is how uppercase letters and symbols arrive).
                    if char_modifier_inhibited(&key) {
                        continue;
                    }
                    if app.active_modal() == ActiveModal::Help {
                        let handled = match key.code {
                            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                                app.close_help();
                                true
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                app.scroll_help(1);
                                true
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                app.scroll_help(-1);
                                true
                            }
                            KeyCode::PageDown => {
                                app.scroll_help(10);
                                true
                            }
                            KeyCode::PageUp => {
                                app.scroll_help(-10);
                                true
                            }
                            KeyCode::Home => {
                                app.help_scroll = 0;
                                true
                            }
                            KeyCode::End => {
                                app.help_scroll = usize::MAX;
                                true
                            }
                            _ => false,
                        };
                        if handled {
                            needs_draw = true;
                        }
                        continue;
                    }
                    if app.active_modal() == ActiveModal::Delete {
                        let handled = match key.code {
                            KeyCode::Char('y') => {
                                app.confirm_delete()?;
                                true
                            }
                            KeyCode::Char('n') | KeyCode::Esc => {
                                app.cancel_delete();
                                true
                            }
                            _ => false,
                        };
                        if handled {
                            needs_draw = true;
                        }
                        continue;
                    }
                    if app.active_modal() == ActiveModal::Uninstall {
                        let handled = match key.code {
                            KeyCode::Char('y') => {
                                app.confirm_uninstall();
                                true
                            }
                            KeyCode::Char('n') | KeyCode::Esc => {
                                app.cancel_uninstall();
                                true
                            }
                            _ => false,
                        };
                        if handled {
                            needs_draw = true;
                        }
                        continue;
                    }
                    if app.active_modal() == ActiveModal::EmptyTrash {
                        let handled = match key.code {
                            KeyCode::Char('y') => {
                                app.confirm_empty_trash();
                                true
                            }
                            KeyCode::Char('n') | KeyCode::Esc => {
                                app.cancel_empty_trash();
                                true
                            }
                            _ => false,
                        };
                        if handled {
                            needs_draw = true;
                        }
                        continue;
                    }
                    if matches!(
                        app.active_modal(),
                        ActiveModal::Package | ActiveModal::DiskInfo | ActiveModal::FileInfo
                    ) {
                        let scrolled = match key.code {
                            KeyCode::Down | KeyCode::Char('j') => {
                                app.scroll_detail(1);
                                true
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                app.scroll_detail(-1);
                                true
                            }
                            KeyCode::PageDown => {
                                app.scroll_detail(10);
                                true
                            }
                            KeyCode::PageUp => {
                                app.scroll_detail(-10);
                                true
                            }
                            KeyCode::Home => {
                                app.detail_scroll = 0;
                                true
                            }
                            KeyCode::End => {
                                app.detail_scroll = usize::MAX;
                                true
                            }
                            _ => false,
                        };
                        if scrolled {
                            needs_draw = true;
                            continue;
                        }
                    }
                    if app.active_modal() == ActiveModal::Package {
                        let handled = match key.code {
                            KeyCode::Esc | KeyCode::Char('i') | KeyCode::Char('q') => {
                                app.close_pkg_detail();
                                true
                            }
                            KeyCode::Char('x') => {
                                app.close_pkg_detail();
                                app.request_uninstall();
                                true
                            }
                            KeyCode::Char('d') => {
                                app.close_pkg_detail();
                                app.request_delete();
                                true
                            }
                            _ => false,
                        };
                        if handled {
                            needs_draw = true;
                        }
                        continue;
                    }
                    if app.active_modal() == ActiveModal::TopFiles {
                        let handled = match key.code {
                            KeyCode::Esc => {
                                app.close_top_files();
                                true
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                app.move_top_files(1);
                                true
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                app.move_top_files(-1);
                                true
                            }
                            KeyCode::PageDown => {
                                app.page_top_files(1);
                                true
                            }
                            KeyCode::PageUp => {
                                app.page_top_files(-1);
                                true
                            }
                            KeyCode::Home => {
                                app.set_top_files_selected(0);
                                true
                            }
                            KeyCode::End => {
                                app.set_top_files_selected(app.top_files_count().saturating_sub(1));
                                true
                            }
                            KeyCode::Enter | KeyCode::Char('f') => {
                                if let Some(path) = app.selected_top_file_path() {
                                    let label =
                                        terminal_text::sanitize(&path.display().to_string())
                                            .into_owned();
                                    launch_external_action_at(
                                        app,
                                        ExternalAction::RevealInFinder,
                                        path,
                                        label,
                                    );
                                }
                                true
                            }
                            KeyCode::Char('O') => {
                                if let Some(path) = app.selected_top_file_path() {
                                    let label =
                                        terminal_text::sanitize(&path.display().to_string())
                                            .into_owned();
                                    launch_external_action_at(
                                        app,
                                        ExternalAction::Open,
                                        path,
                                        label,
                                    );
                                }
                                true
                            }
                            KeyCode::Char('d') => {
                                app.request_delete_top_file();
                                true
                            }
                            _ => false,
                        };
                        if handled {
                            needs_draw = true;
                        }
                        continue;
                    }
                    if app.active_modal() == ActiveModal::ReclaimPaths {
                        let handled = match key.code {
                            KeyCode::Esc => {
                                app.close_reclaim_paths();
                                true
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                app.move_reclaim_paths(1);
                                true
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                app.move_reclaim_paths(-1);
                                true
                            }
                            KeyCode::PageDown => {
                                app.page_reclaim_paths(1);
                                true
                            }
                            KeyCode::PageUp => {
                                app.page_reclaim_paths(-1);
                                true
                            }
                            KeyCode::Home => {
                                app.set_reclaim_paths_selected(0);
                                true
                            }
                            KeyCode::End => {
                                app.set_reclaim_paths_selected(
                                    app.reclaim_paths_count().saturating_sub(1),
                                );
                                true
                            }
                            KeyCode::Enter | KeyCode::Char('f') => {
                                if let Some((name, path)) = app.selected_reclaim_path() {
                                    launch_external_action_at(
                                        app,
                                        ExternalAction::RevealInFinder,
                                        path,
                                        terminal_text::sanitize(&name).into_owned(),
                                    );
                                }
                                true
                            }
                            KeyCode::Char('O') => {
                                if let Some((name, path)) = app.selected_reclaim_path() {
                                    launch_external_action_at(
                                        app,
                                        ExternalAction::Open,
                                        path,
                                        terminal_text::sanitize(&name).into_owned(),
                                    );
                                }
                                true
                            }
                            KeyCode::Char('d') => {
                                app.request_delete_reclaim_path();
                                true
                            }
                            _ => false,
                        };
                        if handled {
                            needs_draw = true;
                        }
                        continue;
                    }
                    if app.active_modal() == ActiveModal::DiskInfo {
                        let handled = match key.code {
                            KeyCode::Esc => {
                                app.close_disk_info();
                                true
                            }
                            KeyCode::Char('s') => {
                                app.retry_disk_snapshots();
                                true
                            }
                            KeyCode::Char('a') => {
                                app.retry_disk_apfs();
                                true
                            }
                            _ => false,
                        };
                        if handled {
                            needs_draw = true;
                        }
                        continue;
                    }
                    if app.active_modal() == ActiveModal::FileInfo {
                        let handled = match key.code {
                            KeyCode::Esc | KeyCode::Char('i') | KeyCode::Char('q') => {
                                app.close_file_info();
                                true
                            }
                            KeyCode::Char('f') => {
                                if let Some((path, label)) = app.file_info_action_target() {
                                    app.close_file_info();
                                    launch_external_action_at(
                                        app,
                                        ExternalAction::RevealInFinder,
                                        path,
                                        label,
                                    );
                                }
                                true
                            }
                            KeyCode::Char('O') => {
                                if let Some((path, label)) = app.file_info_action_target() {
                                    app.close_file_info();
                                    launch_external_action_at(
                                        app,
                                        ExternalAction::Open,
                                        path,
                                        label,
                                    );
                                }
                                true
                            }
                            KeyCode::Char('d') => {
                                app.request_delete_file_info();
                                true
                            }
                            _ => false,
                        };
                        if handled {
                            needs_draw = true;
                        }
                        continue;
                    }
                    if app.active_modal() == ActiveModal::Input {
                        let handled = match key.code {
                            KeyCode::Esc => {
                                app.exit_input_mode();
                                true
                            }
                            KeyCode::Enter => {
                                app.input_commit()?;
                                true
                            }
                            KeyCode::Backspace => {
                                app.input_pop();
                                true
                            }
                            KeyCode::Char(ch) => {
                                app.input_push(ch);
                                true
                            }
                            _ => false,
                        };
                        if handled {
                            needs_draw = true;
                        }
                        continue;
                    }
                    if app.search_mode {
                        let handled = match key.code {
                            KeyCode::Esc => {
                                app.clear_search();
                                true
                            }
                            KeyCode::Enter => {
                                app.keep_search();
                                true
                            }
                            KeyCode::Backspace => {
                                app.search_pop();
                                true
                            }
                            KeyCode::Down => {
                                app.move_cursor(1);
                                true
                            }
                            KeyCode::Up => {
                                app.move_cursor(-1);
                                true
                            }
                            KeyCode::PageDown => {
                                app.page_move(1);
                                true
                            }
                            KeyCode::PageUp => {
                                app.page_move(-1);
                                true
                            }
                            KeyCode::Home => {
                                app.move_to_start();
                                true
                            }
                            KeyCode::End => {
                                app.move_to_end();
                                true
                            }
                            KeyCode::Char(ch) => {
                                app.search_push(ch);
                                true
                            }
                            _ => false,
                        };
                        if handled {
                            needs_draw = true;
                        }
                        continue;
                    }
                    if app.pkg_search_mode {
                        if handle_pkg_search_key(app, key.code) {
                            needs_draw = true;
                        }
                        continue;
                    }
                    let handled = match key.code {
                        KeyCode::Char('?') => {
                            app.open_help();
                            true
                        }
                        KeyCode::Char('q') => {
                            if app.has_pending_shutdown_work() {
                                app.begin_shutdown();
                                shutting_down = true;
                                true
                            } else {
                                return Ok(());
                            }
                        }
                        KeyCode::Esc => {
                            if app.focus != Focus::Files {
                                app.focus = Focus::Files;
                                true
                            } else {
                                false
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            app.move_cursor(1);
                            true
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            app.move_cursor(-1);
                            true
                        }
                        KeyCode::PageDown => {
                            app.page_move(1);
                            true
                        }
                        KeyCode::PageUp => {
                            app.page_move(-1);
                            true
                        }
                        KeyCode::Home => {
                            app.move_to_start();
                            true
                        }
                        KeyCode::End => {
                            app.move_to_end();
                            true
                        }
                        KeyCode::Enter => {
                            if app.focus == Focus::Packages && app.packages_loaded {
                                app.open_pkg_detail();
                                true
                            } else if app.focus == Focus::Reclaim {
                                app.open_selected_reclaim_paths();
                                true
                            } else {
                                app.enter()?;
                                true
                            }
                        }
                        KeyCode::Backspace => {
                            app.go_up()?;
                            true
                        }
                        KeyCode::Char('f') => {
                            launch_external_action(app, ExternalAction::RevealInFinder);
                            true
                        }
                        KeyCode::Char('O') => {
                            launch_external_action(app, ExternalAction::Open);
                            true
                        }
                        KeyCode::Char('y') => {
                            app.copy_path_to_clipboard();
                            true
                        }
                        KeyCode::Char('s') => {
                            if let Err(err) = app.open_shell() {
                                app.status = format!("open shell failed: {err}");
                            }
                            true
                        }
                        KeyCode::Char('r') => {
                            if app.focus == Focus::Packages {
                                app.refresh_packages();
                            } else if app.focus == Focus::Reclaim {
                                app.request_reclaim_scan();
                            } else {
                                app.force_rescan();
                            }
                            true
                        }
                        KeyCode::Char('S') if keymap::is_available_in_focus(app.focus, "S") => {
                            app.scan_all_missing_visible();
                            true
                        }
                        KeyCode::Char('d') => {
                            app.request_delete();
                            true
                        }
                        KeyCode::Char('c') => {
                            if app.focus == Focus::Files {
                                app.request_rename();
                                true
                            } else {
                                false
                            }
                        }
                        KeyCode::Char('n') => {
                            if app.focus == Focus::Files {
                                app.request_mkdir();
                                true
                            } else {
                                false
                            }
                        }
                        KeyCode::Char('v') => {
                            if app.focus == Focus::Files {
                                app.toggle_mark();
                                true
                            } else {
                                false
                            }
                        }
                        KeyCode::Char('a') => {
                            if app.focus == Focus::Files {
                                app.mark_all_visible();
                                true
                            } else {
                                false
                            }
                        }
                        KeyCode::Char('E') => {
                            if app.focus == Focus::Reclaim {
                                app.request_empty_trash();
                                true
                            } else {
                                false
                            }
                        }
                        KeyCode::Char('R') if keymap::is_available_in_focus(app.focus, "R") => {
                            app.request_reclaim_scan();
                            true
                        }
                        KeyCode::Char('o') if keymap::is_available_in_focus(app.focus, "o") => {
                            app.cycle_sort();
                            true
                        }
                        KeyCode::Char('t') => {
                            if app.focus == Focus::Files {
                                let scan_target = app
                                    .visible_entry(app.selected)
                                    .filter(|entry| entry.is_dir)
                                    .map(|entry| entry.path.clone())
                                    .unwrap_or_else(|| app.cwd.clone());
                                app.open_top_files_for_path(scan_target);
                                true
                            } else {
                                false
                            }
                        }
                        KeyCode::Char('p') => {
                            open_packages(app);
                            true
                        }
                        KeyCode::Char('/') if app.focus == Focus::Files => {
                            app.enter_search();
                            true
                        }
                        KeyCode::Char('/')
                            if app.focus == Focus::Packages && app.packages_loaded =>
                        {
                            app.enter_pkg_search();
                            true
                        }
                        KeyCode::Char('i') if app.focus == Focus::Files => {
                            app.open_file_info();
                            true
                        }
                        KeyCode::Char('i')
                            if app.focus == Focus::Packages && app.packages_loaded =>
                        {
                            app.open_pkg_detail();
                            true
                        }
                        KeyCode::Char('i') if app.focus == Focus::Disks => {
                            app.request_disk_info_for_selected_disk();
                            true
                        }
                        KeyCode::Char('B') if keymap::is_available_in_focus(app.focus, "B") => {
                            app.save_history_baseline();
                            true
                        }
                        KeyCode::Char('u')
                            if app.focus == Focus::Packages && app.packages_loaded =>
                        {
                            app.toggle_unused_filter();
                            true
                        }
                        KeyCode::Char('x')
                            if app.focus == Focus::Packages && app.packages_loaded =>
                        {
                            app.request_uninstall();
                            true
                        }
                        KeyCode::Left | KeyCode::Char('h') => {
                            if app.focus == Focus::Packages
                                && app.pkg_view == app::PkgView::ProjectDeps
                            {
                                app.toggle_pkg_view();
                                true
                            } else {
                                focus_previous(app);
                                true
                            }
                        }
                        KeyCode::Right | KeyCode::Char('l') => {
                            if app.focus == Focus::Packages
                                && app.pkg_view == app::PkgView::SystemManagers
                            {
                                app.toggle_pkg_view();
                                true
                            } else {
                                focus_next(app);
                                true
                            }
                        }
                        KeyCode::Char('.') if keymap::is_available_in_focus(app.focus, ".") => {
                            app.toggle_hidden()?;
                            true
                        }
                        KeyCode::BackTab => {
                            focus_previous(app);
                            true
                        }
                        KeyCode::Tab => {
                            focus_next(app);
                            true
                        }
                        _ => false,
                    };
                    if handled {
                        needs_draw = true;
                    }
                }
                _ => {}
            }
        } else if app.has_pending_scan_work() {
            needs_draw = true;
        }
    }
}

fn focus_next(app: &mut App) {
    let next = match app.focus {
        Focus::Files => Focus::Disks,
        Focus::Disks => Focus::Packages,
        Focus::Packages => Focus::Reclaim,
        Focus::Reclaim => Focus::Files,
    };
    set_focus(app, next);
}

fn open_packages(app: &mut App) {
    let switch_view = app.focus == Focus::Packages;
    set_focus(app, Focus::Packages);
    if switch_view {
        app.toggle_pkg_view();
    }
    app.load_packages();
}

fn handle_cancel_key(app: &mut App, key: &crossterm::event::KeyEvent) -> bool {
    if key.code == KeyCode::Esc
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        cancel_active_state(app);
        true
    } else {
        false
    }
}

fn focus_previous(app: &mut App) {
    let previous = match app.focus {
        Focus::Files => Focus::Reclaim,
        Focus::Disks => Focus::Files,
        Focus::Packages => Focus::Disks,
        Focus::Reclaim => Focus::Packages,
    };
    set_focus(app, previous);
}

fn set_focus(app: &mut App, focus: Focus) {
    if app.focus != focus {
        app.status.clear();
    }
    app.focus = focus;
    if app.focus == Focus::Reclaim {
        app.open_reclaim_for_focus();
    }
}

// Returns true if a key event is a Char key modified by CONTROL, ALT, or SUPER.
// Such events must be dropped before plain-character match arms are reached.
// SHIFT is allowed: it is how uppercase letters and symbols (e.g. `?`, `!`) arrive.
fn char_modifier_inhibited(key: &crossterm::event::KeyEvent) -> bool {
    const INHIBIT: KeyModifiers = KeyModifiers::CONTROL
        .union(KeyModifiers::ALT)
        .union(KeyModifiers::SUPER);
    matches!(key.code, KeyCode::Char(_)) && key.modifiers.intersects(INHIBIT)
}

// Rendering and keyboard dispatch use the same modal priority.
fn cancel_active_state(app: &mut App) {
    match app.active_modal() {
        ActiveModal::Help => app.close_help(),
        ActiveModal::Delete => app.cancel_delete(),
        ActiveModal::Uninstall => app.cancel_uninstall(),
        ActiveModal::EmptyTrash => app.cancel_empty_trash(),
        ActiveModal::Package => app.close_pkg_detail(),
        ActiveModal::TopFiles => app.close_top_files(),
        ActiveModal::ReclaimPaths => app.close_reclaim_paths(),
        ActiveModal::DiskInfo => app.close_disk_info(),
        ActiveModal::FileInfo => app.close_file_info(),
        ActiveModal::Input => app.exit_input_mode(),
        ActiveModal::None => {
            if app.focus == Focus::Packages && (app.pkg_search_mode || app.pkg_filter_active()) {
                app.clear_pkg_search();
            } else if app.search_mode || app.search_filter_active() {
                app.clear_search();
            } else if app.pkg_search_mode || app.pkg_filter_active() {
                app.clear_pkg_search();
            } else if app.focus != Focus::Files {
                app.focus = Focus::Files;
            }
        }
    }
}

fn handle_pkg_search_key(app: &mut App, code: KeyCode) -> bool {
    match code {
        KeyCode::Esc => {
            app.clear_pkg_search();
            true
        }
        KeyCode::Enter => {
            app.keep_pkg_search();
            true
        }
        KeyCode::Backspace => {
            app.pkg_search_pop();
            true
        }
        KeyCode::Down => {
            app.move_cursor(1);
            true
        }
        KeyCode::Up => {
            app.move_cursor(-1);
            true
        }
        KeyCode::PageDown => {
            app.page_move(1);
            true
        }
        KeyCode::PageUp => {
            app.page_move(-1);
            true
        }
        KeyCode::Home => {
            app.move_to_start();
            true
        }
        KeyCode::End => {
            app.move_to_end();
            true
        }
        KeyCode::Char(ch) => {
            app.pkg_search_push(ch);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_key_clears_the_active_kept_filter() {
        let root = test_root("escape_kept_filters");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a"), b"x").unwrap();
        let mut app = App::new(root.clone()).unwrap();
        app.enter_search();
        app.search_push('a');
        app.keep_search();
        app.focus = Focus::Packages;
        app.enter_pkg_search();
        app.pkg_search_push('a');
        app.keep_pkg_search();
        let escape = crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(handle_cancel_key(&mut app, &escape));
        assert!(!app.pkg_filter_active());
        assert!(app.search_filter_active());
        app.focus = Focus::Files;
        assert!(handle_cancel_key(&mut app, &escape));
        assert!(!app.search_filter_active());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn package_shortcut_switches_view_only_after_focusing_packages() {
        let root = test_root("package_shortcut_views");
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(root.clone()).unwrap();
        app.packages_loaded = true;
        open_packages(&mut app);
        assert_eq!(app.focus, Focus::Packages);
        assert!(app.pkg_view == app::PkgView::SystemManagers);
        open_packages(&mut app);
        assert!(app.pkg_view == app::PkgView::ProjectDeps);
        open_packages(&mut app);
        assert!(app.pkg_view == app::PkgView::SystemManagers);
        std::fs::remove_dir_all(root).unwrap();
    }
    use std::fs;

    fn parse(parts: &[&str]) -> Result<CliAction> {
        parse_args(parts.iter().map(OsString::from))
    }

    #[test]
    fn change_status_uses_logical_delta_when_allocated_is_unchanged() {
        let shrank = history::ChildChange {
            name: String::from("logical-only"),
            before: Some(bulkstat::SizeInfo::new(200, 100)),
            after: Some(bulkstat::SizeInfo::new(120, 100)),
            before_inaccessible: 0,
            after_inaccessible: 0,
            before_skipped_mounts: 0,
            after_skipped_mounts: 0,
            before_is_dir: Some(true),
            after_is_dir: Some(true),
        };
        let grew = history::ChildChange {
            name: String::from("logical-only"),
            before: Some(bulkstat::SizeInfo::new(120, 100)),
            after: Some(bulkstat::SizeInfo::new(200, 100)),
            before_inaccessible: 0,
            after_inaccessible: 0,
            before_skipped_mounts: 0,
            after_skipped_mounts: 0,
            before_is_dir: Some(true),
            after_is_dir: Some(true),
        };

        assert_eq!(change_status(&shrank), "shrank");
        assert_eq!(change_status(&grew), "grew");
    }

    #[test]
    fn focus_transit_to_packages_does_not_start_package_scan() {
        let root = test_root("focus_packages_no_scan");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        set_focus(&mut app, Focus::Packages);

        assert!(matches!(app.focus, Focus::Packages));
        assert!(!app.packages_loading);
        assert!(!app.packages_loaded);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn defaults_to_home_without_args() {
        assert!(matches!(parse(&[]).unwrap(), CliAction::Run(_)));
    }

    #[test]
    fn accepts_one_path() {
        let action = parse(&["/tmp"]).unwrap();
        assert!(matches!(action, CliAction::Run(path) if path == std::path::Path::new("/tmp")));
    }

    #[test]
    fn accepts_dash_prefixed_path_after_separator() {
        let action = parse(&["--", "-cache"]).unwrap();
        assert!(matches!(action, CliAction::Run(path) if path == std::path::Path::new("-cache")));
    }

    #[test]
    fn suggested_shell_paths_round_trip_in_zsh() {
        use std::os::unix::ffi::OsStringExt;
        use std::process::Command;

        for bytes in [
            b"/tmp/space and 'apostrophe'".to_vec(),
            "/tmp/résumé".as_bytes().to_vec(),
            b"/tmp/control\nline".to_vec(),
            b"/tmp/raw-\xff".to_vec(),
        ] {
            let path = PathBuf::from(OsString::from_vec(bytes.clone()));
            let expression = shell_quote_path(&path);
            let output = Command::new("zsh")
                .arg("-c")
                .arg(format!("printf %s {expression}"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "zsh rejected {}: {}",
                expression,
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, bytes);
        }
    }

    #[test]
    fn rejects_unknown_option_before_separator() {
        let error = parse(&["--tops"]).err().unwrap();
        assert!(error.to_string().contains("unknown option --tops"));
        assert!(error.to_string().contains("use --"));
    }

    #[test]
    fn parses_top_report() {
        let action = parse(&["--top", "20", "--json", "/tmp"]).unwrap();
        assert!(matches!(
            action,
            CliAction::Top {
                path,
                limit: 20,
                json: true
            } if path == std::path::Path::new("/tmp")
        ));
    }

    #[test]
    fn parses_top_equals_report_with_default_path() {
        let action = parse(&["--top=5"]).unwrap();
        assert!(matches!(
            action,
            CliAction::Top {
                limit: 5,
                json: false,
                ..
            }
        ));
    }

    #[test]
    fn parses_save_baseline() {
        let action = parse(&["--save", "/tmp"]).unwrap();
        assert!(matches!(
            action,
            CliAction::Save { path, json: false } if path == std::path::Path::new("/tmp")
        ));
    }

    #[test]
    fn parses_diff_report() {
        let action = parse(&["--diff", "--json", "/tmp"]).unwrap();
        assert!(matches!(
            action,
            CliAction::Diff { path, json: true } if path == std::path::Path::new("/tmp")
        ));
    }

    #[test]
    fn parses_space_report() {
        let action = parse(&["--space", "--json", "/tmp"]).unwrap();
        assert!(matches!(
            action,
            CliAction::Space {
                path,
                json: true
            } if path == std::path::Path::new("/tmp")
        ));
    }

    #[test]
    fn parses_packages_report() {
        let action = parse(&["--packages", "--json", "/tmp"]).unwrap();
        assert!(matches!(
            action,
            CliAction::Packages {
                path,
                json: true
            } if path == std::path::Path::new("/tmp")
        ));
    }

    #[test]
    fn parses_reclaim_report() {
        let action = parse(&["--reclaim", "--json", "/tmp"]).unwrap();
        assert!(matches!(
            action,
            CliAction::Reclaim {
                path,
                json: true
            } if path == std::path::Path::new("/tmp")
        ));
    }

    #[test]
    fn parses_snapshot_thin_dry_run() {
        let action = parse(&["--thin-snapshots", "1.5G", "/tmp"]).unwrap();
        assert!(matches!(
            action,
            CliAction::ThinSnapshots {
                path,
                bytes: 1_610_612_736,
                confirmed: false
            } if path == std::path::Path::new("/tmp")
        ));
    }

    #[test]
    fn parses_snapshot_thin_confirmation() {
        let action = parse(&["--thin-snapshots=2G", "--yes"]).unwrap();
        assert!(matches!(
            action,
            CliAction::ThinSnapshots {
                bytes: 2_147_483_648,
                confirmed: true,
                ..
            }
        ));
    }

    #[test]
    fn top_rejects_zero_limit() {
        let err = parse(&["--top", "0"]).err().unwrap();
        assert!(err.to_string().contains("--top must be greater than zero"));
    }

    #[test]
    fn json_requires_top_report() {
        let err = parse(&["--json"]).err().unwrap();
        assert!(err
            .to_string()
            .contains("--json requires --top, --reclaim, --save, --diff, --space, or --packages"));
    }

    #[test]
    fn yes_requires_snapshot_thinning() {
        let err = parse(&["--yes"]).err().unwrap();
        assert!(err.to_string().contains("--yes requires --thin-snapshots"));
    }

    #[test]
    fn report_modes_are_mutually_exclusive() {
        let err = parse(&["--top", "5", "--space"]).err().unwrap();
        assert!(err.to_string().contains(
            "choose only one of --top, --reclaim, --save, --diff, --space, --packages, or --thin-snapshots"
        ));
    }

    #[test]
    fn canonical_dir_resolves_dotted_paths() {
        let root = test_root("canonical_dir");
        fs::create_dir_all(&root).unwrap();

        let resolved = canonical_dir(root.join(".")).unwrap();

        assert_eq!(resolved, root.canonicalize().unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reverse_focus_from_files_goes_to_reclaim() {
        let root = test_root("reverse_focus");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.focus = Focus::Files;
        focus_previous(&mut app);

        assert!(app.focus == Focus::Reclaim);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn external_action_target_uses_selected_file_disk_or_package() {
        let root = test_root("external_target");
        let file = root.join("visible.txt");
        fs::create_dir_all(&root).unwrap();
        fs::write(&file, b"visible").unwrap();

        let mut app = App::new(root.clone()).unwrap();
        let target = selected_action_target(&app).unwrap();
        assert_eq!(target.0, file);
        assert_eq!(target.1, "visible.txt");

        let mount = root.join("mount");
        fs::create_dir_all(&mount).unwrap();
        app.disks = vec![app::DiskInfo {
            name: String::from("External"),
            mount: mount.clone(),
            total: 100,
            available: 50,
        }];
        app.focus = Focus::Disks;

        let target = selected_action_target(&app).unwrap();
        assert_eq!(target.0, mount);
        assert_eq!(target.1, "External");

        let pkg_path = root.join("pkg-bin");
        fs::write(&pkg_path, b"binary").unwrap();
        app.pkg_reports = vec![packages::ManagerReport {
            manager: packages::Manager::Cargo,
            packages: vec![packages::Package {
                name: String::from("diskr"),
                version: String::from("0.1.5"),
                size: None,
                path: Some(pkg_path.clone()),
                metadata_path: None,
                coverage: Default::default(),
            }],
            total_size: crate::bulkstat::SizeInfo::default(),
            available: true,
            warning: None,
            state: packages::ManagerScanState::Complete,
            environment: None,
        }];
        app.project_deps = vec![packages::ProjectDeps {
            path: root.clone(),
            manager_label: String::from("cargo"),
            manifest: String::from("Cargo.toml"),
            dep_count: 3,
            deps_size: None,
            deps_dir: Some(root.join("target")),
            coverage: Default::default(),
        }];
        app.packages_loaded = true;
        app.rebuild_flat_packages();
        app.focus = Focus::Packages;
        app.pkg_view = app::PkgView::SystemManagers;
        app.selected_pkg = 0;

        let target = selected_action_target(&app).unwrap();
        assert_eq!(target.0, pkg_path);
        assert_eq!(target.1, "cargo diskr");

        app.pkg_view = app::PkgView::ProjectDeps;
        let target = selected_action_target(&app).unwrap();
        assert_eq!(target.0, root.join("target"));
        assert_eq!(target.1, format!("cargo {}", root.display()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn separator_requires_path() {
        let err = parse(&["--"]).err().unwrap();
        assert!(err.to_string().contains("usage: diskr [PATH]"));
    }

    #[test]
    fn parses_help_and_version_flags() {
        assert!(matches!(parse(&["--help"]).unwrap(), CliAction::Help));
        assert!(matches!(parse(&["-h"]).unwrap(), CliAction::Help));
        assert!(matches!(parse(&["--version"]).unwrap(), CliAction::Version));
        assert!(matches!(parse(&["-V"]).unwrap(), CliAction::Version));
    }

    #[test]
    fn rejects_extra_args() {
        assert!(parse(&["/tmp", "/var"]).is_err());
        assert!(parse(&["--", "/tmp", "/var"]).is_err());
    }

    #[test]
    fn canonical_dir_resolves_relative_and_dot_paths() {
        let root = test_root("canonical_relative");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();

        // A `.`-suffixed path resolves to its absolute canonical form, so the
        // TUI start path and report roots are never left relative (which broke
        // Backspace navigation and HOME-relative reclaim categories).
        let resolved = canonical_dir(nested.join(".")).unwrap();
        assert!(resolved.is_absolute());
        assert_eq!(resolved, nested.canonicalize().unwrap());

        // Nonexistent paths and files are rejected.
        assert!(canonical_dir(root.join("missing")).is_err());
        let file = root.join("file.txt");
        fs::write(&file, b"x").unwrap();
        assert!(canonical_dir(file).is_err());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn packages_report_rejects_nonexistent_path() {
        // print_packages validates before scanning, so a bad root errors
        // instead of exiting 0 with only global package info.
        let missing = test_root("packages_missing");
        assert!(print_packages(missing, false).is_err());
    }

    #[test]
    fn thin_snapshots_rejects_nonexistent_and_file_paths() {
        let root = test_root("thin_validate");
        fs::create_dir_all(&root).unwrap();

        // Both error inside canonical_dir, before tmutil is ever invoked.
        assert!(thin_snapshots(root.join("missing"), 1024, false).is_err());
        let file = root.join("snap.txt");
        fs::write(&file, b"x").unwrap();
        assert!(thin_snapshots(file, 1024, false).is_err());

        fs::remove_dir_all(root).unwrap();
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "diskr_main_{name}_{}_{}",
            std::process::id(),
            nanos
        ))
    }

    // ---- modifier guard tests ----

    fn key_char(ch: char, mods: KeyModifiers) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(KeyCode::Char(ch), mods)
    }

    fn key_nonchar(code: KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn package_search_treats_j_and_k_as_query_text() {
        let root = test_root("pkg_search_jk");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.focus = Focus::Packages;
        app.packages_loaded = true;
        app.pkg_reports = vec![packages::ManagerReport {
            manager: packages::Manager::Brew,
            packages: vec![
                packages::Package {
                    name: String::from("jq"),
                    version: String::from("1.0"),
                    size: Some(crate::bulkstat::SizeInfo::new(30, 30)),
                    path: None,
                    metadata_path: None,
                    coverage: Default::default(),
                },
                packages::Package {
                    name: String::from("kubectl"),
                    version: String::from("1.0"),
                    size: Some(crate::bulkstat::SizeInfo::new(20, 20)),
                    path: None,
                    metadata_path: None,
                    coverage: Default::default(),
                },
                packages::Package {
                    name: String::from("alpha"),
                    version: String::from("1.0"),
                    size: Some(crate::bulkstat::SizeInfo::new(10, 10)),
                    path: None,
                    metadata_path: None,
                    coverage: Default::default(),
                },
            ],
            total_size: crate::bulkstat::SizeInfo::new(60, 60),
            available: true,
            warning: None,
            state: packages::ManagerScanState::Complete,
            environment: None,
        }];
        app.rebuild_flat_packages();

        app.selected_pkg = 0;
        app.enter_pkg_search();
        assert!(handle_pkg_search_key(&mut app, KeyCode::Down));
        assert_eq!(app.selected_pkg, 1);
        assert!(app.pkg_search_query.is_empty());

        assert!(handle_pkg_search_key(&mut app, KeyCode::Char('j')));
        assert_eq!(app.pkg_search_query, "j");
        assert_eq!(app.pkg_visible_indices(), &[0]);

        app.clear_pkg_search();
        app.enter_pkg_search();
        assert!(handle_pkg_search_key(&mut app, KeyCode::Char('k')));
        assert_eq!(app.pkg_search_query, "k");
        assert_eq!(app.pkg_visible_indices(), &[1]);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn char_with_control_is_inhibited() {
        assert!(char_modifier_inhibited(&key_char(
            'c',
            KeyModifiers::CONTROL
        )));
        assert!(char_modifier_inhibited(&key_char(
            'd',
            KeyModifiers::CONTROL
        )));
        assert!(char_modifier_inhibited(&key_char(
            'r',
            KeyModifiers::CONTROL
        )));
        assert!(char_modifier_inhibited(&key_char(
            'v',
            KeyModifiers::CONTROL
        )));
    }

    #[test]
    fn char_with_alt_or_super_is_inhibited() {
        assert!(char_modifier_inhibited(&key_char('c', KeyModifiers::ALT)));
        assert!(char_modifier_inhibited(&key_char('x', KeyModifiers::SUPER)));
    }

    #[test]
    fn plain_char_and_shift_char_are_not_inhibited() {
        assert!(!char_modifier_inhibited(&key_char('c', KeyModifiers::NONE)));
        assert!(!char_modifier_inhibited(&key_char(
            'S',
            KeyModifiers::SHIFT
        )));
        assert!(!char_modifier_inhibited(&key_char(
            'O',
            KeyModifiers::SHIFT
        )));
        assert!(!char_modifier_inhibited(&key_char(
            '?',
            KeyModifiers::SHIFT
        )));
    }

    #[test]
    fn non_char_keys_with_control_are_not_inhibited() {
        // Only Char(_) keys are blocked; Esc/Enter with modifiers pass through.
        let esc = crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::CONTROL);
        assert!(!char_modifier_inhibited(&esc));
        let _ = key_nonchar(KeyCode::Enter); // satisfies unused-variable lint
    }

    #[test]
    fn ctrl_c_exits_input_mode_via_cancel_active_state() {
        let root = test_root("ctrl_c_input_mode");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("file.txt"), b"x").unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.request_rename();
        assert!(matches!(app.input_mode, app::InputMode::Rename));

        cancel_active_state(&mut app);
        assert!(matches!(app.input_mode, app::InputMode::None));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ctrl_c_cancels_delete_confirmation() {
        let root = test_root("ctrl_c_delete_confirm");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("file.txt"), b"x").unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.request_delete();
        assert!(app.confirming_delete);

        cancel_active_state(&mut app);
        assert!(!app.confirming_delete);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ctrl_c_closes_help_overlay() {
        let root = test_root("ctrl_c_help_overlay");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.open_help();
        assert!(app.show_help);

        cancel_active_state(&mut app);
        assert!(!app.show_help);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ctrl_c_cancels_empty_trash_confirmation() {
        let root = test_root("ctrl_c_empty_trash_confirm");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        // E only arms when the loaded reclaim report lists Trash.
        app.reclaim_report = Some(app::trash_only_report(&root, 1024));
        app.request_empty_trash();
        assert!(app.confirming_empty_trash);

        cancel_active_state(&mut app);
        assert!(!app.confirming_empty_trash);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ctrl_c_in_search_mode_exits_search() {
        let root = test_root("ctrl_c_search");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.enter_search();
        assert!(app.search_mode);

        cancel_active_state(&mut app);
        assert!(!app.search_mode);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ctrl_c_clears_kept_search_filter() {
        let root = test_root("ctrl_c_kept_search");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("alpha.txt"), b"x").unwrap();
        fs::write(root.join("beta.txt"), b"x").unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.enter_search();
        app.search_push('b');
        app.keep_search();
        assert!(app.search_filter_active());

        cancel_active_state(&mut app);

        assert!(!app.search_mode);
        assert!(!app.search_filter_active());
        assert_eq!(app.visible_entry_count(), 2);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ctrl_c_in_base_files_state_is_noop() {
        let root = test_root("ctrl_c_base_state");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        assert!(matches!(app.focus, Focus::Files));

        // Should not panic or change meaningful state.
        cancel_active_state(&mut app);
        assert!(matches!(app.focus, Focus::Files));
        assert!(matches!(app.input_mode, app::InputMode::None));
        assert!(!app.confirming_delete);

        fs::remove_dir_all(root).unwrap();
    }
}
