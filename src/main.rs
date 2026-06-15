mod app;
mod bulkstat;
mod fs_ops;
mod history;
mod keymap;
mod packages;
mod pool;
mod reclaim;
mod scanner;
mod space;
mod state;
mod terminal_backend;
mod ui;

use anyhow::{bail, Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui_core::terminal::Terminal;
use std::{
    ffi::OsString,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use app::{format_elapsed, human, App, Focus};
use terminal_backend::CrosstermBackend;

fn main() -> Result<()> {
    match parse_args(std::env::args_os().skip(1))? {
        CliAction::Help => {
            print_help();
            Ok(())
        }
        CliAction::Version => {
            println!("diskr {}", env!("CARGO_PKG_VERSION"));
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
    }
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

fn print_help() {
    print!(
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
    print_key_help();
}

fn print_key_help() {
    for section in keymap::HELP_SECTIONS {
        println!("\n{}:", section.title);
        for binding in section.bindings {
            println!("  {:<16} {}", binding.key, binding.action);
        }
    }
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
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!(
        "Top {} files by allocated size under {}",
        limit,
        path.display()
    );
    println!(
        "Total: {} disk / {} apparent",
        human(scan.size.allocated),
        human(scan.size.logical)
    );
    if scan.inaccessible > 0 {
        println!(
            "Warning: {} directories were unreadable; totals are lower bounds.",
            scan.inaccessible
        );
    }
    if scan.skipped_mounts > 0 {
        println!(
            "Note: skipped {} mounted volumes on other devices.",
            scan.skipped_mounts
        );
    }
    if scan.largest_files.is_empty() {
        println!("No regular files found.");
        return Ok(());
    }
    for (index, file) in scan.largest_files.iter().enumerate() {
        println!(
            "{:>2}. {:>22}  {}",
            index + 1,
            top_size_label(file.size),
            file.path.display()
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
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    println!("Reclaimable space under {}", report.root.display());
    println!(
        "Total: {} disk / {} apparent",
        human(report.total.allocated),
        human(report.total.logical)
    );
    if report.inaccessible > 0 {
        println!(
            "Warning: {} directories were unreadable; totals are lower bounds.",
            report.inaccessible
        );
    }
    if report.skipped_mounts > 0 {
        println!(
            "Note: skipped {} mounted volumes on other devices.",
            report.skipped_mounts
        );
    }
    if report.findings.is_empty() {
        println!("No known reclaimable caches or build artifacts found.");
        return Ok(());
    }
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
        println!(
            "{:>22}  [{:^11}]  {}{}{}",
            reclaim_size_label(finding),
            finding.class.label(),
            finding.label,
            count,
            rollup_suffix
        );
        println!("                          {}", finding.note);
    }
    println!("\nThis is a report only; diskr does not delete anything here.");
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
            "children": children,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    println!("Saved baseline for {}", record.path.display());
    println!(
        "Total: {} disk / {} apparent across {} entries",
        human(total.allocated),
        human(total.logical),
        record.children.len()
    );
    println!(
        "Compare later with `diskr --diff {}`.",
        record.path.display()
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
            "total_delta_logical": report.total_delta_logical().to_string(),
            "total_delta_allocated": report.total_delta_allocated().to_string(),
            "changes": changes,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    println!("Diff for {}", report.path.display());
    println!(
        "Baseline captured {}",
        format_elapsed(
            report
                .current_timestamp
                .saturating_sub(report.baseline_timestamp)
        )
    );
    println!(
        "Total: {} disk → {} disk  ({})",
        human(report.before_total.allocated),
        human(report.after_total.allocated),
        format_signed_bytes(report.total_delta_allocated())
    );
    if report.changes.is_empty() {
        println!("No changes since the baseline.");
        return Ok(());
    }
    for change in &report.changes {
        println!(
            "{:>12}  {:<9}  {}",
            format_signed_bytes(change.delta_allocated()),
            change_status(change),
            change.name
        );
    }
    Ok(())
}

fn change_status(change: &history::ChildChange) -> &'static str {
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
            "local_snapshots": snapshots,
            "local_snapshot_error": report.local_snapshots.error,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    println!("Space report for {}", report.path.display());
    println!(
        "Mount: {} ({}, {})",
        report.mount.display(),
        report.fs_type,
        report.device
    );
    println!("Total: {}", human(report.total));
    println!("Used: {}", human(report.used));
    println!("Free blocks: {}", human(report.free));
    println!("Available to user: {}", human(report.available));
    let unavailable = report.unavailable_free();
    if unavailable > 0 {
        println!("Free but not user-available: {}", human(unavailable));
    }
    if let Some(container) = &report.apfs_container {
        println!(
            "APFS container {}: {} total / {} free",
            container.reference,
            human(container.size),
            human(container.free)
        );
    }
    match &report.local_snapshots.error {
        Some(error) if !error.is_empty() => {
            println!("Local snapshots: unavailable ({error})");
        }
        _ if report.local_snapshots.names.is_empty() => {
            println!("Local snapshots: none reported by tmutil");
        }
        _ => {
            println!("Local snapshots: {}", report.local_snapshots.names.len());
            for name in &report.local_snapshots.names {
                println!("  {name}");
            }
            println!(
                "Snapshot sizes are not reported by tmutil. To request reclamation, first preview with `diskr --thin-snapshots 10G {}`.",
                report.path.display()
            );
        }
    }
    Ok(())
}

fn print_packages(path: PathBuf, json: bool) -> Result<()> {
    let path = canonical_dir(path)?;
    let reports = packages::scan_managers();
    let project_deps = packages::find_project_deps(&path, 5);

    if json {
        let managers: Vec<_> = reports
            .iter()
            .filter(|r| r.available)
            .map(|r| {
                let pkgs: Vec<_> = r
                    .packages
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "name": p.name,
                            "version": p.version,
                            "logical": p.size.map(|s| s.logical),
                            "allocated": p.size.map(|s| s.allocated),
                            "path": p.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                            "metadata_path": p.metadata_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                        })
                    })
                    .collect();
                let mut obj = serde_json::json!({
                    "manager": r.manager.label(),
                    "count": r.packages.len(),
                    "total_logical": r.total_size.logical,
                    "total_allocated": r.total_size.allocated,
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
                })
            })
            .collect();
        let value = serde_json::json!({
            "system_packages": managers,
            "project_dependencies": projects,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    println!("Package report");
    println!();

    let mut any_manager = false;
    for report in &reports {
        if !report.available {
            continue;
        }
        any_manager = true;
        if let Some(w) = &report.warning {
            println!("{}: {w}", report.manager.label());
            println!();
            continue;
        }
        println!(
            "{}: {} packages · {}",
            report.manager.label(),
            report.packages.len(),
            human(report.total_size.allocated)
        );
        let mut sorted: Vec<_> = report.packages.iter().collect();
        sorted.sort_by(|a, b| {
            let a_size = a.size.map(|s| s.allocated).unwrap_or(0);
            let b_size = b.size.map(|s| s.allocated).unwrap_or(0);
            b_size.cmp(&a_size)
        });
        for pkg in sorted.iter().take(10) {
            let size_str = pkg
                .size
                .map(|s| human(s.allocated))
                .unwrap_or_else(|| String::from("?"));
            println!("  {:>10}  {} {}", size_str, pkg.name, pkg.version);
        }
        if report.packages.len() > 10 {
            println!("  … and {} more", report.packages.len() - 10);
        }
        println!();
    }

    if !any_manager {
        println!("No supported package managers found.");
        println!();
    }

    if !project_deps.is_empty() {
        println!("Project dependencies under {}", path.display());
        for dep in &project_deps {
            let size_str = dep
                .deps_size
                .map(|s| human(s.allocated))
                .unwrap_or_else(|| String::from("—"));
            println!(
                "  {:>10}  {} ({}, {} deps)",
                size_str,
                dep.path.display(),
                dep.manager_label,
                dep.dep_count
            );
        }
    }

    Ok(())
}

fn thin_snapshots(path: PathBuf, bytes: u64, confirmed: bool) -> Result<()> {
    if !path.exists() {
        bail!("path does not exist: {}", path.display());
    }
    if !confirmed {
        println!(
            "Would run: tmutil thinlocalsnapshots {} {} 4",
            path.display(),
            bytes
        );
        println!(
            "Re-run with --yes to request {} of local snapshot reclamation.",
            human(bytes)
        );
        return Ok(());
    }

    let result = space::thin_local_snapshots(&path, bytes)?;
    println!(
        "Requested {} of local snapshot reclamation for {}.",
        human(result.requested_bytes),
        result.path.display()
    );
    if !result.stdout.is_empty() {
        println!("{}", result.stdout);
    }
    if !result.stderr.is_empty() {
        eprintln!("{}", result.stderr);
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ExternalAction {
    QuickLook,
    RevealInFinder,
    Open,
}

impl ExternalAction {
    fn status_label(self) -> &'static str {
        match self {
            ExternalAction::QuickLook => "quick look",
            ExternalAction::RevealInFinder => "reveal in Finder",
            ExternalAction::Open => "open",
        }
    }
}

fn launch_external_action(app: &mut App, action: ExternalAction) {
    let Some((path, label)) = selected_action_target(app) else {
        app.status = String::from("nothing selected");
        return;
    };

    let result = match action {
        ExternalAction::QuickLook => spawn_quick_look(&path),
        ExternalAction::RevealInFinder => spawn_reveal_in_finder(&path),
        ExternalAction::Open => spawn_open(&path),
    };

    app.status = match result {
        Ok(()) => format!("{}: {label}", action.status_label()),
        Err(err) => format!("{} failed: {err}", action.status_label()),
    };
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

fn spawn_quick_look(path: &Path) -> Result<()> {
    let mut command = Command::new("qlmanage");
    command.arg("-p").arg(path);
    spawn_detached(&mut command)
}

fn spawn_reveal_in_finder(path: &Path) -> Result<()> {
    let mut command = Command::new("open");
    command.arg("-R").arg(path);
    spawn_detached(&mut command)
}

fn spawn_open(path: &Path) -> Result<()> {
    let mut command = Command::new("open");
    command.arg(path);
    spawn_detached(&mut command)
}

fn spawn_detached(command: &mut Command) -> Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn macOS command")?;
    Ok(())
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

    loop {
        if app.drain_scan_results() {
            needs_draw = true;
        }

        if needs_draw {
            terminal.draw(|f| ui::draw(f, app))?;
            needs_draw = false;
        }

        let timeout = if app.has_pending_scan_work() {
            Duration::from_millis(50)
        } else {
            Duration::from_secs(1)
        };

        if event::poll(timeout)? {
            match event::read()? {
                Event::Resize(_, _) => {
                    needs_draw = true;
                }
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    // Ctrl+C cancels the active mode/modal like Esc in every state.
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        cancel_active_state(app);
                        needs_draw = true;
                        continue;
                    }
                    // Ignore character keys carrying CONTROL, ALT, or SUPER (keep SHIFT,
                    // which is how uppercase letters and symbols arrive).
                    if char_modifier_inhibited(&key) {
                        continue;
                    }
                    if app.show_help {
                        let handled = match key.code {
                            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                                app.close_help();
                                true
                            }
                            _ => false,
                        };
                        if handled {
                            needs_draw = true;
                        }
                        continue;
                    }
                    if app.confirming_delete {
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
                    if app.confirming_uninstall {
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
                    if app.confirming_empty_trash {
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
                    if app.pkg_detail {
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
                    if app.top_files_open() {
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
                                    let result = spawn_reveal_in_finder(&path);
                                    app.status = match result {
                                        Ok(()) => String::from("revealed top file in Finder"),
                                        Err(err) => format!("reveal failed: {err}"),
                                    };
                                }
                                true
                            }
                            KeyCode::Char('O') => {
                                if let Some(path) = app.selected_top_file_path() {
                                    let result = spawn_open(&path);
                                    app.status = match result {
                                        Ok(()) => String::from("opened top file"),
                                        Err(err) => format!("open failed: {err}"),
                                    };
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
                    if app.reclaim_paths_open() {
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
                                if let Some((_, path)) = app.selected_reclaim_path() {
                                    let result = spawn_reveal_in_finder(&path);
                                    app.status = match result {
                                        Ok(()) => String::from("revealed reclaim path in Finder"),
                                        Err(err) => format!("reveal failed: {err}"),
                                    };
                                }
                                true
                            }
                            KeyCode::Char('O') => {
                                if let Some((_, path)) = app.selected_reclaim_path() {
                                    let result = spawn_open(&path);
                                    app.status = match result {
                                        Ok(()) => String::from("opened reclaim path"),
                                        Err(err) => format!("open failed: {err}"),
                                    };
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
                    if app.disk_info_open() {
                        let handled = match key.code {
                            KeyCode::Esc => {
                                app.close_disk_info();
                                true
                            }
                            _ => false,
                        };
                        if handled {
                            needs_draw = true;
                        }
                        continue;
                    }
                    if app.file_info_open {
                        let handled = match key.code {
                            KeyCode::Esc | KeyCode::Char('i') | KeyCode::Char('q') => {
                                app.close_file_info();
                                true
                            }
                            KeyCode::Char(' ') => {
                                app.close_file_info();
                                launch_external_action(app, ExternalAction::QuickLook);
                                true
                            }
                            KeyCode::Char('f') => {
                                app.close_file_info();
                                launch_external_action(app, ExternalAction::RevealInFinder);
                                true
                            }
                            KeyCode::Char('O') => {
                                app.close_file_info();
                                launch_external_action(app, ExternalAction::Open);
                                true
                            }
                            KeyCode::Char('d') => {
                                app.close_file_info();
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
                    if app.input_mode != app::InputMode::None {
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
                        KeyCode::Char('q') => return Ok(()),
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
                        KeyCode::Char(' ') => {
                            launch_external_action(app, ExternalAction::QuickLook);
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
                        KeyCode::Char('S') => {
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
                        KeyCode::Char('R') => {
                            app.request_reclaim_scan();
                            true
                        }
                        KeyCode::Char('o') => {
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
                            set_focus(app, Focus::Packages);
                            app.load_packages();
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
                        KeyCode::Char('B') => {
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
                        KeyCode::Char('.') => {
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

// Cancel whatever active state the app is in, mirroring Esc in each mode.
// Used by Ctrl+C so it always acts as a safe "get me out of here" chord.
fn cancel_active_state(app: &mut App) {
    if app.show_help {
        app.close_help();
    } else if app.confirming_delete {
        app.cancel_delete();
    } else if app.confirming_uninstall {
        app.cancel_uninstall();
    } else if app.confirming_empty_trash {
        app.cancel_empty_trash();
    } else if app.pkg_detail {
        app.close_pkg_detail();
    } else if app.top_files_open() {
        app.close_top_files();
    } else if app.reclaim_paths_open() {
        app.close_reclaim_paths();
    } else if app.disk_info_open() {
        app.close_disk_info();
    } else if app.file_info_open {
        app.close_file_info();
    } else if app.input_mode != app::InputMode::None {
        app.exit_input_mode();
    } else if app.search_mode || app.search_filter_active() {
        app.clear_search();
    } else if app.pkg_search_mode || app.pkg_filter_active() {
        app.clear_pkg_search();
    } else if app.focus != Focus::Files {
        app.focus = Focus::Files;
    }
    // In the base Files-focused state Esc is a no-op, so Ctrl+C is too.
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
        };
        let grew = history::ChildChange {
            name: String::from("logical-only"),
            before: Some(bulkstat::SizeInfo::new(120, 100)),
            after: Some(bulkstat::SizeInfo::new(200, 100)),
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
            }],
            total_size: crate::bulkstat::SizeInfo::default(),
            available: true,
            warning: None,
        }];
        app.project_deps = vec![packages::ProjectDeps {
            path: root.clone(),
            manager_label: String::from("cargo"),
            manifest: String::from("Cargo.toml"),
            dep_count: 3,
            deps_size: None,
            deps_dir: Some(root.join("target")),
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
                },
                packages::Package {
                    name: String::from("kubectl"),
                    version: String::from("1.0"),
                    size: Some(crate::bulkstat::SizeInfo::new(20, 20)),
                    path: None,
                    metadata_path: None,
                },
                packages::Package {
                    name: String::from("alpha"),
                    version: String::from("1.0"),
                    size: Some(crate::bulkstat::SizeInfo::new(10, 10)),
                    path: None,
                    metadata_path: None,
                },
            ],
            total_size: crate::bulkstat::SizeInfo::new(60, 60),
            available: true,
            warning: None,
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
