mod app;
mod bulkstat;
mod fs_ops;
mod fsevents;
mod history;
mod keymap;
mod manifest;
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
use terminal_text::{InputEdit, SafePath};

thread_local! {
    static CLI_OUTPUT: RefCell<io::BufWriter<io::Stdout>> =
        RefCell::new(io::BufWriter::new(io::stdout()));
    static REPORT_CONTEXT: RefCell<ReportContext> = RefCell::new(ReportContext::default());
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ReportOptions {
    strict: bool,
    min_bytes: u64,
    class: Option<reclaim::Reclaimability>,
}

struct ReportContext {
    options: ReportOptions,
    started: std::time::Instant,
    complete: bool,
}

impl Default for ReportContext {
    fn default() -> Self {
        Self {
            options: ReportOptions::default(),
            started: std::time::Instant::now(),
            complete: true,
        }
    }
}

#[derive(Debug)]
struct IncompleteReport;
impl fmt::Display for IncompleteReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .write_str("report is incomplete; --strict requires complete measurements (exit 2)")
    }
}
impl std::error::Error for IncompleteReport {}

fn report_options() -> ReportOptions {
    REPORT_CONTEXT.with(|context| context.borrow().options)
}

fn note_report_completeness(complete: bool) {
    REPORT_CONTEXT.with(|context| context.borrow_mut().complete &= complete);
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
        std::process::exit(if err.downcast_ref::<IncompleteReport>().is_some() {
            2
        } else {
            1
        });
    }
}

