use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use crate::bulkstat::{self, SizeInfo};
pub use crate::manifest::DependencyCount;
use crate::manifest::{self, bounded_text};
use crate::pool;
use crate::process::CommandResult;
use crate::terminal_text;

thread_local! {
    static SCAN_CANCELLATION: std::cell::RefCell<Option<bulkstat::ScanCancellation>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn with_scan_cancellation<R>(
    cancellation: &bulkstat::ScanCancellation,
    run: impl FnOnce() -> R,
) -> R {
    struct Restore(Option<bulkstat::ScanCancellation>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SCAN_CANCELLATION.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let previous = SCAN_CANCELLATION.with(|slot| slot.replace(Some(cancellation.clone())));
    let _restore = Restore(previous);
    run()
}

fn current_cancellation() -> bulkstat::ScanCancellation {
    SCAN_CANCELLATION
        .with(|slot| slot.borrow().clone())
        .unwrap_or_else(bulkstat::ScanCancellation::never)
}

fn scan_map<T: Send, R: Send>(items: Vec<T>, run: impl Fn(T) -> R + Sync) -> Vec<R> {
    let cancellation = current_cancellation();
    pool::par_map(items, |item| {
        with_scan_cancellation(&cancellation, || run(item))
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub size: Option<SizeInfo>,
    pub path: Option<PathBuf>,
    pub metadata_path: Option<PathBuf>,
    pub coverage: FootprintCoverage,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FootprintBasis {
    #[default]
    Unknown,
    Tree,
    AppAndMetadata,
    InstallerMetadata,
    KnownBinaries,
    RecordFiles,
}

impl FootprintBasis {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Tree => "directory tree",
            Self::AppAndMetadata => "app and manager metadata",
            Self::InstallerMetadata => "installer metadata only",
            Self::KnownBinaries => "known installed binaries",
            Self::RecordFiles => "RECORD-listed files",
        }
    }

    pub fn is_limited(self) -> bool {
        matches!(
            self,
            Self::Unknown | Self::InstallerMetadata | Self::KnownBinaries | Self::RecordFiles
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FootprintCoverage {
    pub basis: FootprintBasis,
    pub inaccessible: u32,
    pub skipped_mounts: u32,
    pub missing_files: u32,
}

impl FootprintCoverage {
    fn new(basis: FootprintBasis) -> Self {
        Self {
            basis,
            ..Self::default()
        }
    }

    fn add_scan(&mut self, scan: &bulkstat::DirScan) {
        self.inaccessible = self.inaccessible.saturating_add(scan.inaccessible);
        self.skipped_mounts = self.skipped_mounts.saturating_add(scan.skipped_mounts);
    }

    pub fn is_partial(self) -> bool {
        self.inaccessible > 0 || self.skipped_mounts > 0 || self.missing_files > 0
    }

    pub fn json(self) -> serde_json::Value {
        serde_json::json!({
            "basis": self.basis.label(),
            "scope_limited": self.basis.is_limited(),
            "partial": self.is_partial(),
            "inaccessible": self.inaccessible,
            "skipped_mounts": self.skipped_mounts,
            "missing_files": self.missing_files,
        })
    }

    pub fn summary(self) -> String {
        format!(
            "{} · {} unreadable · {} mounts skipped · {} missing",
            self.basis.label(),
            self.inaccessible,
            self.skipped_mounts,
            self.missing_files
        )
    }
}

fn tree_footprint(
    path: Option<&Path>,
    scope: Option<&bulkstat::ScanScope>,
) -> (Option<SizeInfo>, FootprintCoverage) {
    tree_footprint_cancellable(path, scope, &current_cancellation())
}

fn scan_inventory_tree(path: &Path) -> bulkstat::DirScan {
    bulkstat::scan_dir_with_cancellation(path, 0, &current_cancellation()).unwrap_or_default()
}

fn tree_footprint_cancellable(
    path: Option<&Path>,
    scope: Option<&bulkstat::ScanScope>,
    cancellation: &bulkstat::ScanCancellation,
) -> (Option<SizeInfo>, FootprintCoverage) {
    let mut coverage = FootprintCoverage::new(FootprintBasis::Tree);
    if cancellation.is_cancelled() {
        coverage.inaccessible = 1;
        return (None, coverage);
    }
    let Some(path) = path else {
        coverage.missing_files = 1;
        return (None, coverage);
    };
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => {}
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            coverage.inaccessible = 1;
            return (None, coverage);
        }
        _ => {
            coverage.missing_files = 1;
            return (None, coverage);
        }
    }
    let scan = match scope {
        Some(scope) => bulkstat::scan_dir_in_scope(path, 0, cancellation, Some(scope)),
        None => bulkstat::scan_dir_with_cancellation(path, 0, cancellation),
    };
    if let Some(scan) = scan {
        coverage.add_scan(&scan);
        (Some(scan.size), coverage)
    } else {
        coverage.inaccessible = 1;
        (None, coverage)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Manager {
    Brew,
    BrewCask,
    Npm,
    Pip,
    Cargo,
    Bun,
}

impl Manager {
    pub fn key(self) -> &'static str {
        match self {
            Self::Brew => "brew",
            Self::BrewCask => "brew-cask",
            Self::Npm => "npm",
            Self::Pip => "pip",
            Self::Cargo => "cargo",
            Self::Bun => "bun",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|manager| manager.key() == key)
    }

    pub fn label(self) -> &'static str {
        match self {
            Manager::Brew => "brew",
            Manager::BrewCask => "brew (cask)",
            Manager::Npm => "npm (global)",
            Manager::Pip => "pip",
            Manager::Cargo => "cargo",
            Manager::Bun => "bun (global)",
        }
    }

    pub fn command(self) -> &'static str {
        match self {
            Manager::Brew | Manager::BrewCask => "brew",
            Manager::Npm => "npm",
            Manager::Pip => "pip3",
            Manager::Cargo => "cargo",
            Manager::Bun => "bun",
        }
    }

    pub const ALL: &[Manager] = &[
        Manager::Brew,
        Manager::BrewCask,
        Manager::Npm,
        Manager::Pip,
        Manager::Cargo,
        Manager::Bun,
    ];

    pub fn is_global_leaf_manager(self) -> bool {
        matches!(
            self,
            Manager::BrewCask | Manager::Npm | Manager::Cargo | Manager::Bun
        )
    }

    pub fn uninstall_args(self, name: &str) -> (&'static str, Vec<String>) {
        match self {
            Manager::Brew => ("brew", vec!["uninstall".into(), name.into()]),
            Manager::BrewCask => (
                "brew",
                vec!["uninstall".into(), "--cask".into(), name.into()],
            ),
            Manager::Npm => ("npm", vec!["uninstall".into(), "-g".into(), name.into()]),
            Manager::Pip => ("pip3", vec!["uninstall".into(), "-y".into(), name.into()]),
            Manager::Cargo => ("cargo", vec!["uninstall".into(), name.into()]),
            Manager::Bun => ("bun", vec!["remove".into(), "-g".into(), name.into()]),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepEvidence {
    ManagerGraph,
    PartialGraph,
    Untracked,
}

impl DepEvidence {
    pub fn label(self) -> &'static str {
        match self {
            Self::ManagerGraph => "manager_graph",
            Self::PartialGraph => "partial_graph",
            Self::Untracked => "untracked",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageUseStatus {
    DependencyLeaf,
    RequiredByDependents,
    Untracked,
}

#[derive(Clone, Debug)]
pub struct DepInfo {
    pub dependencies: Vec<String>,
    pub dependents: Vec<String>,
    pub evidence: DepEvidence,
}

impl Default for DepInfo {
    fn default() -> Self {
        Self {
            dependencies: Vec::new(),
            dependents: Vec::new(),
            evidence: DepEvidence::Untracked,
        }
    }
}

impl DepInfo {
    fn tracked(dependencies: Vec<String>, dependents: Vec<String>) -> Self {
        Self {
            dependencies,
            dependents,
            evidence: DepEvidence::ManagerGraph,
        }
    }

    pub fn is_dependency_leaf(&self) -> bool {
        self.evidence == DepEvidence::ManagerGraph && self.dependents.is_empty()
    }
}

#[derive(Clone, Debug, Default)]
pub struct DepGraph {
    entries: HashMap<(Manager, String), DepInfo>,
    warnings: Vec<GraphWarning>,
}

#[derive(Clone, Debug)]
pub struct GraphWarning {
    pub manager: Manager,
    pub message: String,
}

impl DepGraph {
    pub fn get(&self, manager: Manager, name: &str) -> Option<&DepInfo> {
        self.entries.get(&(manager, name.to_string()))
    }

    pub fn warnings(&self) -> &[GraphWarning] {
        &self.warnings
    }

    pub fn warning_for(&self, manager: Manager) -> Option<&str> {
        self.warnings
            .iter()
            .find(|warning| warning.manager == manager)
            .map(|warning| warning.message.as_str())
    }

    #[cfg(test)]
    pub(crate) fn from_entries(entries: Vec<(Manager, &str, DepInfo)>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|(manager, name, info)| ((manager, name.to_string()), info))
                .collect(),
            warnings: Vec::new(),
        }
    }

    pub fn use_status(&self, manager: Manager, name: &str) -> PackageUseStatus {
        match self.entries.get(&(manager, name.to_string())) {
            Some(info)
                if info.evidence == DepEvidence::ManagerGraph && info.dependents.is_empty() =>
            {
                PackageUseStatus::DependencyLeaf
            }
            Some(info) if info.evidence == DepEvidence::ManagerGraph => {
                PackageUseStatus::RequiredByDependents
            }
            _ => PackageUseStatus::Untracked,
        }
    }

    pub fn dependency_leaf_count(&self) -> usize {
        self.entries
            .values()
            .filter(|info| info.is_dependency_leaf())
            .count()
    }
}

#[derive(Clone, Debug)]
pub struct ManagerReport {
    pub manager: Manager,
    pub packages: Vec<Package>,
    pub total_size: SizeInfo,
    pub available: bool,
    pub warning: Option<String>,
    pub state: ManagerScanState,
    pub environment: Option<ManagerEnvironment>,
}

#[derive(Clone, Debug)]
pub struct ManagerEnvironment {
    pub executable: PathBuf,
    executable_target: PathBuf,
    pub prefix_args: Vec<String>,
    pub install_root: Option<PathBuf>,
    pub package_roots: Vec<PathBuf>,
    cwd: PathBuf,
    manager: Manager,
    search_path: OsString,
}

impl ManagerEnvironment {
    fn discover(manager: Manager) -> Result<Option<Self>, String> {
        let paths = std::env::var_os("PATH").unwrap_or_default();
        Self::discover_with_path(manager, &paths)
    }

    fn discover_with_path(manager: Manager, paths: &OsStr) -> Result<Option<Self>, String> {
        let home = std::env::var_os("HOME").and_then(|home| Path::new(&home).canonicalize().ok());
        let euid = unsafe { libc::geteuid() };
        let search_path = trusted_search_path(paths, home.as_deref(), euid);
        let resolve = |name| command_path_from(name, &search_path, home.as_deref(), euid);
        let mut prefix_args = Vec::new();
        let mut executable = resolve(manager.command());
        if executable.is_none() && manager == Manager::Pip {
            if let Some((entry, target)) = resolve("python3") {
                let mut probe = Command::new(&target);
                probe.arg0(&entry);
                probe
                    .args(["-I", "-m", "pip", "--version"])
                    .env("PATH", &search_path)
                    .current_dir("/");
                let output = run_scan_process(probe, CMD_TIMEOUT);
                if output.success && !output.stdout.trim().is_empty() {
                    executable = Some((entry, target));
                    prefix_args = vec!["-I".into(), "-m".into(), "pip".into()];
                }
            }
        }
        let Some((executable, executable_target)) = executable else {
            let names: &[&str] = if manager == Manager::Pip {
                &["pip3", "python3"]
            } else {
                &[manager.command()]
            };
            for dir in std::env::split_paths(paths) {
                for name in names {
                    let candidate = dir.join(name);
                    if std::fs::symlink_metadata(&candidate).is_ok() && resolve(name).is_none() {
                        return Err(format!(
                            "skipped untrusted package-manager path {}",
                            candidate.display()
                        ));
                    }
                }
            }
            return Ok(None);
        };
        Ok(Some(Self {
            executable_target,
            executable,
            prefix_args,
            install_root: None,
            package_roots: Vec::new(),
            // Global inventory should not load per-project configuration from the launch directory.
            cwd: PathBuf::from("/"),
            manager,
            search_path,
        }))
    }

    fn command(&self, args: &[&str]) -> Command {
        // A venv or multicall tool selects its environment by invocation name.
        // Execute the verified target directly while retaining that name.
        let mut command = Command::new(&self.executable_target);
        command.arg0(&self.executable);
        command
            .args(&self.prefix_args)
            .args(args)
            .current_dir(&self.cwd);
        command.env("NO_COLOR", "1").env("PATH", &self.search_path);
        if self.manager == Manager::Pip {
            // Verbose pip version checks can append index diagnostics to JSON stdout.
            command.env("PIP_DISABLE_PIP_VERSION_CHECK", "1");
        }
        if matches!(self.manager, Manager::Brew | Manager::BrewCask) {
            command.env("HOMEBREW_NO_AUTO_UPDATE", "1");
        }
        if self.manager == Manager::Bun {
            if let Some(root) = &self.install_root {
                command.env("BUN_INSTALL_GLOBAL_DIR", root);
            }
        }
        command
    }

    pub(crate) fn for_update(
        manager: Manager,
        directory: Option<&Path>,
    ) -> Result<Command, String> {
        let paths = std::env::var_os("PATH").unwrap_or_default();
        let paths = std::env::join_paths(
            directory
                .into_iter()
                .map(Path::to_owned)
                .chain(std::env::split_paths(&paths)),
        )
        .map_err(|error| error.to_string())?;
        let environment = Self::discover_with_path(manager, &paths)?.ok_or_else(|| {
            format!(
                "a trusted {} installation is required to update diskr",
                manager.command()
            )
        })?;
        if directory.is_some_and(|dir| environment.executable != dir.join(manager.command())) {
            return Err(format!(
                "could not locate {} for this installation",
                manager.command()
            ));
        }
        Ok(environment.command(&[]))
    }

    fn run(&self, args: &[&str], timeout: Duration) -> CommandResult {
        run_scan_process(self.command(args), timeout)
    }

    fn query_path(&self, args: &[&str]) -> Result<PathBuf, String> {
        let output = self.run(args, CMD_TIMEOUT);
        if let Some(warning) = cmd_warning(&output, self.manager.label()) {
            return Err(warning);
        }
        let value = output.stdout.trim();
        let path = PathBuf::from(value);
        if value.is_empty() || value.lines().count() != 1 || !path.is_absolute() {
            return Err(format!(
                "{}: installation path unavailable",
                self.manager.label()
            ));
        }
        Ok(path)
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "executable": self.executable.to_string_lossy(),
            "prefix_args": self.prefix_args,
            "install_root": self.install_root.as_ref().map(|path| path.to_string_lossy()),
            "package_roots": self.package_roots.iter().map(|path| path.to_string_lossy()).collect::<Vec<_>>(),
        })
    }
}

impl ManagerReport {
    pub fn unknown_sizes(&self) -> usize {
        self.packages
            .iter()
            .filter(|package| package.size.is_none())
            .count()
    }

    pub fn partial_sizes(&self) -> usize {
        self.packages
            .iter()
            .filter(|package| package.coverage.is_partial())
            .count()
    }

    pub fn limited_sizes(&self) -> usize {
        self.packages
            .iter()
            .filter(|package| package.coverage.basis.is_limited())
            .count()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagerScanState {
    Absent,
    Empty,
    Complete,
    Failed,
    ParseFailed,
    Partial,
}

impl ManagerScanState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Empty => "empty",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::ParseFailed => "parse_failed",
            Self::Partial => "partial",
        }
    }
}

fn manager_scan_state(
    package_count: usize,
    warning: Option<&str>,
    parse_failed: bool,
) -> ManagerScanState {
    if package_count > 0 {
        if warning.is_some() || parse_failed {
            ManagerScanState::Partial
        } else {
            ManagerScanState::Complete
        }
    } else if parse_failed {
        ManagerScanState::ParseFailed
    } else if warning.is_some() {
        ManagerScanState::Failed
    } else {
        ManagerScanState::Empty
    }
}

const CMD_TIMEOUT: Duration = Duration::from_secs(10);
const CMD_TIMEOUT_SLOW: Duration = Duration::from_secs(30);
const UNINSTALL_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectDeps {
    pub path: PathBuf,
    pub manager_label: String,
    pub manifest: String,
    pub dep_count: DependencyCount,
    pub deps_size: Option<SizeInfo>,
    pub deps_dir: Option<PathBuf>,
    pub deps_identity: Option<bulkstat::FileIdentity>,
    pub coverage: FootprintCoverage,
}

pub struct ProjectDepsReport {
    pub projects: Vec<ProjectDeps>,
    pub inaccessible: u32,
    pub scope: bulkstat::DiscoveryCoverage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventoryScope {
    pub managers: Vec<Manager>,
    pub projects: bool,
    pub depth: usize,
    pub excludes: Vec<PathBuf>,
}

impl Default for InventoryScope {
    fn default() -> Self {
        Self {
            managers: Manager::ALL.to_vec(),
            projects: true,
            depth: 5,
            excludes: Vec::new(),
        }
    }
}

impl InventoryScope {
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({"managers":self.managers.iter().map(|manager| manager.key()).collect::<Vec<_>>(),
            "projects":self.projects,"max_depth":self.depth,
            "excludes":self.excludes.iter().map(|path| path.to_string_lossy()).collect::<Vec<_>>(),
            "exclude_identities":self.excludes.iter().map(|path| crate::state::encode_path(path)).collect::<Vec<_>>()})
    }
}

pub fn scan_selected_managers(managers: &[Manager]) -> Vec<ManagerReport> {
    let reports = std::sync::Mutex::new(Vec::new());
    scan_managers_streamed(managers, &bulkstat::ScanCancellation::never(), &|report| {
        reports.lock().unwrap().push(report);
    });
    let mut reports = reports.into_inner().unwrap();
    reports.sort_by_key(|report| {
        Manager::ALL
            .iter()
            .position(|manager| *manager == report.manager)
    });
    reports
}

pub fn scan_managers_streamed(
    managers: &[Manager],
    cancellation: &bulkstat::ScanCancellation,
    emit: &(impl Fn(ManagerReport) + Sync),
) {
    scan_managers_streamed_with(managers, cancellation, emit, &scan_manager);
}

fn scan_managers_streamed_with(
    managers: &[Manager],
    cancellation: &bulkstat::ScanCancellation,
    emit: &(impl Fn(ManagerReport) + Sync),
    runner: &(impl Fn(Manager) -> ManagerReport + Sync),
) {
    thread::scope(|scope| {
        for manager in managers.iter().copied() {
            scope.spawn(move || {
                if cancellation.is_cancelled() {
                    return;
                }
                let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    with_scan_cancellation(cancellation, || runner(manager))
                }))
                .unwrap_or_else(|_| ManagerReport {
                    manager,
                    packages: Vec::new(),
                    total_size: SizeInfo::default(),
                    available: false,
                    warning: Some("scan thread panicked".into()),
                    state: ManagerScanState::Failed,
                    environment: None,
                });
                if !cancellation.is_cancelled() {
                    emit(report);
                }
            });
        }
    });
}

pub fn scan_dep_graph(reports: &[ManagerReport]) -> DepGraph {
    let mut graph = DepGraph::default();

    let brew_names: Vec<String> = reports
        .iter()
        .find(|report| report.manager == Manager::Brew && report.available)
        .map(|report| {
            report
                .packages
                .iter()
                .map(|package| package.name.clone())
                .collect()
        })
        .unwrap_or_default();
    let brew_environment = reports
        .iter()
        .find(|report| report.manager == Manager::Brew)
        .and_then(|report| report.environment.clone());
    let brew_cancellation = current_cancellation();
    let brew_handle = thread::spawn(move || {
        with_scan_cancellation(&brew_cancellation, || {
            if brew_names.is_empty() {
                (HashMap::new(), None)
            } else if let Some(environment) = brew_environment {
                scan_brew_dep_graph(&brew_names, &environment)
            } else {
                (
                    HashMap::new(),
                    Some(String::from(
                        "brew dependency environment unavailable; rescan packages",
                    )),
                )
            }
        })
    });

    let pip_names: Vec<String> = reports
        .iter()
        .find(|r| r.manager == Manager::Pip && r.available)
        .map(|r| r.packages.iter().map(|p| p.name.clone()).collect())
        .unwrap_or_default();
    let pip_environment = reports
        .iter()
        .find(|report| report.manager == Manager::Pip)
        .and_then(|report| report.environment.clone());
    let pip_cancellation = current_cancellation();
    let pip_handle = thread::spawn(move || {
        with_scan_cancellation(&pip_cancellation, || {
            if pip_names.is_empty() {
                (HashMap::new(), None)
            } else if let Some(environment) = pip_environment {
                scan_pip_dep_graph(&pip_names, &environment)
            } else {
                (
                    HashMap::new(),
                    Some(String::from(
                        "pip dependency environment unavailable; rescan packages",
                    )),
                )
            }
        })
    });

    let (brew_deps, brew_warning) = brew_handle.join().unwrap_or_else(|_| {
        (
            HashMap::new(),
            Some(String::from("brew dependency scan panicked")),
        )
    });
    let (pip_deps, pip_warning) = pip_handle.join().unwrap_or_else(|_| {
        (
            HashMap::new(),
            Some(String::from("pip dependency scan panicked")),
        )
    });
    for (manager, warning) in [(Manager::Brew, brew_warning), (Manager::Pip, pip_warning)] {
        if let Some(message) = warning {
            graph.warnings.push(GraphWarning { manager, message });
        }
    }

    for report in reports {
        if !report.available {
            continue;
        }
        for pkg in &report.packages {
            let info = package_dep_info(report.manager, &pkg.name, &brew_deps, &pip_deps);
            graph
                .entries
                .insert((report.manager, pkg.name.clone()), info);
        }
    }

    graph
}

fn package_dep_info(
    manager: Manager,
    name: &str,
    brew_deps: &HashMap<String, DepInfo>,
    pip_deps: &HashMap<String, DepInfo>,
) -> DepInfo {
    match manager {
        Manager::Brew => brew_deps.get(name).cloned().unwrap_or_default(),
        Manager::Pip => pip_deps
            .get(&normalize_dist_name(name))
            .cloned()
            .unwrap_or_default(),
        Manager::BrewCask | Manager::Npm | Manager::Cargo | Manager::Bun => {
            DepInfo::tracked(Vec::new(), Vec::new())
        }
    }
}

fn scan_brew_dep_graph(
    names: &[String],
    environment: &ManagerEnvironment,
) -> (HashMap<String, DepInfo>, Option<String>) {
    let result = environment.run(&["deps", "--installed", "--for-each"], CMD_TIMEOUT);
    brew_graph_from_result(&result, names)
}

fn brew_graph_from_result(
    result: &CommandResult,
    names: &[String],
) -> (HashMap<String, DepInfo>, Option<String>) {
    let mut warning = cmd_warning(result, "brew dependency graph");
    let mut graph = parse_brew_dep_graph(&result.stdout);
    let invalid = result
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter(|line| {
            line.split_once(':')
                .is_none_or(|(name, _)| name.trim().is_empty())
        })
        .count();
    let declared: HashSet<&str> = result
        .stdout
        .lines()
        .filter_map(|line| line.split_once(':').map(|(name, _)| name.trim()))
        .filter(|name| !name.is_empty())
        .collect();
    let missing = names
        .iter()
        .filter(|name| !declared.contains(name.as_str()))
        .count();
    if invalid > 0 || missing > 0 {
        warning = add_warning(
            warning,
            format!("brew dependency graph: {invalid} malformed rows, {missing} installed formulae missing"),
        );
    }
    if warning.is_some() {
        for info in graph.values_mut() {
            info.evidence = DepEvidence::PartialGraph;
        }
    }
    (graph, warning)
}

fn parse_brew_dep_graph(output: &str) -> HashMap<String, DepInfo> {
    let mut forward: HashMap<String, Vec<String>> = HashMap::new();
    let mut reverse: HashMap<String, Vec<String>> = HashMap::new();

    for line in output.lines() {
        let Some((name, deps_str)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let deps: Vec<String> = deps_str
            .split_whitespace()
            .map(String::from)
            .filter(|s| !s.is_empty())
            .collect();
        for dep in &deps {
            reverse.entry(dep.clone()).or_default().push(name.clone());
        }
        forward.insert(name, deps);
    }

    let mut result: HashMap<String, DepInfo> = HashMap::new();
    for (name, deps) in &forward {
        let dependents = reverse.remove(name).unwrap_or_default();
        result.insert(name.clone(), DepInfo::tracked(deps.clone(), dependents));
    }
    for (name, dependents) in reverse {
        result
            .entry(name)
            .and_modify(|info| info.dependents = dependents.clone())
            .or_insert_with(|| DepInfo::tracked(Vec::new(), dependents));
    }
    result
}

fn scan_pip_dep_graph(
    names: &[String],
    environment: &ManagerEnvironment,
) -> (HashMap<String, DepInfo>, Option<String>) {
    let mut args: Vec<&str> = vec!["show"];
    for name in names {
        args.push(name);
    }
    let result = environment.run(&args, CMD_TIMEOUT);
    pip_graph_from_result(&result, names)
}

fn pip_graph_from_result(
    result: &CommandResult,
    names: &[String],
) -> (HashMap<String, DepInfo>, Option<String>) {
    let mut warning = cmd_warning(result, "pip dependency graph");
    let mut graph = parse_pip_show_output(&result.stdout);
    let missing = names
        .iter()
        .filter(|name| !graph.contains_key(&normalize_dist_name(name)))
        .count();
    let incomplete = graph
        .values()
        .filter(|info| info.evidence == DepEvidence::PartialGraph)
        .count();
    if missing > 0 || incomplete > 0 {
        warning = add_warning(
            warning,
            format!("pip dependency graph: {missing} installed distributions missing, {incomplete} incomplete records"),
        );
    }
    if warning.is_some() {
        for info in graph.values_mut() {
            info.evidence = DepEvidence::PartialGraph;
        }
    }
    (graph, warning)
}

fn parse_pip_show_output(output: &str) -> HashMap<String, DepInfo> {
    let mut result = HashMap::new();
    let mut current_name = String::new();
    let mut current_deps = None;
    let mut current_rev = None;

    let flush = |result: &mut HashMap<String, DepInfo>,
                 name: &mut String,
                 deps: &mut Option<Vec<String>>,
                 rev: &mut Option<Vec<String>>| {
        if !name.is_empty() {
            let evidence = if deps.is_some() && rev.is_some() {
                DepEvidence::ManagerGraph
            } else {
                DepEvidence::PartialGraph
            };
            result.insert(
                normalize_dist_name(&std::mem::take(name)),
                DepInfo {
                    dependencies: deps.take().unwrap_or_default(),
                    dependents: rev.take().unwrap_or_default(),
                    evidence,
                },
            );
        } else {
            deps.take();
            rev.take();
        }
    };

    for line in output.lines() {
        if line == "---" {
            flush(
                &mut result,
                &mut current_name,
                &mut current_deps,
                &mut current_rev,
            );
            continue;
        }
        if let Some(name) = line.strip_prefix("Name:") {
            flush(
                &mut result,
                &mut current_name,
                &mut current_deps,
                &mut current_rev,
            );
            current_name = name.trim().to_string();
        } else if let Some(deps) = line.strip_prefix("Requires:") {
            current_deps = Some(
                deps.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            );
        } else if let Some(rev) = line.strip_prefix("Required-by:") {
            current_rev = Some(
                rev.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            );
        }
    }
    flush(
        &mut result,
        &mut current_name,
        &mut current_deps,
        &mut current_rev,
    );
    result
}

pub fn run_uninstall(
    manager: Manager,
    name: &str,
    environment: Option<&ManagerEnvironment>,
) -> Result<String, String> {
    let command = uninstall_command(manager, name, environment)?;
    let result = crate::process::run_process(command, UNINSTALL_TIMEOUT);
    let cmd = manager.label();
    if result.timed_out {
        Err(format!("{cmd} timed out after 5 minutes; installation state may have changed, so rescan before retrying"))
    } else if result.success {
        Ok(result.stdout.trim().to_string())
    } else {
        let stderr = result.stderr.trim();
        Err(if stderr.is_empty() {
            format!("{cmd} exited with an error")
        } else {
            stderr.to_string()
        })
    }
}

fn uninstall_command(
    manager: Manager,
    name: &str,
    environment: Option<&ManagerEnvironment>,
) -> Result<Command, String> {
    let environment = environment
        .filter(|environment| environment.manager == manager)
        .ok_or_else(|| {
            String::from("package environment unavailable; rescan before uninstalling")
        })?;
    let args = if manager == Manager::Pip {
        vec![
            String::from("uninstall"),
            String::from("-y"),
            name.to_string(),
        ]
    } else {
        manager.uninstall_args(name).1
    };
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut command = environment.command(&arg_refs);
    if matches!(manager, Manager::Cargo | Manager::Npm | Manager::Bun) {
        let root = environment.install_root.as_ref().ok_or_else(|| {
            String::from("package install root unavailable; rescan before uninstalling")
        })?;
        match manager {
            Manager::Cargo => {
                command.arg("--root").arg(root);
            }
            Manager::Npm => {
                command.arg("--prefix").arg(root);
            }
            _ => {}
        }
    }
    Ok(command)
}

fn scan_manager(manager: Manager) -> ManagerReport {
    let mut environment = match ManagerEnvironment::discover(manager) {
        Ok(Some(environment)) => environment,
        result => {
            let warning = result.err();
            return ManagerReport {
                manager,
                packages: Vec::new(),
                total_size: SizeInfo::default(),
                available: false,
                state: if warning.is_some() {
                    ManagerScanState::Failed
                } else {
                    ManagerScanState::Absent
                },
                warning,
                environment: None,
            };
        }
    };

    let (packages, warning, parse_failed) = match manager {
        Manager::Brew => {
            let (packages, warning) = scan_brew_formulae(&mut environment);
            (packages, warning, false)
        }
        Manager::BrewCask => scan_brew_casks(&mut environment),
        Manager::Npm => scan_npm_global(&mut environment),
        Manager::Pip => scan_pip(&mut environment),
        Manager::Cargo => scan_cargo(&mut environment),
        Manager::Bun => scan_bun_global(&mut environment),
    };

    let mut total_size = SizeInfo::default();
    for pkg in &packages {
        if let Some(size) = pkg.size {
            total_size.logical = total_size.logical.saturating_add(size.logical);
            total_size.allocated = total_size.allocated.saturating_add(size.allocated);
        }
    }

    let state = manager_scan_state(packages.len(), warning.as_deref(), parse_failed);
    ManagerReport {
        manager,
        packages,
        total_size,
        available: true,
        warning,
        state,
        environment: Some(environment),
    }
}

fn cmd_warning(result: &CommandResult, manager_label: &str) -> Option<String> {
    if result.timed_out {
        return Some(format!("{manager_label}: timed out"));
    }
    if !result.success {
        let detail = result.stderr.lines().next().unwrap_or("").trim();
        if detail.is_empty() {
            return Some(format!("{manager_label}: exited with error"));
        }
        let truncated = terminal_text::truncate_end(detail, 120);
        return Some(format!("{manager_label}: {truncated}"));
    }
    None
}

fn add_warning(existing: Option<String>, detail: impl AsRef<str>) -> Option<String> {
    Some(match existing {
        Some(existing) => format!("{existing}; {}", detail.as_ref()),
        None => detail.as_ref().to_string(),
    })
}

fn scan_brew_formulae(environment: &mut ManagerEnvironment) -> (Vec<Package>, Option<String>) {
    let cellar = match environment.query_path(&["--cellar"]) {
        Ok(path) => path,
        Err(error) => return (Vec::new(), Some(error)),
    };
    environment.install_root = environment.query_path(&["--prefix"]).ok();
    environment.package_roots = vec![cellar.clone()];
    scan_brew_dir(&cellar)
}

fn scan_brew_casks(environment: &mut ManagerEnvironment) -> (Vec<Package>, Option<String>, bool) {
    let caskroom = match environment.query_path(&["--caskroom"]) {
        Ok(path) => path,
        Err(error) => return (Vec::new(), Some(error), false),
    };
    environment.install_root = environment.query_path(&["--prefix"]).ok();
    environment.package_roots = vec![caskroom.clone()];
    let result = environment.run(
        &["info", "--cask", "--json=v2", "--installed"],
        CMD_TIMEOUT_SLOW,
    );
    brew_casks_from_result(&result, &caskroom)
}

fn brew_casks_from_result(
    result: &CommandResult,
    caskroom: &Path,
) -> (Vec<Package>, Option<String>, bool) {
    let mut warning = cmd_warning(result, "brew cask");
    if result.stdout.is_empty() {
        let parse_failed = result.success;
        if parse_failed {
            warning = add_warning(warning, "brew cask: missing JSON inventory");
        }
        return (Vec::new(), warning, parse_failed);
    }
    let parsed: serde_json::Value = match serde_json::from_str(&result.stdout) {
        Ok(parsed) => parsed,
        Err(error) => {
            return (
                Vec::new(),
                add_warning(warning, format!("brew cask: invalid JSON: {error}")),
                true,
            );
        }
    };
    let Some(casks) = parsed.get("casks").and_then(|value| value.as_array()) else {
        return (
            Vec::new(),
            add_warning(warning, "brew cask: missing casks array"),
            true,
        );
    };
    let packages = parse_brew_cask_json(&result.stdout, caskroom);
    let skipped = casks.len().saturating_sub(packages.len());
    if skipped > 0 {
        warning = add_warning(warning, format!("brew cask: {skipped} malformed records"));
    }
    (packages, warning, skipped > 0)
}

fn parse_brew_cask_json(output: &str, caskroom: &Path) -> Vec<Package> {
    let mut app_roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        app_roots.push(home.join("Applications"));
    }
    parse_brew_cask_json_with_app_roots(output, caskroom, &app_roots)
}