fn run_main() -> Result<()> {
    let (action, options) = match parse_args(std::env::args_os().skip(1))? {
        CliAction::Configured { action, options } => (*action, options),
        action => (action, ReportOptions::default()),
    };
    REPORT_CONTEXT.with(|context| {
        *context.borrow_mut() = ReportContext {
            options,
            ..ReportContext::default()
        }
    });
    let result = match action {
        CliAction::Configured { .. } => bail!("nested report configuration is unsupported"),
        CliAction::Help => print_help(),
        CliAction::Version => {
            cli_println!("diskr {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        CliAction::Top { path, limit, json } => print_top(path, limit, json),
        CliAction::Reclaim { path, json } => print_reclaim(path, json),
        CliAction::Save {
            path,
            json,
            baseline,
        } => save_baseline(path, json, baseline),
        CliAction::Diff {
            path,
            json,
            baseline,
            explicit_path,
        } => print_diff(path, json, baseline, explicit_path),
        CliAction::Space { path, json } => print_space(path, json),
        CliAction::Packages {
            path,
            json,
            scope,
            profile,
        } => {
            let (path, scope) = match profile {
                Some(name) => {
                    let profile = history::scan_profile(&name)?;
                    (profile.root, profile.scope)
                }
                None => (path, scope),
            };
            print_packages(path, json, scope)
        }
        CliAction::Library {
            operation,
            json,
            confirmed,
        } => {
            let mut value = history::manage_library(operation, confirmed)?;
            value["scope"] = serde_json::json!({"policy":"baseline/profile catalog metadata; saved roots are not rescanned"});
            let warnings: Vec<String> = value["history_warning"]
                .as_str()
                .map(str::to_owned)
                .into_iter()
                .collect();
            let complete = warnings.is_empty();
            note_report_completeness(complete);
            if json {
                emit_report(
                    "baseline_library",
                    &state::state_dir(),
                    value,
                    complete,
                    warnings,
                )
            } else {
                cli_println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
        }
        CliAction::ThinSnapshots {
            path,
            bytes,
            confirmed,
        } => thin_snapshots(path, bytes, confirmed),
        CliAction::Run(start) => run_app(start),
    };
    result?;
    flush_cli()?;
    if options.strict && !REPORT_CONTEXT.with(|context| context.borrow().complete) {
        return Err(IncompleteReport.into());
    }
    Ok(())
}

fn run_app(start: PathBuf) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("diskr requires an interactive terminal");
    }

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

    let startup_label = format!(
        "diskr\nloading state for {}\nq / Esc / Ctrl+C cancel",
        SafePath(&start)
    );
    terminal.draw(|frame| {
        frame.render_widget(
            ratatui_widgets::paragraph::Paragraph::new(startup_label.clone()),
            frame.area(),
        )
    })?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(canonical_dir(start).and_then(App::new_streaming));
    });
    let mut app = loop {
        match rx.try_recv() {
            Ok(result) => break result?,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => bail!("startup worker stopped"),
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press
                        && (matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
                            || (key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL))) =>
                {
                    return Ok(())
                }
                Event::Resize(_, _) => {
                    terminal.draw(|frame| {
                        frame.render_widget(
                            ratatui_widgets::paragraph::Paragraph::new(startup_label.clone()),
                            frame.area(),
                        )
                    })?;
                }
                _ => {}
            }
        }
    };

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
    Configured {
        action: Box<CliAction>,
        options: ReportOptions,
    },
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
        baseline: Option<String>,
    },
    Diff {
        path: PathBuf,
        json: bool,
        baseline: Option<String>,
        explicit_path: bool,
    },
    Space {
        path: PathBuf,
        json: bool,
    },
    Packages {
        path: PathBuf,
        json: bool,
        scope: packages::InventoryScope,
        profile: Option<String>,
    },
    Library {
        operation: history::LibraryAction,
        json: bool,
        confirmed: bool,
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
    let mut options = ReportOptions::default();
    let mut inventory_scope = packages::InventoryScope::default();
    let mut inventory_options = false;
    let mut explicit_managers = false;
    let mut projects_only = false;
    let mut baseline = None;
    let mut profile = None;
    let mut save_profile = None;
    let mut library_action = None;

    while let Some(arg) = args.next() {
        if separator_seen {
            set_cli_path(&mut path, arg)?;
            continue;
        }

        let arg_text = arg.to_string_lossy().into_owned();
        match arg_text.as_str() {
            "--baseline" => {
                if baseline.is_some() {
                    bail!("specify --baseline once");
                }
                baseline = Some(parse_library_name(args.next())?);
            }
            "--profile" => {
                if profile.is_some() {
                    bail!("specify --profile once");
                }
                profile = Some(parse_library_name(args.next())?);
            }
            "--save-profile" => {
                if save_profile.is_some() {
                    bail!("specify --save-profile once");
                }
                save_profile = Some(parse_library_name(args.next())?);
            }
            "--baselines" => {
                if library_action.is_some() {
                    bail!("choose one baseline library operation");
                }
                let operation = args
                    .next()
                    .context("--baselines requires list, pin, unpin, prune, export, import or remove-profile")?;
                library_action = Some(match operation.to_str() {
                    Some("list") => history::LibraryAction::List,
                    Some("remove-profile") => history::LibraryAction::RemoveProfile {
                        name: parse_library_name(args.next())?,
                    },
                    Some("pin") => history::LibraryAction::Pin {
                        name: parse_library_name(args.next())?,
                        pinned: true,
                    },
                    Some("unpin") => history::LibraryAction::Pin {
                        name: parse_library_name(args.next())?,
                        pinned: false,
                    },
                    Some("prune") => {
                        let keep = args
                            .next()
                            .context("prune requires an unpinned retention count")?;
                        history::LibraryAction::Prune {
                            keep: keep
                                .to_str()
                                .and_then(|value| value.parse().ok())
                                .context("invalid prune retention count")?,
                        }
                    }
                    Some("export") => history::LibraryAction::Export(PathBuf::from(
                        args.next()
                            .context("export requires a new destination file")?,
                    )),
                    Some("import") => history::LibraryAction::Import(PathBuf::from(
                        args.next().context("import requires a library file")?,
                    )),
                    _ => bail!("--baselines requires list, pin, unpin, prune, export, import or remove-profile"),
                });
            }
            "--projects-only" => {
                inventory_options = true;
                projects_only = true;
                inventory_scope.managers.clear();
            }
            "--no-projects" => {
                inventory_options = true;
                inventory_scope.projects = false;
            }
            "--manager" => {
                inventory_options = true;
                let value = args.next().context("--manager requires a manager key")?;
                let manager = value
                    .to_str()
                    .and_then(packages::Manager::from_key)
                    .context("--manager expects brew, brew-cask, npm, pip, cargo or bun")?;
                if !explicit_managers {
                    inventory_scope.managers.clear();
                }
                explicit_managers = true;
                if !inventory_scope.managers.contains(&manager) {
                    inventory_scope.managers.push(manager);
                }
            }
            "--max-depth" => {
                inventory_options = true;
                let value = args.next().context("--max-depth requires 0 through 64")?;
                inventory_scope.depth = value
                    .to_str()
                    .and_then(|value| value.parse().ok())
                    .filter(|depth| *depth <= 64)
                    .context("--max-depth requires 0 through 64")?;
            }
            "--exclude" => {
                inventory_options = true;
                let excluded = PathBuf::from(
                    args.next()
                        .context("--exclude requires a root-relative subtree")?,
                );
                if excluded.as_os_str().is_empty()
                    || excluded.components().any(|component| {
                        matches!(
                            component,
                            std::path::Component::ParentDir
                                | std::path::Component::RootDir
                                | std::path::Component::Prefix(_)
                        )
                    })
                {
                    bail!("--exclude requires a root-relative subtree without ..");
                }
                inventory_scope.excludes.push(excluded);
            }
            "--strict" => options.strict = true,
            "--min-size" => {
                let value = args.next().context("--min-size requires a byte size")?;
                options.min_bytes = space::parse_byte_size(&value.to_string_lossy())?;
            }
            "--class" => {
                let value = args
                    .next()
                    .context("--class requires safe, regenerable or risky")?;
                options.class = Some(match value.to_str() {
                    Some("safe") => reclaim::Reclaimability::Safe,
                    Some("regenerable") => reclaim::Reclaimability::Regenerable,
                    Some("risky") => reclaim::Reclaimability::Risky,
                    _ => bail!("--class requires safe, regenerable or risky"),
                });
            }
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
        && library_action.is_none()
        && save_profile.is_none()
    {
        bail!("--json requires --top, --reclaim, --save, --diff, --space, or --packages (also supported by --baselines and --save-profile)");
    }
    if confirmed
        && thin_snapshots.is_none()
        && !matches!(
            library_action.as_ref(),
            Some(
                history::LibraryAction::Prune { .. } | history::LibraryAction::RemoveProfile { .. }
            )
        )
    {
        bail!("--yes requires --thin-snapshots or --baselines prune/remove-profile");
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
        + usize::from(library_action.is_some())
        + usize::from(save_profile.is_some())
        + usize::from(thin_snapshots.is_some());
    if mode_count > 1 {
        bail!("choose only one of --top, --reclaim, --save, --diff, --space, --packages, or --thin-snapshots; baseline-library and save-profile modes are also exclusive");
    }

    if options.strict && (mode_count == 0 || thin_snapshots.is_some()) {
        bail!("--strict requires a report mode");
    }
    if options.min_bytes > 0 && top_limit.is_none() && !reclaim_report && !packages_report {
        bail!("--min-size requires --top, --reclaim or --packages");
    }
    if options.class.is_some() && !reclaim_report {
        bail!("--class requires --reclaim");
    }
    if inventory_options && !packages_report && save_profile.is_none() {
        bail!("inventory scope options require --packages or --save-profile");
    }
    if projects_only && (explicit_managers || !inventory_scope.projects) {
        bail!("--projects-only cannot be combined with --manager or --no-projects");
    }
    if baseline.is_some() && !save_baseline && !diff_baseline {
        bail!("--baseline requires --save or --diff");
    }
    if profile.is_some() && (!packages_report || path.is_some() || inventory_options) {
        bail!("--profile requires --packages without an explicit path or scope overrides");
    }
    if library_action.is_some() && path.is_some() {
        bail!("baseline library operations do not accept a scan path");
    }
    let explicit_path = path.is_some();
    let action: Result<CliAction> = if let Some(name) = save_profile {
        Ok(CliAction::Library {
            operation: history::LibraryAction::SaveProfile {
                name,
                root: path.unwrap_or_else(dirs_home),
                scope: inventory_scope,
            },
            json,
            confirmed,
        })
    } else if let Some(operation) = library_action {
        Ok(CliAction::Library {
            operation,
            json,
            confirmed,
        })
    } else {
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
                baseline,
            }),
            None if diff_baseline => Ok(CliAction::Diff {
                path: path.unwrap_or_else(dirs_home),
                json,
                baseline,
                explicit_path,
            }),
            None if space_report => Ok(CliAction::Space {
                path: path.unwrap_or_else(dirs_home),
                json,
            }),
            None if packages_report => Ok(CliAction::Packages {
                path: path.unwrap_or_else(dirs_home),
                json,
                scope: inventory_scope,
                profile,
            }),
            None if thin_snapshots.is_some() => Ok(CliAction::ThinSnapshots {
                path: path.unwrap_or_else(dirs_home),
                bytes: thin_snapshots.unwrap_or_default(),
                confirmed,
            }),
            None => Ok(CliAction::Run(path.unwrap_or_else(dirs_home))),
        }
    };
    let action = action?;
    Ok(if options == ReportOptions::default() {
        action
    } else {
        CliAction::Configured {
            action: Box::new(action),
            options,
        }
    })
}