fn parse_brew_cask_json_with_app_roots(
    output: &str,
    caskroom: &Path,
    app_roots: &[PathBuf],
) -> Vec<Package> {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(output) else {
        return Vec::new();
    };
    let Some(casks) = parsed.get("casks").and_then(|c| c.as_array()).cloned() else {
        return Vec::new();
    };

    scan_map(casks, |cask| {
        let token = cask
            .get("token")?
            .as_str()
            .filter(|token| !token.is_empty())?
            .to_string();
        let version = cask
            .get("installed")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let mut is_installer_based = false;
        let mut app_names = Vec::new();

        if let Some(artifacts) = cask.get("artifacts").and_then(|a| a.as_array()) {
            for art in artifacts {
                if art.get("pkg").is_some() || art.get("installer").is_some() {
                    is_installer_based = true;
                }
                if let Some(apps) = art.get("app").and_then(|a| a.as_array()) {
                    for app in apps {
                        if let Some(app_str) = app.as_str() {
                            app_names.push(app_str.to_string());
                        }
                    }
                }
            }
        }

        let mut version_display = version;
        if is_installer_based {
            version_display.push_str(" (installer-based)");
        }

        let pkg_path = caskroom.join(&token);
        let mut coverage = FootprintCoverage::new(if is_installer_based {
            FootprintBasis::InstallerMetadata
        } else {
            FootprintBasis::AppAndMetadata
        });
        let mut size = if pkg_path.is_dir() {
            let scan = scan_inventory_tree(&pkg_path);
            coverage.add_scan(&scan);
            scan.size
        } else {
            coverage.missing_files = 1;
            SizeInfo::default()
        };
        let mut primary_app_path = None;

        if !is_installer_based {
            for app_name in &app_names {
                if let Some(app_path) = find_cask_app_path(app_name, app_roots) {
                    let scan = scan_inventory_tree(&app_path);
                    coverage.add_scan(&scan);
                    let app_size = scan.size;
                    size.logical = size.logical.saturating_add(app_size.logical);
                    size.allocated = size.allocated.saturating_add(app_size.allocated);
                    if primary_app_path.is_none() && app_path != pkg_path {
                        primary_app_path = Some(app_path);
                    }
                } else {
                    coverage.missing_files = coverage.missing_files.saturating_add(1);
                }
            }
        }

        let action_path = primary_app_path.unwrap_or_else(|| pkg_path.clone());
        let metadata_path = if action_path != pkg_path {
            Some(pkg_path)
        } else {
            None
        };

        Some(Package {
            name: token,
            version: version_display,
            size: Some(size),
            path: Some(action_path),
            metadata_path,
            coverage,
        })
    })
    .into_iter()
    .flatten()
    .collect()
}

fn find_cask_app_path(app_name: &str, app_roots: &[PathBuf]) -> Option<PathBuf> {
    let app_path = PathBuf::from(app_name);
    if app_path.is_absolute() && app_path.exists() {
        return Some(app_path);
    }
    app_roots
        .iter()
        .map(|root| root.join(app_name))
        .find(|path| path.exists())
}

fn scan_brew_dir(install_dir: &Path) -> (Vec<Package>, Option<String>) {
    let read = match std::fs::read_dir(install_dir) {
        Ok(read) => read,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return (Vec::new(), None),
        Err(error) => {
            return (
                Vec::new(),
                Some(format!("brew cellar cannot be read: {error}")),
            );
        }
    };
    let mut errors = 0usize;
    let mut entries = Vec::new();
    for entry in read {
        let Ok(entry) = entry else {
            errors += 1;
            continue;
        };
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => entries.push((
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            )),
            Ok(_) => {}
            Err(_) => errors += 1,
        }
    }

    let packages = scan_map(entries, |(name, pkg_path)| {
        let version = latest_subdir_name(&pkg_path);
        let scan = scan_inventory_tree(&pkg_path);
        let mut coverage = FootprintCoverage::new(FootprintBasis::Tree);
        coverage.add_scan(&scan);
        let size = Some(scan.size);
        Package {
            name,
            version,
            size,
            path: Some(pkg_path),
            metadata_path: None,
            coverage,
        }
    });
    let warning = (errors > 0).then(|| format!("brew cellar: {errors} entries unreadable"));
    (packages, warning)
}

fn latest_subdir_name(dir: &Path) -> String {
    std::fs::read_dir(dir)
        .ok()
        .and_then(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

fn scan_npm_global(environment: &mut ManagerEnvironment) -> (Vec<Package>, Option<String>, bool) {
    let result = environment.run(&["list", "-g", "--depth=0", "--json"], CMD_TIMEOUT);
    let global_root = environment.query_path(&["root", "-g"]).ok();
    environment.install_root = environment.query_path(&["prefix", "-g"]).ok();
    environment.package_roots = global_root.iter().cloned().collect();
    let (packages, mut warning, parse_failed) =
        npm_inventory_from_result(&result, global_root.as_deref());
    if environment.install_root.is_none() && !packages.is_empty() {
        warning = add_warning(
            warning,
            "npm: install prefix unavailable; uninstall requires a rescan",
        );
    }
    (packages, warning, parse_failed)
}

fn npm_inventory_from_result(
    result: &CommandResult,
    global_root: Option<&Path>,
) -> (Vec<Package>, Option<String>, bool) {
    let mut warning = cmd_warning(result, "npm");
    if result.stdout.is_empty() {
        let parse_failed = result.success;
        if parse_failed {
            warning = add_warning(warning, "npm: missing JSON inventory");
        }
        return (Vec::new(), warning, parse_failed);
    }
    let parsed: serde_json::Value = match serde_json::from_str(&result.stdout) {
        Ok(parsed) => parsed,
        Err(error) => {
            return (
                Vec::new(),
                add_warning(warning, format!("npm: invalid JSON: {error}")),
                true,
            );
        }
    };
    let Some(root) = parsed.as_object() else {
        return (
            Vec::new(),
            add_warning(warning, "npm: invalid JSON root"),
            true,
        );
    };
    if let Some(error) = root.get("error") {
        warning = add_warning(warning, format!("npm inventory error: {error}"));
    }
    let Some(deps) = root.get("dependencies") else {
        return (Vec::new(), warning, false);
    };
    let Some(deps) = deps.as_object() else {
        return (
            Vec::new(),
            add_warning(warning, "npm: invalid dependencies object"),
            true,
        );
    };

    if global_root.is_none() && !deps.is_empty() {
        warning = add_warning(warning, "npm: global package root unresolved");
    }

    let entries: Vec<_> = deps
        .iter()
        .map(|(name, info)| {
            let version = info
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            (name.clone(), version)
        })
        .collect();
    let malformed = deps
        .values()
        .filter(|info| {
            info.get("version")
                .and_then(|value| value.as_str())
                .is_none()
        })
        .count();
    if malformed > 0 {
        warning = add_warning(
            warning,
            format!("npm: {malformed} package versions unavailable or malformed"),
        );
    }

    let packages = scan_map(entries, |(name, version)| {
        let pkg_path = global_root.as_ref().map(|root| root.join(&name));
        let (size, coverage) = tree_footprint(pkg_path.as_deref(), None);
        Package {
            name,
            version,
            size,
            path: pkg_path.filter(|_| size.is_some()),
            metadata_path: None,
            coverage,
        }
    });
    (packages, warning, malformed > 0)
}

fn scan_pip(environment: &mut ManagerEnvironment) -> (Vec<Package>, Option<String>, bool) {
    // Verbose JSON identifies each distribution's actual location, including
    // custom user bases and virtual environments, without guessing roots.
    let result = environment.run(&["list", "--format=json", "--verbose"], CMD_TIMEOUT);
    let site_packages = pip_locations(&result.stdout);
    environment.package_roots = site_packages.clone();
    pip_inventory_from_result(&result, &site_packages)
}

fn pip_inventory_from_result(
    result: &CommandResult,
    site_packages: &[PathBuf],
) -> (Vec<Package>, Option<String>, bool) {
    let mut warning = cmd_warning(result, "pip");
    if result.stdout.is_empty() {
        let parse_failed = result.success;
        if parse_failed {
            warning = add_warning(warning, "pip: missing JSON inventory");
        }
        return (Vec::new(), warning, parse_failed);
    }
    let parsed: serde_json::Value = match serde_json::from_str(&result.stdout) {
        Ok(parsed) => parsed,
        Err(error) => {
            return (
                Vec::new(),
                add_warning(warning, format!("pip: invalid JSON: {error}")),
                true,
            );
        }
    };
    let Some(arr) = parsed.as_array() else {
        return (
            Vec::new(),
            add_warning(warning, "pip: invalid JSON array"),
            true,
        );
    };

    // Global and per-user site-packages for this interpreter; `--user`
    // installs land in the user root and were previously never found.
    if site_packages.is_empty() && !arr.is_empty() {
        warning = add_warning(warning, "pip: site-packages roots unresolved");
    }
    let mut index_errors = 0u32;
    let indexes: Vec<_> = site_packages
        .iter()
        .filter_map(|root| match PipDistIndex::build(root) {
            Ok(index) => {
                index_errors = index_errors.saturating_add(index.entry_errors);
                Some(index)
            }
            Err(_) => {
                index_errors = index_errors.saturating_add(1);
                None
            }
        })
        .collect();

    let entries: Vec<_> = arr
        .iter()
        .filter_map(|entry| {
            let name = entry
                .get("name")?
                .as_str()
                .filter(|name| !name.is_empty())?
                .to_string();
            let version = entry
                .get("version")?
                .as_str()
                .filter(|version| !version.is_empty())?
                .to_string();
            let location = entry
                .get("location")
                .and_then(|value| value.as_str())
                .map(PathBuf::from)
                .filter(|path| path.is_absolute());
            Some((name, version, location))
        })
        .collect();
    let malformed = arr.len().saturating_sub(entries.len());
    if malformed > 0 {
        warning = add_warning(
            warning,
            format!("pip: {malformed} malformed package records"),
        );
    }

    let measured = scan_map(entries, |(name, version, location)| {
        let (size, path, problems) = if let Some(index) = location
            .as_ref()
            .and_then(|location| indexes.iter().find(|index| &index.root == location))
        {
            find_pip_package_size_and_path_in_indexes(std::slice::from_ref(index), &name, &version)
        } else {
            (
                None,
                None,
                PipRecordProblems {
                    unreadable_manifests: 1,
                    ..Default::default()
                },
            )
        };
        (
            Package {
                name,
                version,
                size,
                path,
                metadata_path: None,
                coverage: FootprintCoverage {
                    basis: FootprintBasis::RecordFiles,
                    inaccessible: problems
                        .unreadable_files
                        .saturating_add(problems.unreadable_manifests),
                    missing_files: problems
                        .missing_files
                        .saturating_add(u32::from(size.is_none())),
                    skipped_mounts: 0,
                },
            },
            problems,
        )
    });
    let mut problems = PipRecordProblems::default();
    let packages = measured
        .into_iter()
        .map(|(package, current)| {
            problems.add(current);
            package
        })
        .collect();
    if !problems.is_empty() || index_errors > 0 {
        let message = format!(
            "pip footprint incomplete: {} missing RECORD files, {} unreadable files, {} unreadable manifests, {} site-package enumeration errors",
            problems.missing_files, problems.unreadable_files, problems.unreadable_manifests, index_errors
        );
        warning = add_warning(warning, message);
    }
    (packages, warning, malformed > 0)
}

fn scan_cargo(environment: &mut ManagerEnvironment) -> (Vec<Package>, Option<String>, bool) {
    let root = match cargo_install_root() {
        Ok(root) => root,
        Err(error) => return (Vec::new(), Some(error), false),
    };
    environment.install_root = Some(root.clone());
    environment.package_roots = vec![root.join("bin")];
    let mut command = environment.command(&["install", "--list", "--root"]);
    command.arg(&root);
    let result = run_scan_process(command, CMD_TIMEOUT);
    cargo_inventory_from_result(&result, Some(&root.join("bin")))
}

fn cargo_inventory_from_result(
    result: &CommandResult,
    cargo_bin: Option<&Path>,
) -> (Vec<Package>, Option<String>, bool) {
    let mut warning = cmd_warning(result, "cargo");
    if result.stdout.is_empty() {
        return (Vec::new(), warning, false);
    }
    let output = &result.stdout;

    let cargo_bin = cargo_bin.map(Path::to_path_buf);
    if cargo_bin.is_none() {
        warning = add_warning(warning, "cargo: install bin root unresolved");
    }

    let mut packages = Vec::new();
    let mut current_name = String::new();
    let mut current_version = String::new();
    let mut current_bins: Vec<String> = Vec::new();
    let mut malformed = 0usize;

    let flush = |name: &str,
                 version: &str,
                 bins: &mut Vec<String>,
                 packages: &mut Vec<Package>,
                 cargo_bin: &Option<PathBuf>| {
        if name.is_empty() {
            return;
        }

        let mut size = SizeInfo::default();
        let mut path = None;
        let mut has_bin = false;
        let mut coverage = FootprintCoverage::new(FootprintBasis::KnownBinaries);

        if let Some(bin_dir) = cargo_bin {
            for bin in bins.iter() {
                let bin_path = bin_dir.join(bin);
                match std::fs::symlink_metadata(&bin_path) {
                    Ok(meta) => {
                        if path.is_none() {
                            path = Some(bin_path.clone());
                        }
                        size.logical = size.logical.saturating_add(meta.len());
                        size.allocated = size
                            .allocated
                            .saturating_add(allocated_size_from_metadata(&meta));
                        has_bin = true;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        coverage.missing_files = coverage.missing_files.saturating_add(1)
                    }
                    Err(_) => coverage.inaccessible = coverage.inaccessible.saturating_add(1),
                }
            }
        } else {
            coverage.missing_files = bins.len().max(1).min(u32::MAX as usize) as u32;
        }

        packages.push(Package {
            name: name.to_string(),
            version: version.to_string(),
            size: if has_bin { Some(size) } else { None },
            path,
            metadata_path: None,
            coverage,
        });
    };

    for line in output.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(bin_name) = line
                .split_whitespace()
                .find(|token| !token.is_empty() && !token.starts_with('('))
            {
                current_bins.push(bin_name.to_string());
            }
            continue;
        }

        flush(
            &current_name,
            &current_version,
            &mut current_bins,
            &mut packages,
            &cargo_bin,
        );

        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else {
            continue;
        };
        let Some(version) = parts.next().filter(|version| {
            version.starts_with('v') && line.trim_end().ends_with(':') && version.len() > 1
        }) else {
            malformed += 1;
            current_name.clear();
            current_version.clear();
            current_bins.clear();
            continue;
        };
        current_name = name.to_string();
        current_version = version
            .trim_start_matches('v')
            .trim_end_matches(':')
            .to_string();
        current_bins.clear();
    }

    flush(
        &current_name,
        &current_version,
        &mut current_bins,
        &mut packages,
        &cargo_bin,
    );

    if malformed > 0 {
        warning = add_warning(
            warning,
            format!("cargo: {malformed} malformed install rows"),
        );
    }
    (packages, warning, malformed > 0)
}

fn scan_bun_global(environment: &mut ManagerEnvironment) -> (Vec<Package>, Option<String>, bool) {
    let result = environment.run(&["pm", "ls", "-g"], CMD_TIMEOUT);
    environment.install_root = bun_root_from_output(&result.stdout);
    let package_root = environment
        .install_root
        .as_ref()
        .map(|root| root.join("node_modules"));
    environment.package_roots = package_root.iter().cloned().collect();
    bun_inventory_from_result(&result, package_root.as_deref())
}

fn bun_root_from_output(output: &str) -> Option<PathBuf> {
    output.lines().find_map(|line| {
        let (root, suffix) = line.trim().rsplit_once(" node_modules")?;
        let root = PathBuf::from(root);
        (root.is_absolute() && (suffix.is_empty() || suffix.starts_with(" ("))).then_some(root)
    })
}

fn pip_locations(output: &str) -> Vec<PathBuf> {
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_str(output) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    entries
        .iter()
        .filter_map(|entry| {
            let root = PathBuf::from(entry.get("location")?.as_str()?);
            (root.is_absolute() && seen.insert(root.clone())).then_some(root)
        })
        .collect()
}

fn bun_inventory_from_result(
    result: &CommandResult,
    bun_global: Option<&Path>,
) -> (Vec<Package>, Option<String>, bool) {
    let mut warning = cmd_warning(result, "bun");
    if result.stdout.is_empty() {
        return (Vec::new(), warning, false);
    }

    let mut malformed = 0usize;
    let entries: Vec<_> = result
        .stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || !line.contains('@') || line.contains(" node_modules") {
                return None;
            }
            let cleaned = line.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '@');
            let (name, version) = if let Some(at_pos) = cleaned.rfind('@') {
                if at_pos == 0 {
                    malformed += 1;
                    return None;
                }
                (
                    cleaned[..at_pos].to_string(),
                    cleaned[at_pos + 1..].to_string(),
                )
            } else {
                (cleaned.to_string(), String::new())
            };
            if name.is_empty() || version.is_empty() {
                malformed += 1;
                return None;
            }
            Some((name, version))
        })
        .collect();
    if malformed > 0 {
        warning = add_warning(warning, format!("bun: {malformed} malformed package rows"));
    }
    if bun_global.is_none() && !entries.is_empty() {
        warning = add_warning(warning, "bun: global package root unresolved");
    }

    let packages = scan_map(entries, |(name, version)| {
        let path = bun_global.map(|root| root.join(&name));
        let (size, coverage) = tree_footprint(path.as_deref(), None);
        let path = path.filter(|_| size.is_some());
        Package {
            name,
            version,
            size,
            path,
            metadata_path: None,
            coverage,
        }
    });
    (packages, warning, malformed > 0)
}

#[cfg(test)]
pub fn find_project_deps(root: &Path, max_depth: usize) -> Vec<ProjectDeps> {
    scan_project_deps(root, max_depth).projects
}

#[cfg(test)]
pub fn scan_project_deps(root: &Path, max_depth: usize) -> ProjectDepsReport {
    let options = InventoryScope {
        depth: max_depth,
        ..Default::default()
    };
    scan_project_deps_streamed(
        root,
        &options,
        &bulkstat::ScanCancellation::never(),
        &|_| {},
    )
}

pub fn scan_project_deps_streamed(
    root: &Path,
    options: &InventoryScope,
    cancellation: &bulkstat::ScanCancellation,
    emit: &(impl Fn(ProjectDeps) + Sync),
) -> ProjectDepsReport {
    let results = std::sync::Mutex::new(Vec::new());
    let inaccessible = AtomicU32::new(0);
    let scope = bulkstat::ScanScope::new(root).ok();
    let coverage = std::sync::Mutex::new(bulkstat::DiscoveryCoverage {
        depth_limit: options.depth,
        ..Default::default()
    });
    let walk = ProjectWalk {
        root,
        manifest_root: manifest::open_scan_root(root),
        options,
        cancellation,
        emit,
        results: &results,
        inaccessible: &inaccessible,
        scope: scope.as_ref(),
        coverage: &coverage,
    };
    pool::par_drain(vec![(root.to_path_buf(), 0usize)], |(dir, depth), queue| {
        scan_project_dir(&dir, depth, queue, &walk);
    });
    let mut results = results.into_inner().unwrap_or_default();
    results.sort_by(|a, b| {
        let a_size = a.deps_size.map(|s| s.allocated).unwrap_or(0);
        let b_size = b.deps_size.map(|s| s.allocated).unwrap_or(0);
        b_size
            .cmp(&a_size)
            .then(a.path.cmp(&b.path))
            .then(a.manifest.cmp(&b.manifest))
    });
    ProjectDepsReport {
        projects: results,
        inaccessible: inaccessible.load(Ordering::Relaxed),
        scope: coverage.into_inner().unwrap_or_default(),
    }
}

const PROJECT_MANIFESTS: &[(&str, &str, &str)] = &[
    ("package.json", "npm/bun/yarn", "node_modules"),
    ("Cargo.toml", "cargo", "target"),
    ("requirements.txt", "pip", ".venv"),
    ("pyproject.toml", "pip/uv", ".venv"),
    ("go.mod", "go", ""),
    ("Gemfile", "bundler", "vendor/bundle"),
    ("composer.json", "composer", "vendor"),
];

#[derive(Debug)]
struct ProjectManifestGroup {
    deps_path: Option<PathBuf>,
    manager_labels: Vec<&'static str>,
    manifests: Vec<&'static str>,
}

impl ProjectManifestGroup {
    fn new(
        deps_path: Option<PathBuf>,
        manager_label: &'static str,
        manifest: &'static str,
    ) -> Self {
        Self {
            deps_path,
            manager_labels: vec![manager_label],
            manifests: vec![manifest],
        }
    }

    fn add(&mut self, manager_label: &'static str, manifest: &'static str) {
        if !self.manager_labels.contains(&manager_label) {
            self.manager_labels.push(manager_label);
        }
        self.manifests.push(manifest);
    }
}

struct ProjectWalk<'a> {
    manifest_root: io::Result<std::fs::File>,
    root: &'a Path,
    options: &'a InventoryScope,
    cancellation: &'a bulkstat::ScanCancellation,
    emit: &'a (dyn Fn(ProjectDeps) + Sync),
    results: &'a std::sync::Mutex<Vec<ProjectDeps>>,
    inaccessible: &'a AtomicU32,
    scope: Option<&'a bulkstat::ScanScope>,
    coverage: &'a std::sync::Mutex<bulkstat::DiscoveryCoverage>,
}

fn scan_project_dir(
    dir: &Path,
    depth: usize,
    queue: &pool::WorkQueue<(PathBuf, usize)>,
    walk: &ProjectWalk<'_>,
) {
    let ProjectWalk {
        options,
        cancellation,
        emit,
        results,
        inaccessible,
        scope,
        coverage,
        ..
    } = walk;
    if cancellation.is_cancelled() {
        return;
    }
    if options
        .excludes
        .iter()
        .any(|excluded| dir.starts_with(walk.root.join(excluded)))
    {
        coverage.lock().unwrap().excluded_policy += 1;
        return;
    }
    if let (Some(policy), Ok(meta)) = (scope, std::fs::symlink_metadata(dir)) {
        if !policy.accepts(dir, &meta) {
            coverage.lock().unwrap().skipped_mounts += 1;
            return;
        }
    }
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            inaccessible.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    let mut children = Vec::new();
    let mut found_manifests: Vec<(usize, &str, &str, &str)> = Vec::new();

    for entry in read {
        if cancellation.is_cancelled() {
            return;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                inaccessible.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                inaccessible.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };

        if file_type.is_file() {
            for (index, (manifest, mgr, deps_dir)) in PROJECT_MANIFESTS.iter().enumerate() {
                if name_str == *manifest {
                    found_manifests.push((index, *manifest, *mgr, *deps_dir));
                }
            }
        } else if file_type.is_dir()
            && !name_str.starts_with('.')
            && name_str != "node_modules"
            && name_str != "target"
            && name_str != "vendor"
        {
            children.push(entry.path());
        } else if file_type.is_dir() {
            coverage.lock().unwrap().excluded_policy += 1;
        }
    }

    found_manifests.sort_by_key(|(index, _, _, _)| *index);
    let mut groups: Vec<ProjectManifestGroup> = Vec::new();
    for (_, manifest, mgr, deps_dir_name) in found_manifests {
        let deps_path = if deps_dir_name.is_empty() {
            None
        } else {
            Some(dir.join(deps_dir_name))
        };
        if let Some(group) = groups.iter_mut().find(|group| group.deps_path == deps_path) {
            group.add(mgr, manifest);
        } else {
            groups.push(ProjectManifestGroup::new(deps_path, mgr, manifest));
        }
    }

    let new_deps = scan_map(groups, |group| {
        let dep_count = manifest::count(
            &walk.manifest_root,
            dir.strip_prefix(walk.root)
                .expect("walked directory under scan root"),
            &group.manifests,
            cancellation,
        );
        let deps_identity = group.deps_path.as_deref().and_then(bulkstat::path_identity);
        let (deps_size, deps_dir, coverage) = match group.deps_path {
            Some(deps_path) => {
                let (size, coverage) = tree_footprint_cancellable(
                    Some(&deps_path),
                    scope.map(|scope| scope.fresh()).as_ref(),
                    cancellation,
                );
                (size, size.map(|_| deps_path), coverage)
            }
            None => (None, None, FootprintCoverage::default()),
        };
        ProjectDeps {
            path: dir.to_path_buf(),
            manager_label: group.manager_labels.join(", "),
            manifest: group.manifests.join(", "),
            dep_count,
            deps_size,
            deps_identity,
            deps_dir,
            coverage,
        }
    });

    if cancellation.is_cancelled() {
        return;
    }
    if !new_deps.is_empty() {
        for project in &new_deps {
            emit(project.clone());
        }
        results.lock().unwrap().extend(new_deps);
    }

    if depth < options.depth {
        for child in children {
            queue.push((child, depth + 1));
        }
    } else {
        coverage.lock().unwrap().depth_limited += children.len() as u32;
    }
}