fn parse_library_name(value: Option<OsString>) -> Result<String> {
    let value = value.context("option requires a name")?;
    let name = value.to_str().context("names must be UTF-8")?;
    history::validate_library_name(name)?;
    Ok(name.to_owned())
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
  diskr --save --baseline NAME [--json] [PATH]
  diskr --diff --baseline NAME [--json] [PATH]
  diskr --baselines list|pin NAME|unpin NAME|prune KEEP|export FILE|import FILE
  diskr --baselines remove-profile NAME [--yes]
  diskr --save-profile NAME [inventory options] [PATH]
  diskr --packages --profile NAME [--json]
  diskr --thin-snapshots SIZE [--yes] [PATH]

Report options:
  --strict             Exit 2 after printing an incomplete report (default: 0)
  --min-size SIZE      Filter rows in --top, --reclaim or --packages
  --class CLASS        Filter --reclaim: safe, regenerable or risky
  --projects-only      --packages: discover projects without running managers
  --manager KEY        --packages: select manager (repeatable); projects remain on
  --no-projects        --packages: omit project discovery
  --max-depth N        --packages: project depth, 0 through 64 (default: 5)
  --exclude SUBTREE    --packages: exclude a root-relative project subtree
  --baseline NAME      Select an immutable named capture for --save or --diff
  --save-profile NAME  Save a root and package scope without running managers
  --profile NAME       Use a local package profile; no path/scope overrides
  --baselines prune N  Preview removal of older unpinned captures; --yes applies

Terminal preferences:
  DISKR_THEME=dark|light|high-contrast|mono
  DISKR_REDUCED_MOTION=1; DISKR_ASCII=1; NO_COLOR selects monochrome.

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

fn emit_report(
    kind: &str,
    root: &Path,
    mut value: serde_json::Value,
    complete: bool,
    warnings: Vec<String>,
) -> Result<()> {
    note_report_completeness(complete);
    value["schema_version"] = serde_json::json!(1);
    value["report"] = serde_json::json!(kind);
    value["root"] = serde_json::json!(root.to_string_lossy());
    value["root_identity"] = serde_json::json!(state::encode_path(root));
    if !value["scope"].is_object() {
        value["scope"] = serde_json::json!({});
    }
    value["scope"]["root"] = serde_json::json!(root.to_string_lossy());
    if !value["coverage"].is_object() {
        value["coverage"] = serde_json::json!({
            "inaccessible": value["inaccessible"].as_u64().unwrap_or(0),
            "skipped_mounts": value["skipped_mounts"].as_u64().unwrap_or(0),
        });
    }
    value["coverage"]["complete"] = serde_json::json!(complete);
    value["coverage"]["status"] = serde_json::json!(if complete { "complete" } else { "partial" });
    value["warnings"] = serde_json::json!(warnings);
    let elapsed = REPORT_CONTEXT.with(|context| {
        context
            .borrow()
            .started
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64
    });
    value["timing"] = serde_json::json!({"elapsed_ms": elapsed});
    value["numeric_semantics"] = serde_json::json!({"bytes": "unsigned 64-bit integer byte counts", "signed_deltas": "base-10 signed integer strings, no floating-point conversion", "timestamps": "Unix epoch seconds", "elapsed_ms": "monotonic elapsed milliseconds"});
    let options = report_options();
    value["filters"] = serde_json::json!({"min_allocated_bytes": options.min_bytes, "class": options.class.map(|class| class.label())});
    value["scope"]["totals"] =
        serde_json::json!("unfiltered requested scope; filters affect rows only");
    cli_println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn scan_warnings(inaccessible: u32, skipped_mounts: u32) -> Vec<String> {
    let mut warnings = Vec::new();
    if inaccessible > 0 {
        warnings.push(format!(
            "{inaccessible} unreadable paths; measurements are lower bounds"
        ));
    }
    if skipped_mounts > 0 {
        warnings.push(format!("{skipped_mounts} mounted filesystems skipped"));
    }
    warnings
}

fn print_filter_notice() -> Result<()> {
    let options = report_options();
    if options.min_bytes > 0 || options.class.is_some() {
        cli_println!("Row filters are active; totals and completeness describe the unfiltered scan. Unknown/partial sizes remain visible.");
    }
    Ok(())
}

fn print_top(path: PathBuf, limit: usize, json: bool) -> Result<()> {
    let path = canonical_dir(path)?;

    let mut scan = bulkstat::scan_dir(&path, limit);
    let complete = scan.inaccessible == 0 && scan.skipped_mounts == 0;
    let warnings = scan_warnings(scan.inaccessible, scan.skipped_mounts);
    note_report_completeness(complete);
    let min_bytes = report_options().min_bytes;
    let unfiltered_count = scan.largest_files.len();
    scan.largest_files
        .retain(|file| file.size.allocated >= min_bytes);
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
            "filtered_out": unfiltered_count - scan.largest_files.len(),
            "scope": {"policy": "same selected filesystem; regular-file content; no symlink traversal"},
        });
        return emit_report("top", &path, report, complete, warnings);
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
    print_filter_notice()?;
    if scan.largest_files.is_empty() {
        cli_println!(
            "{}",
            if unfiltered_count > 0 {
                "No regular files match the row filter."
            } else {
                "No regular files found."
            }
        );
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

    let mut report = reclaim::report(&path);
    let complete = report.inaccessible == 0 && report.skipped_mounts == 0;
    let warnings = scan_warnings(report.inaccessible, report.skipped_mounts);
    note_report_completeness(complete);
    let options = report_options();
    let unfiltered_count = report.findings.len();
    report.findings.retain(|finding| {
        options.class.is_none_or(|class| finding.class == class)
            && (finding.size.allocated >= options.min_bytes
                || finding.inaccessible > 0
                || finding.skipped_mounts > 0)
    });
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
            "filtered_out": unfiltered_count - report.findings.len(),
        });
        return emit_report("reclaim", &path, value, complete, warnings);
    }

    cli_println!("Cleanup candidates under {}", SafePath(&report.root));
    print_filter_notice()?;
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