fn cargo_install_root() -> Result<PathBuf, String> {
    cargo_install_root_with(
        &std::env::current_dir().map_err(|error| error.to_string())?,
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        std::env::var_os("CARGO_HOME").map(PathBuf::from).as_deref(),
        std::env::var_os("CARGO_INSTALL_ROOT")
            .map(PathBuf::from)
            .as_deref(),
    )
}

fn absolute_from(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn cargo_install_root_with(
    cwd: &Path,
    home: Option<&Path>,
    cargo_home: Option<&Path>,
    install_root: Option<&Path>,
) -> Result<PathBuf, String> {
    if let Some(root) = install_root.filter(|root| !root.as_os_str().is_empty()) {
        return Ok(absolute_from(root, cwd));
    }
    let cargo_home = cargo_home
        .filter(|root| !root.as_os_str().is_empty())
        .map(|root| absolute_from(root, cwd))
        .or_else(|| home.map(|home| home.join(".cargo")))
        .ok_or_else(|| String::from("cargo: install root unavailable"))?;
    let legacy = cargo_home.join("config");
    let config = match std::fs::symlink_metadata(&legacy) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => cargo_home.join("config.toml"),
        _ => legacy,
    };
    let root = cargo_config_install_root(&config, &mut HashSet::new(), 0)?;
    Ok(root.unwrap_or(cargo_home))
}

fn cargo_config_install_root(
    path: &Path,
    visiting: &mut HashSet<PathBuf>,
    depth: usize,
) -> Result<Option<PathBuf>, String> {
    if depth > 8 || !visiting.insert(path.to_path_buf()) {
        return Err(String::from(
            "cargo: cyclic or excessively nested config includes",
        ));
    }
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            visiting.remove(path);
            return Ok(None);
        }
        Err(error) => return Err(format!("cargo config unavailable: {error}")),
        _ => {}
    }
    let text = bounded_text(path).map_err(|error| format!("cargo config unavailable: {error}"))?;
    let config = text
        .parse::<toml::Table>()
        .map_err(|_| String::from("cargo config is malformed TOML"))?;
    let mut root = None;
    if let Some(includes) = config.get("include") {
        let includes = includes
            .as_array()
            .ok_or_else(|| String::from("cargo config include must be an array"))?;
        for include in includes {
            let include_path = include
                .as_str()
                .or_else(|| include.get("path").and_then(toml::Value::as_str))
                .ok_or_else(|| String::from("cargo config include has no path"))?;
            let include_path = absolute_from(
                Path::new(include_path),
                path.parent().unwrap_or(Path::new(".")),
            );
            let optional = include
                .get("optional")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false);
            if !optional
                && std::fs::symlink_metadata(&include_path)
                    .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
            {
                return Err(String::from("cargo config required include is missing"));
            }
            if let Some(included_root) =
                cargo_config_install_root(&include_path, visiting, depth + 1)?
            {
                root = Some(included_root);
            }
        }
    }
    if let Some(value) = config.get("install").and_then(|value| value.get("root")) {
        let value = value
            .as_str()
            .ok_or_else(|| String::from("cargo install.root must be a path string"))?;
        let base = path
            .parent()
            .and_then(Path::parent)
            .unwrap_or(Path::new("."));
        root = Some(absolute_from(Path::new(value), base));
    }
    visiting.remove(path);
    Ok(root)
}

// A checkout normally has the same owner and modes as an installed tool. Require
// installation provenance as well as safe permissions before inheriting a directory.
fn installation_bin(dir: &Path, home: Option<&Path>) -> bool {
    if [
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/Library/Developer/CommandLineTools/usr/bin",
        "/Applications/Xcode.app/Contents/Developer/usr/bin",
    ]
    .iter()
    .any(|root| dir == Path::new(root))
    {
        return true;
    }
    fn parts(relative: &Path) -> Option<Vec<&str>> {
        Some(relative.to_str()?.split('/').collect())
    }
    for prefix in ["/usr/local", "/opt/homebrew"] {
        if let Ok(relative) = dir.strip_prefix(prefix) {
            return parts(relative).is_some_and(|elements| {
                matches!(
                    elements.as_slice(),
                    ["bin" | "sbin"]
                        | ["Homebrew", "bin"]
                        | ["Cellar", _, _, "bin" | "sbin"]
                        | ["Cellar", _, _, "libexec", "bin"]
                        | [
                            "Cellar",
                            _,
                            _,
                            "Frameworks",
                            "Python.framework",
                            "Versions",
                            _,
                            "bin"
                        ]
                )
            });
        }
    }
    if let Ok(relative) = dir.strip_prefix("/Library/Frameworks/Python.framework/Versions") {
        return parts(relative).is_some_and(|elements| matches!(elements.as_slice(), [_, "bin"]));
    }
    home.and_then(|home| dir.strip_prefix(home).ok())
        .and_then(parts)
        .is_some_and(|elements| {
            matches!(
                elements.as_slice(),
                [".local" | ".cargo" | ".bun" | ".volta", "bin"]
                    | [".pyenv" | ".asdf", "shims"]
                    | [".virtualenvs", _, "bin"]
                    | ["Library", "Python", _, "bin"]
                    | [".pyenv", "versions", _, "bin"]
                    | [".pyenv", "versions", _, "envs", _, "bin"]
                    | [".rustup", "toolchains", _, "bin"]
                    | [".nvm", "versions", "node", _, "bin"]
                    | [".asdf", "installs", _, _, "bin"]
                    | [".local", "share", "mise", "shims"]
                    | [".local", "share", "mise", "installs", _, _, "bin"]
                    | [".local", "share", "virtualenvs", _, "bin"]
                    | [".local", "share", "pipx", "venvs", _, "bin"]
                    | [".local", "share", "uv", "tools" | "python", _, "bin"]
            )
        })
}

// npm installs its launcher in a package directory and links it into bin.
fn installed_executable(path: &Path, home: Option<&Path>) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if installation_bin(parent, home) {
        return true;
    }
    let suffix = Path::new("lib/node_modules/npm/bin");
    parent.ends_with(suffix)
        && parent
            .ancestors()
            .nth(suffix.components().count())
            .is_some_and(|prefix| installation_bin(&prefix.join("bin"), home))
}

fn trusted_search_path(paths: &OsStr, home: Option<&Path>, euid: u32) -> OsString {
    let directories = std::env::split_paths(paths)
        .filter(|dir| dir.is_absolute())
        .filter_map(|dir| dir.canonicalize().ok())
        .filter(|dir| installation_bin(dir, home) && is_trusted_command_directory(dir, euid));
    std::env::join_paths(directories).unwrap_or_default()
}

fn command_path_from(
    cmd: &str,
    paths: &OsStr,
    home: Option<&Path>,
    euid: u32,
) -> Option<(PathBuf, PathBuf)> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(paths)
        .filter(|dir| dir.is_absolute())
        .find_map(|dir| {
            let entry = dir.join(cmd);
            let target = entry.canonicalize().ok()?;
            let meta = std::fs::metadata(&target).ok()?;
            (installed_executable(&target, home)
                && meta.is_file()
                && meta.permissions().mode() & 0o111 != 0
                && trusted_command_metadata(&meta, &target, euid, false)
                && is_trusted_command_directory(target.parent()?, euid))
            .then_some((entry, target))
        })
}

fn is_trusted_command_directory(path: &Path, euid: u32) -> bool {
    path.ancestors().all(|ancestor| {
        std::fs::metadata(ancestor).is_ok_and(|meta| {
            meta.is_dir() && trusted_command_metadata(&meta, ancestor, euid, ancestor != path)
        })
    })
}

fn trusted_command_metadata(
    meta: &std::fs::Metadata,
    path: &Path,
    euid: u32,
    ancestor: bool,
) -> bool {
    use std::os::unix::fs::MetadataExt;
    if meta.uid() != 0 && meta.uid() != euid {
        return false;
    }
    let mode = meta.mode();
    // Homebrew's standard prefixes can grant write access to macOS administrators (gid 80).
    let brew_admin =
        meta.gid() == 80 && (path.starts_with("/opt/homebrew") || path.starts_with("/usr/local"));
    let mode_trusted = (mode & 0o002 == 0 && (mode & 0o020 == 0 || brew_admin))
        || (ancestor && meta.is_dir() && meta.uid() == 0 && mode & 0o1000 != 0);
    mode_trusted && acl_allows_safe_access(path)
}

// macOS extended ACL grants can make a mode-0755 path writable. Deny entries
// (including the standard home-directory deny-delete entry) grant no rights.
fn acl_allows_safe_access(path: &Path) -> bool {
    use std::ffi::{c_int, c_void, CString};
    use std::os::unix::ffi::OsStrExt;
    use std::ptr;

    unsafe extern "C" {
        fn acl_get_file(path: *const libc::c_char, kind: c_int) -> *mut c_void;
        fn acl_get_entry(acl: *mut c_void, id: c_int, entry: *mut *mut c_void) -> c_int;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut c_int) -> c_int;
        fn acl_get_permset_mask_np(entry: *mut c_void, perms: *mut u64) -> c_int;
        fn acl_free(acl: *mut c_void) -> c_int;
    }
    // sys/acl.h: write data, delete, append, delete child, write attributes,
    // write extended attributes, write security, and change owner.
    const MUTATION_RIGHTS: u64 =
        (1 << 2) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 8) | (1 << 10) | (1 << 12) | (1 << 13);
    const ACL_TYPE_EXTENDED: c_int = 0x100;
    const ACL_EXTENDED_ALLOW: c_int = 1;
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let acl = unsafe { acl_get_file(path.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        // Darwin returns ENOENT for a file with no ACL.
        return matches!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ENOENT | libc::ENOTSUP)
        );
    }
    let mut id = 0; // ACL_FIRST_ENTRY
    let safe = loop {
        let mut entry = ptr::null_mut();
        if unsafe { acl_get_entry(acl, id, &mut entry) } != 0 {
            // EINVAL denotes the end of the ACL on Darwin.
            break io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL);
        }
        let mut tag = 0;
        let mut perms = 0;
        if unsafe { acl_get_tag_type(entry, &mut tag) } != 0
            || unsafe { acl_get_permset_mask_np(entry, &mut perms) } != 0
            || (tag == ACL_EXTENDED_ALLOW && perms & MUTATION_RIGHTS != 0)
        {
            break false;
        }
        id = -1; // ACL_NEXT_ENTRY
    };
    unsafe { acl_free(acl) };
    safe
}

fn run_scan_process(command: Command, timeout: Duration) -> CommandResult {
    let cancellation = current_cancellation();
    crate::process::run_process_cancellable(command, timeout, || cancellation.is_cancelled())
}

struct PipDistIndex {
    root: PathBuf,
    by_name: HashMap<String, Vec<(String, PathBuf)>>,
    entry_errors: u32,
}

impl PipDistIndex {
    fn build(root: &Path) -> io::Result<Self> {
        let mut by_name: HashMap<String, Vec<(String, PathBuf)>> = HashMap::new();
        let mut entry_errors = 0u32;
        for entry in std::fs::read_dir(root)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    entry_errors = entry_errors.saturating_add(1);
                    continue;
                }
            };
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => {}
                Ok(_) => continue,
                Err(_) => {
                    entry_errors = entry_errors.saturating_add(1);
                    continue;
                }
            }
            let file_name = entry.file_name();
            let Some(stem) = file_name
                .to_str()
                .and_then(|name| name.strip_suffix(".dist-info"))
            else {
                continue;
            };
            let (name, version) = stem
                .rsplit_once('-')
                .filter(|(_, version)| version.starts_with(|ch: char| ch.is_ascii_digit()))
                .unwrap_or((stem, ""));
            by_name
                .entry(normalize_dist_name(name))
                .or_default()
                .push((version.to_string(), entry.path()));
        }
        for candidates in by_name.values_mut() {
            candidates.sort_by(|a, b| a.1.cmp(&b.1));
        }
        Ok(Self {
            root: root.to_path_buf(),
            by_name,
            entry_errors,
        })
    }

    fn dist_info(&self, package: &str, version: &str) -> Option<&Path> {
        let candidates = self.by_name.get(&normalize_dist_name(package))?;
        candidates
            .iter()
            .find(|(candidate, _)| candidate == version)
            .or_else(|| {
                if version.is_empty() || candidates.len() == 1 && candidates[0].0.is_empty() {
                    candidates.first()
                } else {
                    None
                }
            })
            .map(|(_, path)| path.as_path())
    }
}

/// Search indexed roots in interpreter order and only size a matching version.
fn find_pip_package_size_and_path_in_indexes(
    indexes: &[PipDistIndex],
    package: &str,
    version: &str,
) -> (Option<SizeInfo>, Option<PathBuf>, PipRecordProblems) {
    let mut fallback_path = None;
    let mut all_problems = PipRecordProblems::default();
    for index in indexes {
        let (size, path, problems) = find_pip_package_size_and_path(index, package, version);
        all_problems.add(problems);
        if size.is_some() {
            return (size, path, all_problems);
        }
        if fallback_path.is_none() {
            fallback_path = path;
        }
    }
    (None, fallback_path, all_problems)
}

fn find_pip_package_size_and_path(
    index: &PipDistIndex,
    package: &str,
    version: &str,
) -> (Option<SizeInfo>, Option<PathBuf>, PipRecordProblems) {
    let site_packages = &index.root;
    let Some(dist_info_path) = index.dist_info(package, version) else {
        return (
            None,
            infer_pip_package_dir(site_packages, package),
            PipRecordProblems::default(),
        );
    };

    let (size, mut problems) = parse_pip_record_size(site_packages, dist_info_path);
    let path = match find_pip_top_level_path(site_packages, dist_info_path) {
        Ok(path) => path,
        // This file is optional in installed distributions.
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => {
            problems.unreadable_manifests = problems.unreadable_manifests.saturating_add(1);
            None
        }
    }
    .or_else(|| infer_pip_package_dir(site_packages, package));

    (size, path, problems)
}

fn infer_pip_package_dir(site_packages: &Path, package: &str) -> Option<PathBuf> {
    let direct = site_packages.join(package);
    if direct.is_dir() {
        return Some(direct);
    }
    let underscored = site_packages.join(package.replace('-', "_"));
    if underscored.is_dir() {
        return Some(underscored);
    }
    None
}

#[cfg(test)]
fn find_pip_dist_info(site_packages: &Path, package: &str) -> Option<PathBuf> {
    PipDistIndex::build(site_packages)
        .ok()?
        .dist_info(package, "")
        .map(Path::to_path_buf)
}

const MAX_PIP_RECORD_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PIP_TOP_LEVEL_BYTES: u64 = 64 * 1024;
const MAX_PIP_METADATA_ROWS: usize = 65_536;
const MAX_PIP_ROW_BYTES: u64 = 16 * 1024;