fn save_baseline(path: PathBuf, json: bool, baseline: Option<String>) -> Result<()> {
    let path = canonical_dir(path)?;
    let record = if let Some(name) = &baseline {
        let record = history::scan_record(&path)?;
        history::save_named_baseline(name, &record)?;
        record
    } else {
        history::save(&path)?
    };
    let total = record.total();
    let complete = record.inaccessible() == 0 && record.skipped_mounts() == 0;
    let warnings = scan_warnings(record.inaccessible(), record.skipped_mounts());
    note_report_completeness(complete);
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
                    "inaccessible": child.inaccessible,
                    "skipped_mounts": child.skipped_mounts,
                })
            })
            .collect();
        let value = serde_json::json!({
            "path": record.path.to_string_lossy(),
            "timestamp": record.timestamp,
            "baseline_name": baseline,
            "retention": {"latest_root_limit":history::HISTORY_MAX_RECORDS,"named":"explicit prune only; pinned records protected"},
            "total_logical": total.logical,
            "total_allocated": total.allocated,
            "inaccessible": record.inaccessible(),
            "skipped_mounts": record.skipped_mounts(),
            "scope": {"policy": "root children measured on the selected filesystem; hardlinks deduplicated; no symlink traversal"},
            "children": children,
        });
        return emit_report("save", &path, value, complete, warnings);
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
        "Compare later with: diskr --diff {}{}",
        baseline
            .as_ref()
            .map(|name| format!("--baseline {name} "))
            .unwrap_or_default(),
        shell_quote_path(&record.path)
    );
    cli_println!("Retention: latest per-root history keeps {} roots; named baselines require explicit prune and may be pinned.", history::HISTORY_MAX_RECORDS);
    Ok(())
}