fn read_pip_metadata_rows(
    path: &Path,
    max_bytes: u64,
    mut visit: impl FnMut(&str) -> io::Result<()>,
) -> io::Result<()> {
    use std::io::{BufRead, BufReader, Read};

    let limit_error = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "pip metadata exceeds parser limit",
        )
    };
    let file = manifest::open_regular(path)?;
    if file.metadata()?.len() > max_bytes {
        return Err(limit_error());
    }
    let cancellation = current_cancellation();
    let mut reader = BufReader::new(file.take(max_bytes + 1));
    let mut total = 0;
    let mut row = String::new();
    let mut rows = 0;
    loop {
        if cancellation.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "scan cancelled"));
        }
        row.clear();
        let bytes = reader
            .by_ref()
            .take(MAX_PIP_ROW_BYTES + 1)
            .read_line(&mut row)? as u64;
        if bytes == 0 {
            return Ok(());
        }
        total += bytes;
        rows += 1;
        if bytes > MAX_PIP_ROW_BYTES || total > max_bytes || rows > MAX_PIP_METADATA_ROWS {
            return Err(limit_error());
        }
        visit(row.trim_end_matches(['\r', '\n']))?;
    }
}

fn find_pip_top_level_path(site_packages: &Path, dist_info: &Path) -> io::Result<Option<PathBuf>> {
    let mut path = None;
    read_pip_metadata_rows(
        &dist_info.join("top_level.txt"),
        MAX_PIP_TOP_LEVEL_BYTES,
        |row| {
            let name = row.trim();
            if path.is_none() && Path::new(name).file_name() == Some(OsStr::new(name)) {
                let candidate = site_packages.join(name);
                if candidate.exists() {
                    path = Some(candidate);
                }
            }
            Ok(())
        },
    )?;
    Ok(path)
}

#[derive(Clone, Copy, Default)]
struct PipRecordProblems {
    missing_files: u32,
    unreadable_files: u32,
    unreadable_manifests: u32,
}

impl PipRecordProblems {
    fn add(&mut self, other: Self) {
        self.missing_files = self.missing_files.saturating_add(other.missing_files);
        self.unreadable_files = self.unreadable_files.saturating_add(other.unreadable_files);
        self.unreadable_manifests = self
            .unreadable_manifests
            .saturating_add(other.unreadable_manifests);
    }

    fn is_empty(self) -> bool {
        self.missing_files == 0 && self.unreadable_files == 0 && self.unreadable_manifests == 0
    }
}

fn parse_pip_record_size(
    site_packages: &Path,
    dist_info: &Path,
) -> (Option<SizeInfo>, PipRecordProblems) {
    let mut size = SizeInfo::default();
    let mut problems = PipRecordProblems::default();
    let result = read_pip_metadata_rows(&dist_info.join("RECORD"), MAX_PIP_RECORD_BYTES, |row| {
        if row.trim().is_empty() {
            return Ok(());
        }
        let path = pip_record_path(row)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid pip RECORD row"))?;
        let full_path = site_packages.join(path);
        match std::fs::symlink_metadata(&full_path) {
            Ok(meta) => {
                size.logical = size.logical.saturating_add(meta.len());
                size.allocated = size
                    .allocated
                    .saturating_add(allocated_size_from_metadata(&meta));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                problems.missing_files = problems.missing_files.saturating_add(1);
            }
            Err(_) => {
                problems.unreadable_files = problems.unreadable_files.saturating_add(1);
            }
        }
        Ok(())
    });
    if result.is_err() {
        problems.unreadable_manifests = 1;
        return (None, problems);
    }
    (Some(size), problems)
}

// RECORD has three CSV fields. Only the path is needed for live filesystem sizes.
fn pip_record_path(row: &str) -> Option<String> {
    let mut path = String::new();
    let mut field = 0;
    let mut quoted = false;
    let mut chars = row.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.next_if_eq(&'"').is_some() => {
                if field == 0 {
                    path.push('"');
                }
            }
            '"' => quoted = !quoted,
            ',' if !quoted => {
                field += 1;
                if field > 2 {
                    return None;
                }
            }
            _ if field == 0 => path.push(ch),
            _ => {}
        }
    }
    (field == 2 && !quoted && !path.is_empty()).then_some(path)
}