fn print_diff(
    path: PathBuf,
    json: bool,
    baseline: Option<String>,
    explicit_path: bool,
) -> Result<()> {
    let (path, report, origin, foreign) = if let Some(name) = baseline {
        let (baseline, foreign) = history::named_baseline(&name)?;
        let path = canonical_dir(if explicit_path {
            path
        } else {
            baseline.record.path.clone()
        })?;
        if path != baseline.record.path {
            bail!("named baseline root does not match selected path; roots are never implicitly retargeted");
        }
        let report = history::diff_from_record(&baseline.record, &path)?;
        let origin = serde_json::json!({"name":name,"machine":baseline.machine,"foreign_machine":foreign,
            "root_identity":state::encode_path(&baseline.record.path)});
        (path, report, Some(origin), foreign)
    } else {
        let path = canonical_dir(path)?;
        let report = history::diff(&path)?;
        (path, report, None, false)
    };
    let complete = report.complete() && !foreign;
    let mut warnings = scan_warnings(
        report
            .baseline_inaccessible
            .saturating_add(report.current_inaccessible),
        report
            .baseline_skipped_mounts
            .saturating_add(report.current_skipped_mounts),
    );
    if foreign {
        warnings.push(String::from("baseline was captured on another machine; allocation deltas are cross-environment comparisons"));
    }
    note_report_completeness(complete);
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
            "baseline_origin": origin,
            "current_timestamp": report.current_timestamp,
            "before_total_logical": report.before_total.logical,
            "before_total_allocated": report.before_total.allocated,
            "after_total_logical": report.after_total.logical,
            "after_total_allocated": report.after_total.allocated,
            "baseline_inaccessible": report.baseline_inaccessible,
            "current_inaccessible": report.current_inaccessible,
            "delta_complete": complete,
            "baseline_skipped_mounts": report.baseline_skipped_mounts,
            "current_skipped_mounts": report.current_skipped_mounts,
            "total_delta_logical": report.total_delta_logical().to_string(),
            "total_delta_allocated": report.total_delta_allocated().to_string(),
            "changes": changes,
            "scope": {"policy": "saved and current root children on the selected filesystem; no symlink traversal"},
            "coverage": {"baseline_inaccessible": report.baseline_inaccessible, "current_inaccessible": report.current_inaccessible, "baseline_skipped_mounts": report.baseline_skipped_mounts, "current_skipped_mounts": report.current_skipped_mounts},
        });
        return emit_report("diff", &path, value, complete, warnings);
    }

    cli_println!("Diff for {}", SafePath(&report.path));
    if foreign {
        cli_println!("Warning: baseline captured on another machine; allocation deltas are cross-environment comparisons.");
    }
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
        if !complete {
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
    let mut warnings = Vec::new();
    if let Some(error) = &report.apfs_error {
        warnings.push(error.to_string());
    }
    if let Some(error) = &report.local_snapshots.error {
        warnings.push(format!("snapshot discovery: {error}"));
    }
    let complete = warnings.is_empty();
    note_report_completeness(complete);
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
            "apfs_error": report.apfs_error.as_ref().map(ToString::to_string),
            "apfs_diagnostic": report.apfs_error.as_ref().map(|error| error.json()),
            "local_snapshots": snapshots,
            "local_snapshot_error": report.local_snapshots.error,
            "scope": {"policy": "owning volume statfs, APFS and snapshot utility queries", "mount": report.mount.to_string_lossy()},
            "coverage": {"utility_failures": warnings.len()},
        });
        return emit_report("space", &path, value, complete, warnings);
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
            terminal_text::sanitize(&error.to_string())
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

fn print_packages(path: PathBuf, json: bool, scope: packages::InventoryScope) -> Result<()> {
    let path = canonical_dir(path)?;
    let mut reports = packages::scan_selected_managers(&scope.managers);
    let mut project_scan = if scope.projects {
        packages::scan_project_deps_streamed(
            &path,
            &scope,
            &bulkstat::ScanCancellation::never(),
            &|_| {},
        )
    } else {
        packages::ProjectDepsReport {
            projects: Vec::new(),
            inaccessible: 0,
            scope: bulkstat::DiscoveryCoverage {
                depth_limit: scope.depth,
                ..Default::default()
            },
        }
    };
    let graph = packages::scan_dep_graph(&reports);
    let mut warnings = scan_warnings(project_scan.inaccessible, project_scan.scope.skipped_mounts);
    let unknown = reports
        .iter()
        .map(|report| report.unknown_sizes())
        .sum::<usize>();
    let partial = reports
        .iter()
        .map(|report| report.partial_sizes())
        .sum::<usize>();
    let scope_limited = reports
        .iter()
        .map(|report| report.limited_sizes())
        .sum::<usize>();
    for report in &reports {
        if let Some(warning) = &report.warning {
            warnings.push(format!("{}: {warning}", report.manager.label()));
        }
    }
    if unknown > 0 || partial > 0 || scope_limited > 0 {
        warnings.push(format!("package footprints: {unknown} unknown, {partial} partial, {scope_limited} limited scope"));
    }
    for warning in graph.warnings() {
        warnings.push(format!(
            "{} dependency graph: {}",
            warning.manager.label(),
            warning.message
        ));
    }
    for dep in &project_scan.projects {
        warnings.extend(
            dep.dep_count
                .warnings
                .iter()
                .map(|warning| format!("{}: {warning}", dep.path.display())),
        );
        if dep.coverage.is_partial() || dep.deps_size.is_none() {
            warnings.push(format!(
                "{}: project footprint incomplete or unavailable",
                dep.path.display()
            ));
        }
    }
    let complete = warnings.is_empty();
    note_report_completeness(complete);
    let min_bytes = report_options().min_bytes;
    let inventory_counts: Vec<_> = reports.iter().map(|report| report.packages.len()).collect();
    for report in &mut reports {
        report.packages.retain(|package| {
            package.size.is_none_or(|size| size.allocated >= min_bytes)
                || package.coverage.is_partial()
                || package.coverage.basis.is_limited()
        });
    }
    let project_count = project_scan.projects.len();
    project_scan.projects.retain(|dep| {
        dep.deps_size.is_none_or(|size| size.allocated >= min_bytes) || dep.coverage.is_partial()
    });
    let project_deps = &project_scan.projects;

    if json {
        let managers: Vec<_> = reports
            .iter()
            .enumerate()
            .map(|(index, r)| {
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
                    "inventory_count": inventory_counts[index],
                    "filtered_out": inventory_counts[index] - r.packages.len(),
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
                    "dep_count": d.dep_count.complete.then_some(d.dep_count.known),
                    "known_dep_count": d.dep_count.known,
                    "dependency_count_complete": d.dep_count.complete,
                    "dependency_warnings": d.dep_count.warnings,
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
            "project_inventory_count": project_count,
            "project_filtered_out": project_count - project_deps.len(),
            "scope": {"policy": "retained manager environments and bounded project discovery", "project": project_scan.scope.json(), "inventory": scope.json()},
            "coverage": {"project_inaccessible": project_scan.inaccessible, "project_skipped_mounts": project_scan.scope.skipped_mounts, "unknown_footprints": unknown, "partial_footprints": partial, "scope_limited_footprints": scope_limited},
            "dependency_graph_warnings": graph.warnings().iter().map(|warning| serde_json::json!({
                "manager": warning.manager.label(),
                "message": warning.message,
            })).collect::<Vec<_>>(),
        });
        return emit_report("packages", &path, value, complete, warnings);
    }

    cli_println!("Package report");
    cli_println!("Inventory scope: {}", scope.json());
    print_filter_notice()?;
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
            for warning in &dep.dep_count.warnings {
                cli_println!(
                    "    Dependency warning: {}",
                    terminal_text::sanitize(warning)
                );
            }
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
                            KeyCode::Esc | KeyCode::Char('t') => {
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
                            _ => handle_text_navigation(app, key.code),
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
                                app.edit_active_input(InputEdit::Home);
                                true
                            }
                            KeyCode::End => {
                                app.edit_active_input(InputEdit::End);
                                true
                            }
                            KeyCode::Char(ch) => {
                                app.search_push(ch);
                                true
                            }
                            _ => handle_text_navigation(app, key.code),
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
                        KeyCode::Char('b') => {
                            app.full_width = !app.full_width;
                            app.status = if app.full_width {
                                String::from("full-width view · b restore side panels")
                            } else {
                                String::from("side panels visible")
                            };
                            true
                        }
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
            if app.focus == Focus::Reclaim && app.reclaim_loading {
                app.cancel_reclaim_scan();
                app.focus = Focus::Files;
            } else if app.focus == Focus::Packages && (app.packages_loading || app.deps_loading) {
                app.cancel_package_scan();
            } else if app.focus == Focus::Files && app.listing_loading() {
                app.cancel_listing();
            } else if app.focus == Focus::Packages
                && (app.pkg_search_mode || app.pkg_filter_active())
            {
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

fn handle_text_navigation(app: &mut App, code: KeyCode) -> bool {
    let edit = match code {
        KeyCode::Left => InputEdit::Left,
        KeyCode::Right => InputEdit::Right,
        KeyCode::Home => InputEdit::Home,
        KeyCode::End => InputEdit::End,
        KeyCode::Delete => InputEdit::Delete,
        _ => return false,
    };
    app.edit_active_input(edit);
    true
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
            app.edit_active_input(InputEdit::Home);
            true
        }
        KeyCode::End => {
            app.edit_active_input(InputEdit::End);
            true
        }
        KeyCode::Char(ch) => {
            app.pkg_search_push(ch);
            true
        }
        _ => handle_text_navigation(app, code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_and_package_filter_keys_edit_at_the_caret() {
        let root = test_root("input_caret_filters");
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(root.clone()).unwrap();
        app.enter_search();
        for ch in "a界e\u{301}".chars() {
            app.search_push(ch);
        }
        assert!(handle_text_navigation(&mut app, KeyCode::Home));
        assert!(handle_text_navigation(&mut app, KeyCode::Delete));
        assert_eq!(app.search_query, "界e\u{301}");
        assert!(handle_text_navigation(&mut app, KeyCode::Right));
        app.search_pop();
        assert_eq!(app.search_query, "e\u{301}");
        app.clear_search();
        app.focus = Focus::Packages;
        app.enter_pkg_search();
        for ch in "ab".chars() {
            handle_pkg_search_key(&mut app, KeyCode::Char(ch));
        }
        handle_pkg_search_key(&mut app, KeyCode::Left);
        handle_pkg_search_key(&mut app, KeyCode::Char('界'));
        assert_eq!(app.pkg_search_query, "a界b");
        handle_pkg_search_key(&mut app, KeyCode::Delete);
        assert_eq!(app.pkg_search_query, "a界");
        handle_pkg_search_key(&mut app, KeyCode::Home);
        handle_pkg_search_key(&mut app, KeyCode::Delete);
        assert_eq!(app.pkg_search_query, "界");
        std::fs::remove_dir_all(root).unwrap();
    }

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
    fn library_modes_reject_conflicting_roots_overrides_and_confirmation_flags() {
        for arguments in [
            vec!["--baseline", "capture"],
            vec!["--baselines", "list", "/tmp"],
            vec!["--baselines", "pin"],
            vec!["--baselines", "prune", "-1"],
            vec!["--baselines", "list", "--yes"],
            vec!["--packages", "--profile", "work", "/tmp"],
            vec!["--packages", "--profile", "work", "--max-depth", "2"],
            vec!["--save-profile", "work", "--packages"],
            vec!["--save-profile", "work", "--save-profile", "other"],
            vec!["--save", "--baseline", "a", "--baseline", "b"],
            vec!["--save", "--baseline", "bad/name"],
        ] {
            assert!(parse(&arguments).is_err(), "accepted {arguments:?}");
        }
        assert!(matches!(
            parse(&["--diff", "--baseline", "capture"]).unwrap(),
            CliAction::Diff {
                baseline: Some(_),
                explicit_path: false,
                ..
            }
        ));
        assert!(matches!(
            parse(&["--baselines", "remove-profile", "work", "--yes"]).unwrap(),
            CliAction::Library {
                confirmed: true,
                ..
            }
        ));
    }

    #[test]
    fn inventory_options_preserve_scope_and_reject_ambiguous_arguments() {
        let CliAction::Packages { scope, .. } = parse(&[
            "--packages",
            "--projects-only",
            "--max-depth",
            "0",
            "--exclude",
            "archive",
        ])
        .unwrap() else {
            panic!("package action")
        };
        assert!(scope.managers.is_empty());
        assert!(scope.projects);
        assert_eq!(scope.depth, 0);
        assert_eq!(scope.excludes, [PathBuf::from("archive")]);
        let CliAction::Packages { scope, .. } = parse(&[
            "--packages",
            "--manager",
            "cargo",
            "--manager",
            "cargo",
            "--no-projects",
        ])
        .unwrap() else {
            panic!("package action")
        };
        assert_eq!(scope.managers, [packages::Manager::Cargo]);
        assert!(!scope.projects);
        for arguments in [
            vec!["--projects-only"],
            vec!["--packages", "--manager", "unknown"],
            vec!["--packages", "--projects-only", "--manager", "cargo"],
            vec!["--packages", "--manager", "cargo", "--projects-only"],
            vec!["--packages", "--projects-only", "--no-projects"],
            vec!["--packages", "--max-depth", "65"],
            vec!["--packages", "--max-depth", "-1"],
            vec!["--packages", "--exclude", "../outside"],
            vec!["--packages", "--exclude", "/outside"],
            vec!["--top", "5", "--manager", "cargo"],
        ] {
            assert!(parse(&arguments).is_err(), "accepted {arguments:?}");
        }
    }

    #[test]
    fn report_options_are_explicit_and_reject_incompatible_modes() {
        let CliAction::Configured { action, options } = parse(&[
            "--reclaim",
            "--strict",
            "--min-size",
            "1GiB",
            "--class",
            "risky",
        ])
        .unwrap() else {
            panic!("configured report")
        };
        assert!(matches!(*action, CliAction::Reclaim { .. }));
        assert!(options.strict);
        assert_eq!(options.min_bytes, 1_073_741_824);
        assert_eq!(options.class, Some(reclaim::Reclaimability::Risky));
        for arguments in [
            vec!["--strict"],
            vec!["--strict", "--thin-snapshots", "1GiB"],
            vec!["--space", "--min-size", "1MiB"],
            vec!["--top", "5", "--class", "safe"],
            vec!["--reclaim", "--class", "unknown"],
            vec!["--reclaim", "--min-size", "0"],
            vec!["--reclaim", "--min-size"],
            vec!["--reclaim", "--class"],
            vec!["--reclaim", "--min-size", "18446744073709551616"],
        ] {
            assert!(parse(&arguments).is_err(), "accepted {arguments:?}");
        }
        assert!(matches!(parse(&["--top", "5", "--", "--strict"]).unwrap(),
            CliAction::Top { path, .. } if path == Path::new("--strict")));
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
            CliAction::Save { path, json: false, .. } if path == std::path::Path::new("/tmp")
        ));
    }

    #[test]
    fn parses_diff_report() {
        let action = parse(&["--diff", "--json", "/tmp"]).unwrap();
        assert!(matches!(
            action,
            CliAction::Diff { path, json: true, .. } if path == std::path::Path::new("/tmp")
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
                json: true,
                ..
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
            dep_count: 3usize.into(),
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
        assert!(print_packages(missing, false, packages::InventoryScope::default()).is_err());
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