fn normalize_dist_name(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in name.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch == '-' || ch == '_' || ch == '.' {
            if !last_dash && !out.is_empty() {
                out.push('-');
            }
            last_dash = true;
        } else {
            out.push(ch);
            last_dash = false;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn allocated_size_from_metadata(meta: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;

    #[cfg(unix)]
    {
        meta.blocks().saturating_mul(512)
    }
    #[cfg(not(unix))]
    {
        meta.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn executable_lookup_honors_macos_extended_access_grants() {
        let root = test_root("manager_acl");
        let bin = root.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let home = root.canonicalize().unwrap();
        let euid = unsafe { libc::geteuid() };
        // Use a harmless deny entry and a write grant on this disposable tree only.
        let set_acl = |action, entry| {
            assert!(Command::new("/bin/chmod")
                .args([action, entry])
                .arg(&bin)
                .status()
                .unwrap()
                .success());
        };
        set_acl("+a", "everyone deny delete");
        assert!(!trusted_search_path(bin.as_os_str(), Some(&home), euid).is_empty());
        set_acl("+a", "everyone allow add_file");
        assert!(trusted_search_path(bin.as_os_str(), Some(&home), euid).is_empty());
        set_acl("-a", "everyone deny delete");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manager_resolution_rejects_writable_and_accepts_safe_absolute_paths() {
        use std::os::unix::fs::PermissionsExt;

        let root = test_root("trusted_manager_path");
        let home = root.join("home");
        let tools = home.join(".virtualenvs/runtime/bin");
        std::fs::create_dir_all(&tools).unwrap();
        let home = home.canonicalize().unwrap();
        let tools = tools.canonicalize().unwrap();
        let base = home.join(".pyenv/versions/base/bin");
        std::fs::create_dir_all(&base).unwrap();
        let executable = base.join("python");
        std::fs::write(&executable, "fixture").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let entry = tools.join("python3");
        std::os::unix::fs::symlink(&executable, &entry).unwrap();
        let euid = unsafe { libc::geteuid() };
        let path = tools.as_os_str();

        std::fs::set_permissions(&tools, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(trusted_search_path(path, Some(&home), euid).is_empty());

        std::fs::set_permissions(&tools, std::fs::Permissions::from_mode(0o700)).unwrap();
        let safe = trusted_search_path(path, Some(&home), euid);
        assert_eq!(
            command_path_from("python3", &safe, Some(&home), euid),
            Some((entry, executable.clone()))
        );

        let nested = home.join(".local/bin/checkout/bin");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(trusted_search_path(nested.as_os_str(), Some(&home), euid).is_empty());
        let project = root.join("project/bin");
        std::fs::create_dir_all(&project).unwrap();
        assert!(trusted_search_path(project.as_os_str(), Some(&home), euid).is_empty());
        std::fs::copy(&executable, project.join("python")).unwrap();
        std::os::unix::fs::symlink(project.join("python"), tools.join("cargo")).unwrap();
        assert_eq!(command_path_from("cargo", &safe, Some(&home), euid), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manager_stream_emits_ready_reports_without_waiting_for_a_slow_manager() {
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = std::sync::Mutex::new(release_rx);
        let (tx, rx) = std::sync::mpsc::channel();
        thread::scope(|scope| {
            let worker = scope.spawn(|| {
                scan_managers_streamed_with(
                    &[Manager::Brew, Manager::Npm],
                    &bulkstat::ScanCancellation::never(),
                    &|report| {
                        tx.send(report.manager).unwrap();
                    },
                    &|manager| {
                        if manager == Manager::Brew {
                            release_rx
                                .lock()
                                .unwrap()
                                .recv_timeout(Duration::from_secs(5))
                                .unwrap();
                        }
                        ManagerReport {
                            manager,
                            packages: Vec::new(),
                            total_size: Default::default(),
                            available: false,
                            warning: None,
                            state: ManagerScanState::Absent,
                            environment: None,
                        }
                    },
                )
            });
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(5)).unwrap(),
                Manager::Npm
            );
            release_tx.send(()).unwrap();
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(5)).unwrap(),
                Manager::Brew
            );
            worker.join().unwrap();
        });
    }

    #[test]
    fn project_stream_honors_depth_exclusions_and_cancellation() {
        let root = test_root("project_scope_stream");
        for relative in ["", "keep", "keep/deep", "archive"] {
            let dir = root.join(relative);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("Cargo.toml"), "[package]\nname='fixture'\n").unwrap();
        }
        let options = InventoryScope {
            depth: 1,
            excludes: vec![PathBuf::from("archive")],
            ..Default::default()
        };
        let emitted = std::sync::Mutex::new(Vec::new());
        let report = scan_project_deps_streamed(
            &root,
            &options,
            &bulkstat::ScanCancellation::never(),
            &|project| {
                emitted.lock().unwrap().push(project);
            },
        );
        assert_eq!(report.projects.len(), 2);
        assert_eq!(emitted.into_inner().unwrap().len(), 2);
        assert_eq!(report.scope.depth_limited, 1);
        assert_eq!(report.scope.excluded_policy, 1);
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
        let report = scan_project_deps_streamed(
            &root,
            &options,
            &bulkstat::ScanCancellation::new(generation, 0),
            &|_| panic!("cancelled scan emitted a project"),
        );
        assert!(report.projects.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_context_cancels_commands_and_propagates_to_sizing_workers() {
        let root = test_root("command_cancel");
        std::fs::create_dir_all(&root).unwrap();
        let ready = root.join("ready");
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let cancellation = bulkstat::ScanCancellation::new(std::sync::Arc::clone(&generation), 0);
        let worker_cancellation = cancellation.clone();
        let worker_ready = ready.clone();
        let worker = thread::spawn(move || {
            with_scan_cancellation(&worker_cancellation, || {
                let mut command = Command::new("/bin/sh");
                command
                    .args(["-c", "printf ready > \"$1\"; exec /bin/sleep 30", "fixture"])
                    .arg(worker_ready);
                run_scan_process(command, Duration::from_secs(10))
            })
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(ready.exists());
        generation.fetch_add(1, Ordering::Relaxed);
        let result = worker.join().unwrap();
        assert!(!result.success);
        assert!(!result.timed_out);
        assert!(result.stderr.contains("cancelled"));
        let cancelled = with_scan_cancellation(&cancellation, || {
            scan_map(vec![1, 2, 3, 4], |_| current_cancellation().is_cancelled())
        });
        assert!(cancelled.into_iter().all(|cancelled| cancelled));
        assert!(!current_cancellation().is_cancelled());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dependency_graph_workers_cancel_their_processes() {
        use std::os::unix::fs::PermissionsExt;

        let root = test_root("graph_cancel");
        std::fs::create_dir_all(&root).unwrap();
        let mut reports = Vec::new();
        for manager in [Manager::Brew, Manager::Pip] {
            let executable = root.join(manager.key());
            std::fs::write(
                &executable,
                "#!/bin/sh\nprintf ready > \"$0.ready\"\nexec /bin/sleep 30\n",
            )
            .unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
            reports.push(ManagerReport {
                manager,
                packages: vec![Package {
                    name: String::from("fixture"),
                    version: String::from("1.0"),
                    size: None,
                    path: None,
                    metadata_path: None,
                    coverage: FootprintCoverage::default(),
                }],
                total_size: SizeInfo::default(),
                available: true,
                warning: None,
                state: ManagerScanState::Complete,
                environment: Some(ManagerEnvironment {
                    executable_target: executable.clone(),
                    executable,
                    prefix_args: Vec::new(),
                    install_root: None,
                    package_roots: Vec::new(),
                    cwd: root.clone(),
                    manager,
                    search_path: OsString::from("/usr/bin:/bin"),
                }),
            });
        }
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let cancellation = bulkstat::ScanCancellation::new(std::sync::Arc::clone(&generation), 0);
        let worker = thread::spawn(move || {
            with_scan_cancellation(&cancellation, || scan_dep_graph(&reports))
        });
        let ready = [root.join("brew.ready"), root.join("pip.ready")];
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.iter().all(|path| path.exists()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(ready.iter().all(|path| path.exists()));
        let cancelled_at = Instant::now();
        generation.fetch_add(1, Ordering::Relaxed);
        let graph = worker.join().unwrap();
        assert!(cancelled_at.elapsed() < Duration::from_secs(2));
        assert_eq!(graph.warnings.len(), 2);
        assert!(graph
            .warnings
            .iter()
            .all(|warning| warning.message.contains("cancelled")));
        assert!(graph
            .entries
            .values()
            .all(|entry| !entry.is_dependency_leaf()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn uninstall_does_not_inherit_inventory_cancellation() {
        let environment = ManagerEnvironment {
            executable_target: PathBuf::from("/bin/sh"),
            executable: PathBuf::from("/bin/sh"),
            prefix_args: vec![
                String::from("-c"),
                String::from("printf 'fixture completed'"),
                String::from("fixture"),
            ],
            install_root: None,
            package_roots: Vec::new(),
            cwd: std::env::temp_dir(),
            manager: Manager::Pip,
            search_path: OsString::from("/usr/bin:/bin"),
        };
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
        let cancellation = bulkstat::ScanCancellation::new(generation, 0);
        let result = with_scan_cancellation(&cancellation, || {
            run_uninstall(Manager::Pip, "fixture", Some(&environment))
        });
        assert_eq!(result.unwrap(), "fixture completed");
    }

    #[test]
    fn pip_json_inventory_disables_verbose_version_check_diagnostics() {
        use std::os::unix::fs::PermissionsExt;

        let root = test_root("pip_version_check");
        std::fs::create_dir_all(root.join("fixture-1.0.dist-info")).unwrap();
        std::fs::write(root.join("fixture.py"), b"fixture").unwrap();
        std::fs::write(root.join("fixture-1.0.dist-info/RECORD"), "fixture.py,,\n").unwrap();
        let executable = root.join("pip3");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
[ "$*" = 'list --format=json --verbose' ] || exit 2
/bin/cat "$0.json"
if [ "$PIP_DISABLE_PIP_VERSION_CHECK" != 1 ]; then
  printf '\n  Link requires a different Python: fixture\n'
fi
"#,
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            root.join("pip3.json"),
            serde_json::json!([{"name":"fixture", "version":"1.0", "location":root}]).to_string(),
        )
        .unwrap();
        let mut environment = ManagerEnvironment {
            executable_target: executable.clone(),
            executable,
            prefix_args: Vec::new(),
            install_root: None,
            package_roots: Vec::new(),
            cwd: root.clone(),
            manager: Manager::Pip,
            search_path: OsString::from("/usr/bin:/bin"),
        };
        let mut unconfigured = environment.command(&["list", "--format=json", "--verbose"]);
        unconfigured.env_remove("PIP_DISABLE_PIP_VERSION_CHECK");
        let contaminated = crate::process::run_process(unconfigured, CMD_TIMEOUT);
        assert!(contaminated.success);
        assert!(contaminated
            .stdout
            .contains("Link requires a different Python"));
        let (_, warning, parse_failed) = pip_inventory_from_result(&contaminated, &[]);
        assert!(parse_failed);
        assert!(warning.unwrap().contains("invalid JSON"));

        let (packages, warning, parse_failed) = scan_pip(&mut environment);
        assert!(!parse_failed, "{warning:?}");
        assert!(warning.is_none(), "{warning:?}");
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "fixture");
        assert_eq!(packages[0].size.unwrap().logical, 7);
        assert_eq!(environment.package_roots, vec![root.clone()]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cargo_install_root_follows_environment_config_and_home_precedence() {
        let root = test_root("cargo_root_precedence");
        let home = root.join("home");
        let cargo_home = root.join("custom-cargo");
        std::fs::create_dir_all(&cargo_home).unwrap();
        std::fs::write(
            cargo_home.join("config.toml"),
            "[install]\nroot = 'configured-tools'\n",
        )
        .unwrap();
        assert_eq!(
            cargo_install_root_with(&root, Some(&home), Some(&cargo_home), None).unwrap(),
            root.join("configured-tools")
        );
        assert_eq!(
            cargo_install_root_with(
                &root,
                Some(&home),
                Some(&cargo_home),
                Some(Path::new("explicit-tools"))
            )
            .unwrap(),
            root.join("explicit-tools")
        );
        std::fs::write(
            cargo_home.join("config"),
            "[install]\nroot = 'legacy-tools'\n",
        )
        .unwrap();
        assert_eq!(
            cargo_install_root_with(&root, Some(&home), Some(&cargo_home), None).unwrap(),
            root.join("legacy-tools")
        );
        assert_eq!(
            cargo_install_root_with(&root, Some(&home), None, None).unwrap(),
            home.join(".cargo")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cargo_config_includes_preserve_root_origin_and_reject_cycles() {
        let root = test_root("cargo_root_includes");
        let cargo_home = root.join("cargo-home");
        std::fs::create_dir_all(&cargo_home).unwrap();
        std::fs::write(
            cargo_home.join("config.toml"),
            "include = ['included.toml']\n",
        )
        .unwrap();
        std::fs::write(
            cargo_home.join("included.toml"),
            "[install]\nroot = 'included-tools'\n",
        )
        .unwrap();
        assert_eq!(
            cargo_install_root_with(&root, None, Some(&cargo_home), None).unwrap(),
            root.join("included-tools")
        );
        std::fs::write(
            cargo_home.join("included.toml"),
            "include = ['config.toml']\n",
        )
        .unwrap();
        assert!(cargo_install_root_with(&root, None, Some(&cargo_home), None).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pip_inventory_sizes_only_the_reported_distribution_location() {
        let root = test_root("pip_location_identity");
        let first = root.join("first/site-packages");
        let selected = root.join("custom-user-base/site-packages");
        for (site, bytes) in [(&first, 4), (&selected, 40)] {
            std::fs::create_dir_all(site.join("example-1.0.dist-info")).unwrap();
            std::fs::write(site.join("example.py"), vec![b'x'; bytes]).unwrap();
            std::fs::write(site.join("example-1.0.dist-info/RECORD"), "example.py,,\n").unwrap();
        }
        let result = CommandResult {
            stdout: serde_json::json!([{"name":"example","version":"1.0","location":selected}])
                .to_string(),
            stderr: String::new(),
            success: true,
            timed_out: false,
        };
        assert_eq!(pip_locations(&result.stdout), vec![selected.clone()]);
        let (packages, _, _) = pip_inventory_from_result(&result, &[first, selected]);
        assert_eq!(packages[0].size.unwrap().logical, 40);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn uninstall_commands_retain_the_scanned_manager_and_root() {
        let environment = |manager, executable: &str, root: &str| ManagerEnvironment {
            executable_target: PathBuf::from(executable),
            executable: PathBuf::from(executable),
            prefix_args: Vec::new(),
            install_root: Some(PathBuf::from(root)),
            package_roots: Vec::new(),
            cwd: PathBuf::from("/fixture"),
            manager,
            search_path: OsString::from("/usr/bin:/bin"),
        };
        let cargo = environment(
            Manager::Cargo,
            "/fixture/custom/bin/cargo",
            "/fixture/custom install",
        );
        let command = uninstall_command(Manager::Cargo, "tool", Some(&cargo)).unwrap();
        assert_eq!(command.get_program(), "/fixture/custom/bin/cargo");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["uninstall", "tool", "--root", "/fixture/custom install"]
        );
        let bun = environment(Manager::Bun, "/fixture/bun", "/fixture/bun global");
        let command = uninstall_command(Manager::Bun, "@scope/tool", Some(&bun)).unwrap();
        assert!(command
            .get_envs()
            .any(|(key, value)| key == "BUN_INSTALL_GLOBAL_DIR"
                && value == Some(std::ffi::OsStr::new("/fixture/bun global"))));
        let mut pip = environment(Manager::Pip, "/fixture/venv/bin/python3", "/fixture/venv");
        pip.prefix_args = vec![String::from("-I"), String::from("-m"), String::from("pip")];
        let command = uninstall_command(Manager::Pip, "example", Some(&pip)).unwrap();
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["-I", "-m", "pip", "uninstall", "-y", "example"]
        );
        for prefix in ["/opt/homebrew", "/usr/local"] {
            let brew = environment(Manager::Brew, &format!("{prefix}/bin/brew"), prefix);
            assert_eq!(
                uninstall_command(Manager::Brew, "tool", Some(&brew))
                    .unwrap()
                    .get_program(),
                std::ffi::OsStr::new(&format!("{prefix}/bin/brew"))
            );
        }
        assert!(uninstall_command(Manager::Cargo, "tool", None).is_err());
        assert!(uninstall_command(Manager::Cargo, "tool", Some(&pip)).is_err());
    }

    #[test]
    fn bun_inventory_retains_the_effective_global_root_with_spaces() {
        assert_eq!(
            bun_root_from_output(
                "/fixture/alternate bun/global node_modules (2 installed)\n├── one@1.0\n"
            ),
            Some(PathBuf::from("/fixture/alternate bun/global"))
        );
        assert!(bun_root_from_output("unexpected format\n").is_none());
        let result = CommandResult {
            stdout: String::from(
                "/fixture/user@example/bun global node_modules (1 installed)\n└── one@1.0\n",
            ),
            stderr: String::new(),
            success: true,
            timed_out: false,
        };
        let (packages, _, _) = bun_inventory_from_result(&result, None);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "one");
    }

    #[test]
    fn manager_labels_are_distinct() {
        let labels: Vec<_> = Manager::ALL.iter().map(|m| m.label()).collect();
        let mut unique = labels.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(labels.len(), unique.len());
    }

    #[test]
    fn manager_uninstall_args_use_native_manager_commands() {
        let (cmd, args) = Manager::Brew.uninstall_args("ripgrep");
        assert_eq!(cmd, "brew");
        assert_eq!(args, vec!["uninstall", "ripgrep"]);

        let (cmd, args) = Manager::Pip.uninstall_args("ruff");
        assert_eq!(cmd, "pip3");
        assert_eq!(args, vec!["uninstall", "-y", "ruff"]);

        let (cmd, args) = Manager::Npm.uninstall_args("typescript");
        assert_eq!(cmd, "npm");
        assert_eq!(args, vec!["uninstall", "-g", "typescript"]);
    }

    #[test]
    fn normalizes_pip_distribution_names() {
        assert_eq!(normalize_dist_name("PyYAML"), "pyyaml");
        assert_eq!(normalize_dist_name("ruff"), "ruff");
        assert_eq!(
            normalize_dist_name("typing_extensions"),
            "typing-extensions"
        );
    }

    #[test]
    fn finds_pip_dist_info_and_top_level_path() {
        let root = test_root("pip_dist_info");
        let dist_info = root.join("PyYAML-6.0.1.dist-info");
        std::fs::create_dir_all(&dist_info).unwrap();
        let top_level = dist_info.join("top_level.txt");
        std::fs::write(&top_level, "yaml\n").unwrap();
        std::fs::create_dir_all(root.join("yaml")).unwrap();
        std::fs::write(root.join("yaml").join("__init__.py"), b"").unwrap();

        assert_eq!(find_pip_dist_info(&root, "PyYAML"), Some(dist_info.clone()));
        assert_eq!(
            find_pip_top_level_path(&root, &dist_info).unwrap(),
            Some(root.join("yaml"))
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn dist_info_match_requires_version_digit_after_name() {
        let root = test_root("pip_dist_info_prefix");
        std::fs::create_dir_all(root.join("sentry-2.0.dist-info")).unwrap();
        std::fs::create_dir_all(root.join("sentry_sdk-2.0.dist-info")).unwrap();

        // `sentry` must match its own dist-info, never the `sentry-sdk` family.
        assert_eq!(
            find_pip_dist_info(&root, "sentry"),
            Some(root.join("sentry-2.0.dist-info"))
        );
        assert_eq!(
            find_pip_dist_info(&root, "sentry-sdk"),
            Some(root.join("sentry_sdk-2.0.dist-info"))
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn finds_package_size_across_multiple_site_packages_roots() {
        let root = test_root("pip_multi_root");
        let global = root.join("global");
        let user = root.join("user");
        let dist_info = user.join("flit-3.9.0.dist-info");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&dist_info).unwrap();
        std::fs::create_dir_all(user.join("flit")).unwrap();
        std::fs::write(user.join("flit").join("__init__.py"), b"x").unwrap();
        std::fs::write(dist_info.join("RECORD"), "flit/__init__.py,sha256=abc,1\n").unwrap();

        // Package is only in the user root; the global root is searched first
        // but the size must still be found in the user root.
        let indexes = [
            PipDistIndex::build(&global).unwrap(),
            PipDistIndex::build(&user).unwrap(),
        ];
        let (size, path, problems) =
            find_pip_package_size_and_path_in_indexes(&indexes, "flit", "3.9.0");
        assert!(size.is_some());
        assert_eq!(path, Some(user.join("flit")));
        assert!(problems.is_empty());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pip_index_matches_exact_version_and_normalized_name() {
        let root = test_root("pip_index_version");
        std::fs::create_dir_all(root.join("typing_extensions-4.0.dist-info")).unwrap();
        std::fs::create_dir_all(root.join("typing_extensions-5.0.dist-info")).unwrap();
        let index = PipDistIndex::build(&root).unwrap();
        assert_eq!(
            index.dist_info("typing-extensions", "5.0"),
            Some(root.join("typing_extensions-5.0.dist-info").as_path())
        );
        assert!(index.dist_info("typing-extensions", "6.0").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "manual site-packages scaling benchmark"]
    fn benchmark_pip_dist_index_sizes() {
        for count in [100, 1_000, 5_000] {
            let root = test_root(&format!("pip_index_bench_{count}"));
            std::fs::create_dir_all(&root).unwrap();
            for n in 0..count {
                std::fs::create_dir(root.join(format!("package{n}-1.0.dist-info"))).unwrap();
            }
            let started = Instant::now();
            let index = PipDistIndex::build(&root).unwrap();
            let build_time = started.elapsed();
            let started = Instant::now();
            for n in 0..count {
                assert!(index.dist_info(&format!("package{n}"), "1.0").is_some());
            }
            let lookup_time = started.elapsed();
            let repeated_started = Instant::now();
            for n in 0..count {
                let expected = format!("package{n}-1.0.dist-info");
                assert!(std::fs::read_dir(&root)
                    .unwrap()
                    .any(|entry| entry.unwrap().file_name() == expected.as_str()));
            }
            eprintln!(
                "pip index: {count} distributions, one enumeration in {build_time:?}, {count} lookups in {lookup_time:?}; repeated enumeration in {:?}",
                repeated_started.elapsed()
            );
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn pip_record_requires_three_fields_and_preserves_quoted_paths() {
        assert!(pip_record_path("path,to,script.py,sha256=abc,12").is_none());
        assert_eq!(
            pip_record_path(r#""hello,""world"".py",sha256=abc,7"#).as_deref(),
            Some(r#"hello,"world".py"#)
        );
    }

    #[test]
    fn pip_metadata_limits_and_cancellation_preserve_incomplete_coverage() {
        let root = test_root("pip_metadata_limits");
        let dist = root.join("sample-1.0.dist-info");
        std::fs::create_dir_all(&dist).unwrap();
        let record = dist.join("RECORD");
        let top = dist.join("top_level.txt");
        let index = PipDistIndex::build(&root).unwrap();

        std::fs::File::create(&top)
            .unwrap()
            .set_len(MAX_PIP_TOP_LEVEL_BYTES + 1)
            .unwrap();
        std::fs::File::create(&record)
            .unwrap()
            .set_len(MAX_PIP_RECORD_BYTES + 1)
            .unwrap();
        let (size, _, problems) = find_pip_package_size_and_path(&index, "sample", "1.0");
        assert!(size.is_none());
        assert_eq!(problems.unreadable_manifests, 2);

        for contents in [
            vec![b'x'; MAX_PIP_ROW_BYTES as usize + 1],
            vec![b'\n'; MAX_PIP_METADATA_ROWS + 1],
        ] {
            std::fs::write(&record, contents).unwrap();
            assert_eq!(
                parse_pip_record_size(&root, &dist).1.unreadable_manifests,
                1
            );
        }
        std::fs::write(&record, b"package.py,,\n").unwrap();
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
        with_scan_cancellation(&bulkstat::ScanCancellation::new(generation, 0), || {
            let error = read_pip_metadata_rows(&record, MAX_PIP_RECORD_BYTES, |_| {
                panic!("cancelled scan read a row")
            })
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        });

        std::fs::remove_file(&record).unwrap();
        std::os::unix::fs::symlink(&top, &record).unwrap();
        assert!(manifest::open_regular(&record).is_err());
        std::fs::remove_file(&record).unwrap();
        std::fs::create_dir(&record).unwrap();
        assert_eq!(
            parse_pip_record_size(&root, &dist).1.unreadable_manifests,
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn measures_pip_record_sizes_from_live_files() {
        let root = test_root("pip_record_sizes");
        let dist_info = root.join("requests-2.32.0.dist-info");
        std::fs::create_dir_all(&dist_info).unwrap();
        std::fs::create_dir_all(root.join("requests")).unwrap();
        std::fs::write(
            root.join("requests").join("__init__.py"),
            b"print('requests')",
        )
        .unwrap();
        std::fs::write(
            dist_info.join("RECORD"),
            "requests/__init__.py,sha256=abc,99999\n\
             requests-2.32.0.dist-info/RECORD,sha256=def,\n\
             requests/missing.py,sha256=missing,90000\n",
        )
        .unwrap();

        let (size, problems) = parse_pip_record_size(&root, &dist_info);
        let size = size.unwrap();
        let expected = std::fs::symlink_metadata(root.join("requests/__init__.py"))
            .unwrap()
            .len()
            + std::fs::symlink_metadata(dist_info.join("RECORD"))
                .unwrap()
                .len();
        assert_eq!(size.logical, expected);
        assert!(size.allocated > 0);
        assert_eq!(problems.missing_files, 1);
        assert_eq!(problems.unreadable_files, 0);

        let inventory = CommandResult {
            stdout: serde_json::json!([{"name":"requests","version":"2.32.0","location":root}])
                .to_string(),
            stderr: String::new(),
            success: true,
            timed_out: false,
        };
        let (packages, warning, parse_failed) =
            pip_inventory_from_result(&inventory, std::slice::from_ref(&root));
        assert!(!parse_failed);
        assert!(warning.is_some());
        assert_eq!(packages[0].coverage.basis, FootprintBasis::RecordFiles);
        assert_eq!(packages[0].coverage.missing_files, 1);
        assert_eq!(packages[0].coverage.json()["partial"], true);
        let report = ManagerReport {
            manager: Manager::Pip,
            packages,
            total_size: size,
            available: true,
            warning,
            state: ManagerScanState::Partial,
            environment: None,
        };
        assert_eq!(report.partial_sizes(), 1);
        assert_eq!(report.limited_sizes(), 1);
        assert_eq!(report.unknown_sizes(), 0);

        std::fs::remove_file(dist_info.join("RECORD")).unwrap();
        let (size, problems) = parse_pip_record_size(&root, &dist_info);
        assert!(size.is_none());
        assert_eq!(problems.unreadable_manifests, 1);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn parses_brew_dependency_graph_and_reverse_deps() {
        let graph = parse_brew_dep_graph(
            "a: b c\n\
             b: c\n\
             c:\n\
             app: a\n",
        );

        assert_eq!(graph["a"].dependencies, vec!["b", "c"]);
        assert_eq!(graph["a"].dependents, vec!["app"]);
        assert_eq!(graph["b"].dependencies, vec!["c"]);
        assert_eq!(graph["b"].dependents, vec!["a"]);
        assert_eq!(graph["c"].dependencies, Vec::<String>::new());
        assert_eq!(graph["c"].dependents, vec!["a", "b"]);
    }

    #[test]
    fn parses_pip_show_dependency_fields() {
        let graph = parse_pip_show_output(
            "Name: requests\n\
             Version: 2.32.0\n\
             Requires: certifi, urllib3\n\
             Required-by: my-tool\n\
             ---\n\
             Name: urllib3\n\
             Version: 2.0.0\n\
             Requires:\n\
             Required-by: requests\n",
        );

        assert_eq!(graph["requests"].dependencies, vec!["certifi", "urllib3"]);
        assert_eq!(graph["requests"].dependents, vec!["my-tool"]);
        assert_eq!(graph["urllib3"].dependencies, Vec::<String>::new());
        assert_eq!(graph["urllib3"].dependents, vec!["requests"]);
    }

    #[test]
    fn footprint_coverage_preserves_scanner_uncertainty() {
        let scan = bulkstat::DirScan {
            inaccessible: 2,
            skipped_mounts: 3,
            ..Default::default()
        };
        let mut coverage = FootprintCoverage::new(FootprintBasis::Tree);
        coverage.add_scan(&scan);
        assert!(coverage.is_partial());
        assert_eq!(coverage.json()["inaccessible"], 2);
        assert_eq!(coverage.json()["skipped_mounts"], 3);
        assert!(!coverage.basis.is_limited());
    }

    #[test]
    fn inventory_parsers_distinguish_malformed_empty_and_partial_results() {
        let output = |stdout: &str| CommandResult {
            stdout: stdout.to_string(),
            stderr: String::new(),
            success: true,
            timed_out: false,
        };
        let unused = test_root("inventory_parse_unused");
        for malformed in ["", "{broken", "null"] {
            for (packages, warning, parse_failed) in [
                npm_inventory_from_result(&output(malformed), None),
                pip_inventory_from_result(&output(malformed), &[]),
                brew_casks_from_result(&output(malformed), &unused),
            ] {
                assert!(packages.is_empty());
                assert!(warning.is_some());
                assert!(parse_failed);
            }
        }
        for (packages, warning, parse_failed) in [
            npm_inventory_from_result(&output("{}"), None),
            pip_inventory_from_result(&output("[]"), &[]),
            brew_casks_from_result(&output(r#"{"casks":[]}"#), &unused),
        ] {
            assert!(packages.is_empty());
            assert!(warning.is_none());
            assert!(!parse_failed);
        }
        let (packages, warning, malformed) = pip_inventory_from_result(
            &output(r#"[{"name":"valid","version":"1.0"},{"name":"broken"}]"#),
            &[],
        );
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "valid");
        assert!(malformed);
        assert!(warning.unwrap().contains("1 malformed package records"));
    }

    #[test]
    fn text_inventory_parsers_keep_local_and_scoped_install_identifiers() {
        let output = |stdout: &str| CommandResult {
            stdout: stdout.to_string(),
            stderr: String::new(),
            success: true,
            timed_out: false,
        };
        let (cargo, warning, malformed) = cargo_inventory_from_result(
            &output("local-tool v1.2.3 (/fixture/source):\n    local-tool\nunrecognized\n"),
            None,
        );
        assert_eq!(cargo.len(), 1);
        assert_eq!(cargo[0].name, "local-tool");
        assert_eq!(cargo[0].version, "1.2.3");
        assert_eq!(cargo[0].coverage.basis, FootprintBasis::KnownBinaries);
        assert!(cargo[0].coverage.is_partial());
        assert!(malformed);
        assert!(warning.unwrap().contains("malformed install rows"));
        let (bun, warning, malformed) = bun_inventory_from_result(
            &output("global node_modules\n├── @scope/tool@1.0.0\n└── @broken\n"),
            None,
        );
        assert_eq!(bun.len(), 1);
        assert_eq!(bun[0].name, "@scope/tool");
        assert!(malformed);
        assert!(warning.unwrap().contains("malformed package rows"));
    }

    #[test]
    fn manager_scan_states_distinguish_absent_empty_failure_and_partial() {
        assert_eq!(manager_scan_state(0, None, false), ManagerScanState::Empty);
        assert_eq!(
            manager_scan_state(2, None, false),
            ManagerScanState::Complete
        );
        assert_eq!(
            manager_scan_state(0, Some("command failed"), false),
            ManagerScanState::Failed
        );
        assert_eq!(
            manager_scan_state(0, None, true),
            ManagerScanState::ParseFailed
        );
        assert_eq!(
            manager_scan_state(2, Some("one malformed row"), true),
            ManagerScanState::Partial
        );
        assert_eq!(ManagerScanState::Absent.label(), "absent");
    }

    #[test]
    fn dependency_graph_results_keep_partial_output_and_diagnostics() {
        let brew_result = CommandResult {
            stdout: String::from("alpha: beta\nmalformed row\n"),
            stderr: String::new(),
            success: true,
            timed_out: false,
        };
        let (brew, warning) = brew_graph_from_result(
            &brew_result,
            &[String::from("alpha"), String::from("missing")],
        );
        assert_eq!(brew["alpha"].dependencies, vec!["beta"]);
        let warning = warning.unwrap();
        assert!(warning.contains("1 malformed rows"));
        assert!(warning.contains("1 installed formulae missing"));

        let pip_result = CommandResult {
            stdout: String::from("Name: Requests\nRequires: urllib3\nRequired-by:\n"),
            stderr: String::from("one distribution was not found"),
            success: false,
            timed_out: false,
        };
        let (pip, warning) = pip_graph_from_result(
            &pip_result,
            &[String::from("requests"), String::from("missing")],
        );
        assert_eq!(pip["requests"].dependencies, vec!["urllib3"]);
        assert_eq!(
            package_dep_info(Manager::Pip, "Requests", &HashMap::new(), &pip).evidence,
            DepEvidence::PartialGraph
        );
        assert!(!pip["requests"].is_dependency_leaf());
        assert!(!brew["alpha"].is_dependency_leaf());
        let warning = warning.unwrap();
        assert!(warning.contains("one distribution was not found"));
        assert!(warning.contains("1 installed distributions missing"));
    }

    #[test]
    fn global_cli_managers_are_dependency_leaves() {
        let brew_deps = HashMap::new();
        let pip_deps = HashMap::new();

        for manager in [
            Manager::BrewCask,
            Manager::Npm,
            Manager::Cargo,
            Manager::Bun,
        ] {
            let info = package_dep_info(manager, "tool", &brew_deps, &pip_deps);
            assert_eq!(info.evidence, DepEvidence::ManagerGraph);
            assert!(info.dependencies.is_empty());
            assert!(info.dependents.is_empty());
            assert!(manager.is_global_leaf_manager());
        }
    }

    #[test]
    fn finds_project_deps_in_directory() {
        let root = test_root("project_deps");
        let _ = std::fs::remove_dir_all(&root);
        let proj = root.join("myapp");
        std::fs::create_dir_all(proj.join("node_modules/lodash")).unwrap();
        std::fs::write(
            proj.join("package.json"),
            r#"{"dependencies":{"lodash":"4.0"}}"#,
        )
        .unwrap();
        std::fs::write(
            proj.join("node_modules/lodash/index.js"),
            b"module.exports={}",
        )
        .unwrap();

        let deps = find_project_deps(&root, 3);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].manifest, "package.json");
        assert_eq!(deps[0].dep_count.known, 1);
        assert!(deps[0].deps_size.is_some());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_deps_merge_manifests_that_share_deps_dir() {
        let root = test_root("project_deps_shared_dir");
        let _ = std::fs::remove_dir_all(&root);
        let proj = root.join("pyapp");
        std::fs::create_dir_all(proj.join(".venv/lib")).unwrap();
        std::fs::write(proj.join("requirements.txt"), "requests\nflask\n").unwrap();
        std::fs::write(
            proj.join("pyproject.toml"),
            "[project]\ndependencies = [\"click>=8\", \"requests\"]\n",
        )
        .unwrap();
        std::fs::write(proj.join(".venv/lib/site.py"), b"python deps").unwrap();

        let deps = find_project_deps(&root, 3);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].path, proj);
        assert_eq!(deps[0].manager_label, "pip, pip/uv");
        assert_eq!(deps[0].manifest, "requirements.txt, pyproject.toml");
        assert_eq!(deps[0].dep_count.known, 3);
        assert!(deps[0].dep_count.complete);
        assert_eq!(deps[0].deps_dir.as_ref(), Some(&root.join("pyapp/.venv")));
        assert!(deps[0].deps_size.is_some());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_brew_cask_json_correctly() {
        let json_data = r#"{
            "casks": [
                {
                    "token": "pdfextractor",
                    "installed": "1.5",
                    "artifacts": [
                        {
                            "app": ["PDFExtractor.app"]
                        }
                    ]
                },
                {
                    "token": "packages",
                    "installed": "1.2.10",
                    "artifacts": [
                        {
                            "pkg": ["Install Packages.pkg"]
                        }
                    ]
                }
            ]
        }"#;

        let temp_caskroom = test_root("caskroom");
        let temp_apps = test_root("apps");
        std::fs::create_dir_all(temp_caskroom.join("pdfextractor")).unwrap();
        std::fs::create_dir_all(temp_caskroom.join("packages")).unwrap();
        std::fs::create_dir_all(temp_apps.join("PDFExtractor.app/Contents")).unwrap();
        std::fs::write(temp_apps.join("PDFExtractor.app/Contents/app.bin"), b"app").unwrap();

        let packages = parse_brew_cask_json_with_app_roots(
            json_data,
            &temp_caskroom,
            std::slice::from_ref(&temp_apps),
        );
        assert_eq!(packages.len(), 2);

        let pdf = packages.iter().find(|p| p.name == "pdfextractor").unwrap();
        assert_eq!(pdf.coverage.basis, FootprintBasis::AppAndMetadata);
        assert_eq!(pdf.version, "1.5");
        assert_eq!(
            pdf.path.as_ref().unwrap(),
            &temp_apps.join("PDFExtractor.app")
        );
        assert_eq!(
            pdf.metadata_path.as_ref().unwrap(),
            &temp_caskroom.join("pdfextractor")
        );

        let pkgs = packages.iter().find(|p| p.name == "packages").unwrap();
        assert_eq!(pkgs.coverage.basis, FootprintBasis::InstallerMetadata);
        assert!(pkgs.coverage.basis.is_limited());
        assert_eq!(pkgs.version, "1.2.10 (installer-based)");
        assert_eq!(pkgs.path.as_ref().unwrap(), &temp_caskroom.join("packages"));
        assert!(pkgs.metadata_path.is_none());

        std::fs::remove_dir_all(temp_caskroom).unwrap();
        std::fs::remove_dir_all(temp_apps).unwrap();
    }

    #[test]
    fn cmd_warning_returns_none_on_success() {
        let result = CommandResult {
            stdout: "output".into(),
            stderr: String::new(),
            success: true,
            timed_out: false,
        };
        assert!(cmd_warning(&result, "test").is_none());
    }

    #[test]
    fn cmd_warning_reports_timeout() {
        let result = CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            success: false,
            timed_out: true,
        };
        assert_eq!(cmd_warning(&result, "brew"), Some("brew: timed out".into()));
    }

    #[test]
    fn cmd_warning_includes_first_stderr_line() {
        let result = CommandResult {
            stdout: String::new(),
            stderr: "error: externally-managed-environment\nmore detail".into(),
            success: false,
            timed_out: false,
        };
        let w = cmd_warning(&result, "pip").unwrap();
        assert_eq!(w, "pip: error: externally-managed-environment");
    }

    #[test]
    fn cmd_warning_truncates_long_stderr() {
        let long_line = "x".repeat(200);
        let result = CommandResult {
            stdout: String::new(),
            stderr: long_line,
            success: false,
            timed_out: false,
        };
        let w = cmd_warning(&result, "npm").unwrap();
        assert!(w.ends_with('…'));
        assert!(w.len() < 140);
    }

    #[test]
    fn cmd_warning_never_splits_utf8() {
        let result = CommandResult {
            stdout: String::new(),
            stderr: format!("{}é🙂", "x".repeat(119)),
            success: false,
            timed_out: false,
        };
        let warning = cmd_warning(&result, "pip").unwrap();
        assert!(warning.ends_with('…'));
        assert!(terminal_text::display_width(&warning) <= 125);
    }

    #[test]
    fn scan_manager_propagates_warning_field() {
        let report = ManagerReport {
            manager: Manager::Brew,
            packages: Vec::new(),
            total_size: SizeInfo::default(),
            available: true,
            warning: Some("brew cask: timed out".into()),
            state: ManagerScanState::Failed,
            environment: None,
        };
        assert_eq!(report.warning.as_deref(), Some("brew cask: timed out"));
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("diskr_{name}_{}_{}", std::process::id(), nanos))
    }
}
