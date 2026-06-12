use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use crate::bulkstat::{self, SizeInfo};
use crate::pool;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub size: Option<SizeInfo>,
    pub path: Option<PathBuf>,
    pub metadata_path: Option<PathBuf>,
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
            Manager::Pip => {
                if command_exists("pip3") {
                    ("pip3", vec!["uninstall".into(), "-y".into(), name.into()])
                } else {
                    (
                        "python3",
                        vec![
                            "-m".into(),
                            "pip".into(),
                            "uninstall".into(),
                            "-y".into(),
                            name.into(),
                        ],
                    )
                }
            }
            Manager::Cargo => ("cargo", vec!["uninstall".into(), name.into()]),
            Manager::Bun => ("bun", vec!["remove".into(), "-g".into(), name.into()]),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepEvidence {
    ManagerGraph,
    Untracked,
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
}

impl DepGraph {
    pub fn get(&self, manager: Manager, name: &str) -> Option<&DepInfo> {
        self.entries.get(&(manager, name.to_string()))
    }

    #[cfg(test)]
    pub(crate) fn from_entries(entries: Vec<(Manager, &str, DepInfo)>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|(manager, name, info)| ((manager, name.to_string()), info))
                .collect(),
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectDeps {
    pub path: PathBuf,
    pub manager_label: String,
    pub manifest: String,
    pub dep_count: usize,
    pub deps_size: Option<SizeInfo>,
    pub deps_dir: Option<PathBuf>,
}

pub fn scan_managers() -> Vec<ManagerReport> {
    let mut handles = Vec::with_capacity(Manager::ALL.len());
    for (index, manager) in Manager::ALL.iter().copied().enumerate() {
        handles.push((index, manager, thread::spawn(move || scan_manager(manager))));
    }

    let mut reports = vec![None; Manager::ALL.len()];
    for (index, manager, handle) in handles {
        let report = handle.join().unwrap_or_else(|_| ManagerReport {
            manager,
            packages: Vec::new(),
            total_size: SizeInfo::default(),
            available: false,
        });
        reports[index] = Some(report);
    }

    reports.into_iter().flatten().collect()
}

pub fn scan_dep_graph(reports: &[ManagerReport]) -> DepGraph {
    let mut graph = DepGraph::default();

    let brew_handle = thread::spawn(|| {
        if command_exists("brew") {
            scan_brew_dep_graph()
        } else {
            HashMap::new()
        }
    });

    let pip_names: Vec<String> = reports
        .iter()
        .find(|r| r.manager == Manager::Pip && r.available)
        .map(|r| r.packages.iter().map(|p| p.name.clone()).collect())
        .unwrap_or_default();
    let pip_handle = thread::spawn(move || {
        if pip_names.is_empty() || pip_command().is_none() {
            HashMap::new()
        } else {
            scan_pip_dep_graph(&pip_names)
        }
    });

    let brew_deps = brew_handle.join().unwrap_or_default();
    let pip_deps = pip_handle.join().unwrap_or_default();

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
        Manager::Pip => pip_deps.get(name).cloned().unwrap_or_default(),
        Manager::BrewCask | Manager::Npm | Manager::Cargo | Manager::Bun => {
            DepInfo::tracked(Vec::new(), Vec::new())
        }
    }
}

fn scan_brew_dep_graph() -> HashMap<String, DepInfo> {
    let output = run_command("brew", &["deps", "--installed", "--for-each"]);
    if output.is_empty() {
        return HashMap::new();
    }
    parse_brew_dep_graph(&output)
}

fn parse_brew_dep_graph(output: &str) -> HashMap<String, DepInfo> {
    let mut forward: HashMap<String, Vec<String>> = HashMap::new();
    let mut reverse: HashMap<String, Vec<String>> = HashMap::new();

    for line in output.lines() {
        let Some((name, deps_str)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_string();
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

fn scan_pip_dep_graph(names: &[String]) -> HashMap<String, DepInfo> {
    let mut args: Vec<&str> = vec!["show"];
    for name in names {
        args.push(name);
    }
    let output = run_pip_command(&args);
    if output.is_empty() {
        return HashMap::new();
    }
    parse_pip_show_output(&output)
}

fn parse_pip_show_output(output: &str) -> HashMap<String, DepInfo> {
    let mut result = HashMap::new();
    let mut current_name = String::new();
    let mut current_deps = Vec::new();
    let mut current_rev = Vec::new();

    let flush = |result: &mut HashMap<String, DepInfo>,
                 name: &mut String,
                 deps: &mut Vec<String>,
                 rev: &mut Vec<String>| {
        if !name.is_empty() {
            result.insert(
                std::mem::take(name),
                DepInfo::tracked(std::mem::take(deps), std::mem::take(rev)),
            );
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
        if let Some(name) = line.strip_prefix("Name: ") {
            flush(
                &mut result,
                &mut current_name,
                &mut current_deps,
                &mut current_rev,
            );
            current_name = name.trim().to_string();
        } else if let Some(deps) = line.strip_prefix("Requires: ") {
            current_deps = deps
                .split(", ")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        } else if let Some(rev) = line.strip_prefix("Required-by: ") {
            current_rev = rev
                .split(", ")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
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

pub fn run_uninstall(manager: Manager, name: &str) -> Result<String, String> {
    let (cmd, args) = manager.uninstall_args(name);
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = Command::new(cmd)
        .args(&arg_refs)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("failed to run {cmd}: {e}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("{cmd} exited with {}", output.status)
        } else {
            stderr
        })
    }
}

fn scan_manager(manager: Manager) -> ManagerReport {
    let available = if manager == Manager::Pip {
        pip_command().is_some()
    } else {
        command_exists(manager.command())
    };
    if !available {
        return ManagerReport {
            manager,
            packages: Vec::new(),
            total_size: SizeInfo::default(),
            available: false,
        };
    }

    let packages = match manager {
        Manager::Brew => scan_brew_formulae(),
        Manager::BrewCask => scan_brew_casks(),
        Manager::Npm => scan_npm_global(),
        Manager::Pip => scan_pip(),
        Manager::Cargo => scan_cargo(),
        Manager::Bun => scan_bun_global(),
    };

    let mut total_size = SizeInfo::default();
    for pkg in &packages {
        if let Some(size) = pkg.size {
            total_size.logical = total_size.logical.saturating_add(size.logical);
            total_size.allocated = total_size.allocated.saturating_add(size.allocated);
        }
    }

    ManagerReport {
        manager,
        packages,
        total_size,
        available: true,
    }
}

fn scan_brew_formulae() -> Vec<Package> {
    let cellar = brew_prefix().join("Cellar");
    scan_brew_dir(&cellar)
}

fn scan_brew_casks() -> Vec<Package> {
    let output = run_command("brew", &["info", "--cask", "--json=v2", "--installed"]);
    if output.is_empty() {
        return Vec::new();
    }
    let caskroom = brew_prefix().join("Caskroom");
    parse_brew_cask_json(&output, &caskroom)
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

    pool::par_map(casks, |cask| {
        let token = cask.get("token")?.as_str()?.to_string();
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
        let mut size = if pkg_path.is_dir() {
            bulkstat::scan_dir(&pkg_path, 0).size
        } else {
            SizeInfo::default()
        };
        let mut primary_app_path = None;

        if !is_installer_based {
            for app_name in &app_names {
                if let Some(app_path) = find_cask_app_path(app_name, app_roots) {
                    let app_size = bulkstat::scan_dir(&app_path, 0).size;
                    size.logical = size.logical.saturating_add(app_size.logical);
                    size.allocated = size.allocated.saturating_add(app_size.allocated);
                    if primary_app_path.is_none() && app_path != pkg_path {
                        primary_app_path = Some(app_path);
                    }
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

fn scan_brew_dir(install_dir: &Path) -> Vec<Package> {
    if !install_dir.is_dir() {
        return Vec::new();
    }
    let Ok(read) = std::fs::read_dir(install_dir) else {
        return Vec::new();
    };
    let entries: Vec<_> = read
        .flatten()
        .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let pkg_path = e.path();
            (name, pkg_path)
        })
        .collect();

    pool::par_map(entries, |(name, pkg_path)| {
        let version = latest_subdir_name(&pkg_path);
        let size = Some(bulkstat::scan_dir(&pkg_path, 0).size);
        Package {
            name,
            version,
            size,
            path: Some(pkg_path),
            metadata_path: None,
        }
    })
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

fn scan_npm_global() -> Vec<Package> {
    let output = run_command("npm", &["list", "-g", "--depth=0", "--json"]);
    if output.is_empty() {
        return Vec::new();
    }
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&output) else {
        return Vec::new();
    };
    let Some(deps) = parsed.get("dependencies").and_then(|d| d.as_object()) else {
        return Vec::new();
    };

    let global_root = find_npm_global_root();

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

    pool::par_map(entries, |(name, version)| {
        let pkg_path = global_root.as_ref().map(|root| root.join(&name));
        let size = pkg_path
            .as_ref()
            .filter(|path| path.is_dir())
            .map(|path| bulkstat::scan_dir(path, 0).size);
        Package {
            name,
            version,
            size,
            path: pkg_path.filter(|path| path.is_dir()),
            metadata_path: None,
        }
    })
}

fn find_npm_global_root() -> Option<PathBuf> {
    let known_root = if cfg!(target_arch = "aarch64") {
        Some(PathBuf::from("/opt/homebrew/lib/node_modules"))
    } else {
        Some(PathBuf::from("/usr/local/lib/node_modules"))
    };
    find_npm_global_root_with(
        run_command,
        std::env::var_os("HOME").map(PathBuf::from),
        std::env::var_os("NVM_DIR").map(PathBuf::from),
        known_root,
    )
}

fn find_npm_global_root_with<F>(
    mut run: F,
    home: Option<PathBuf>,
    nvm_dir: Option<PathBuf>,
    known_root: Option<PathBuf>,
) -> Option<PathBuf>
where
    F: FnMut(&str, &[&str]) -> String,
{
    let active_root = run("npm", &["root", "-g"]);
    let active_root = active_root.trim();
    if !active_root.is_empty() {
        return Some(PathBuf::from(active_root));
    }

    if let Some(root) = nvm_dir
        .as_ref()
        .map(|dir| dir.join("versions/node"))
        .and_then(|dir| single_version_node_modules_root(&dir, Path::new("lib/node_modules")))
    {
        return Some(root);
    }

    if let Some(root) = home
        .as_ref()
        .map(|home| home.join(".local/share/fnm/node-versions"))
        .and_then(|dir| {
            single_version_node_modules_root(&dir, Path::new("installation/lib/node_modules"))
        })
    {
        return Some(root);
    }

    known_root.filter(|path| path.is_dir())
}

fn single_version_node_modules_root(base: &Path, suffix: &Path) -> Option<PathBuf> {
    let mut roots = std::fs::read_dir(base)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        .map(|entry| entry.path().join(suffix))
        .filter(|path| path.is_dir());
    let first = roots.next()?;
    if roots.next().is_some() {
        return None;
    }
    Some(first)
}

fn scan_pip() -> Vec<Package> {
    let output = run_pip_command(&["list", "--format=json"]);
    if output.is_empty() {
        return Vec::new();
    }
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&output) else {
        return Vec::new();
    };
    let Some(arr) = parsed.as_array() else {
        return Vec::new();
    };

    let site_packages = find_pip_site_packages();

    let entries: Vec<_> = arr
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?.to_string();
            let version = entry
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some((name, version))
        })
        .collect();

    pool::par_map(entries, |(name, version)| {
        let (size, path) = if let Some(sp) = &site_packages {
            find_pip_package_size_and_path(sp, &name)
        } else {
            (None, None)
        };
        Package {
            name,
            version,
            size,
            path,
            metadata_path: None,
        }
    })
}

fn scan_cargo() -> Vec<Package> {
    let output = run_command("cargo", &["install", "--list"]);
    if output.is_empty() {
        return Vec::new();
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cargo_bin = home.as_ref().map(|h| h.join(".cargo/bin"));

    let mut packages = Vec::new();
    let mut current_name = String::new();
    let mut current_version = String::new();
    let mut current_bins: Vec<String> = Vec::new();

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

        if let Some(bin_dir) = cargo_bin {
            for bin in bins.iter() {
                let bin_path = bin_dir.join(bin);
                if !bin_path.exists() {
                    continue;
                }
                if path.is_none() {
                    path = Some(bin_path.clone());
                }
                if let Ok(meta) = std::fs::symlink_metadata(&bin_path) {
                    size.logical = size.logical.saturating_add(meta.len());
                    size.allocated = size
                        .allocated
                        .saturating_add(allocated_size_from_metadata(&meta));
                    has_bin = true;
                }
            }
        }

        packages.push(Package {
            name: name.to_string(),
            version: version.to_string(),
            size: if has_bin { Some(size) } else { None },
            path,
            metadata_path: None,
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
        current_name = name.to_string();
        current_version = parts
            .next()
            .unwrap_or("")
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

    packages
}

fn scan_bun_global() -> Vec<Package> {
    let output = run_command("bun", &["pm", "ls", "-g"]);
    if output.is_empty() {
        return Vec::new();
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let bun_global = home
        .as_ref()
        .map(|h| h.join(".bun/install/global/node_modules"));

    let entries: Vec<_> = output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || !line.contains('@') {
                return None;
            }
            let cleaned = line.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '@');
            let (name, version) = if let Some(at_pos) = cleaned.rfind('@') {
                if at_pos == 0 {
                    return None;
                }
                (
                    cleaned[..at_pos].to_string(),
                    cleaned[at_pos + 1..].to_string(),
                )
            } else {
                (cleaned.to_string(), String::new())
            };
            Some((name, version))
        })
        .collect();

    pool::par_map(entries, |(name, version)| {
        let (size, path) = if let Some(global_dir) = &bun_global {
            let pkg_path = global_dir.join(&name);
            if pkg_path.is_dir() {
                (Some(bulkstat::scan_dir(&pkg_path, 0).size), Some(pkg_path))
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };
        Package {
            name,
            version,
            size,
            path,
            metadata_path: None,
        }
    })
}

pub fn find_project_deps(root: &Path, max_depth: usize) -> Vec<ProjectDeps> {
    let results = std::sync::Mutex::new(Vec::new());
    pool::par_drain(vec![(root.to_path_buf(), 0usize)], |(dir, depth), queue| {
        scan_project_dir(&dir, depth, max_depth, &results, queue);
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
    results
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

fn scan_project_dir(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    results: &std::sync::Mutex<Vec<ProjectDeps>>,
    queue: &pool::WorkQueue<(PathBuf, usize)>,
) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };

    let mut children = Vec::new();
    let mut found_manifests: Vec<(usize, &str, &str, &str)> = Vec::new();

    for entry in read.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
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

    let new_deps = pool::par_map(groups, |group| {
        let dep_count = group
            .manifests
            .iter()
            .map(|manifest| count_manifest_deps(dir, manifest))
            .sum();
        let (deps_size, deps_dir) = match group.deps_path {
            Some(deps_path) if deps_path.is_dir() => (
                Some(bulkstat::scan_dir(&deps_path, 0).size),
                Some(deps_path),
            ),
            _ => (None, None),
        };
        ProjectDeps {
            path: dir.to_path_buf(),
            manager_label: group.manager_labels.join(", "),
            manifest: group.manifests.join(", "),
            dep_count,
            deps_size,
            deps_dir,
        }
    });

    if !new_deps.is_empty() {
        results.lock().unwrap().extend(new_deps);
    }

    if depth < max_depth {
        for child in children {
            queue.push((child, depth + 1));
        }
    }
}

fn count_manifest_deps(dir: &Path, manifest: &str) -> usize {
    let path = dir.join(manifest);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return 0;
    };

    match manifest {
        "package.json" => count_package_json_deps(&content),
        "Cargo.toml" => count_cargo_toml_deps(&content),
        "requirements.txt" => count_requirements_deps(&content),
        "pyproject.toml" => count_pyproject_deps(&content),
        "go.mod" => count_go_mod_deps(&content),
        "Gemfile" => count_gemfile_deps(&content),
        "composer.json" => count_composer_deps(&content),
        _ => 0,
    }
}

fn count_package_json_deps(content: &str) -> usize {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content) else {
        return 0;
    };
    let deps = parsed
        .get("dependencies")
        .and_then(|d| d.as_object())
        .map(|d| d.len())
        .unwrap_or(0);
    let dev_deps = parsed
        .get("devDependencies")
        .and_then(|d| d.as_object())
        .map(|d| d.len())
        .unwrap_or(0);
    deps + dev_deps
}

fn count_cargo_toml_deps(content: &str) -> usize {
    let mut count = 0;
    let mut in_deps = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_deps = trimmed == "[dependencies]"
                || trimmed == "[dev-dependencies]"
                || trimmed == "[build-dependencies]";
            continue;
        }
        if in_deps && trimmed.contains('=') && !trimmed.starts_with('#') {
            count += 1;
        }
    }
    count
}

fn count_requirements_deps(content: &str) -> usize {
    content
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#') && !trimmed.starts_with('-')
        })
        .count()
}

fn count_pyproject_deps(content: &str) -> usize {
    let mut count = 0;
    let mut in_deps = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_deps = trimmed.contains("dependencies");
            continue;
        }
        if in_deps
            && ((trimmed.starts_with('"') || trimmed.starts_with('\''))
                || (trimmed.contains('=') && !trimmed.starts_with('#')))
        {
            count += 1;
        }
    }
    count
}

fn count_go_mod_deps(content: &str) -> usize {
    let mut count = 0;
    let mut in_require = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("require (") || trimmed == "require (" {
            in_require = true;
            continue;
        }
        if trimmed == ")" {
            in_require = false;
            continue;
        }
        if in_require && !trimmed.is_empty() && !trimmed.starts_with("//") {
            count += 1;
        }
        if trimmed.starts_with("require ") && !trimmed.contains('(') {
            count += 1;
        }
    }
    count
}

fn count_gemfile_deps(content: &str) -> usize {
    content
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("gem ")
        })
        .count()
}

fn count_composer_deps(content: &str) -> usize {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content) else {
        return 0;
    };
    let required = parsed
        .get("require")
        .and_then(|d| d.as_object())
        .map(|d| d.len())
        .unwrap_or(0);
    let dev_required = parsed
        .get("require-dev")
        .and_then(|d| d.as_object())
        .map(|d| d.len())
        .unwrap_or(0);
    required + dev_required
}

fn command_exists(cmd: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };

    std::env::split_paths(&paths).any(|dir| is_executable(dir.join(cmd)))
}

fn is_executable(path: PathBuf) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn run_command(cmd: &str, args: &[&str]) -> String {
    let mut command = Command::new(cmd);
    command.args(args);
    if cmd == "brew" {
        command.env("HOMEBREW_NO_AUTO_UPDATE", "1");
    }
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
}

fn run_pip_command(args: &[&str]) -> String {
    match pip_command() {
        Some(PipCommand::Pip3) => run_command("pip3", args),
        Some(PipCommand::Python3Module) => {
            let mut pip_args: Vec<&str> = vec!["-m", "pip"];
            pip_args.extend_from_slice(args);
            run_command("python3", &pip_args)
        }
        None => String::new(),
    }
}

#[derive(Clone, Copy)]
enum PipCommand {
    Pip3,
    Python3Module,
}

fn pip_command() -> Option<PipCommand> {
    if command_exists("pip3") {
        return Some(PipCommand::Pip3);
    }
    let has_python_pip = !run_command("python3", &["-m", "pip", "--version"])
        .trim()
        .is_empty();
    if command_exists("python3") && has_python_pip {
        return Some(PipCommand::Python3Module);
    }
    None
}

fn find_pip_package_size_and_path(
    site_packages: &Path,
    package: &str,
) -> (Option<SizeInfo>, Option<PathBuf>) {
    let Some(dist_info_path) = find_pip_dist_info(site_packages, package) else {
        return (None, infer_pip_package_dir(site_packages, package));
    };

    let size = parse_pip_record_size(site_packages, &dist_info_path);
    let path = find_pip_top_level_path(site_packages, &dist_info_path)
        .or_else(|| infer_pip_package_dir(site_packages, package));

    (size, path)
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

fn find_pip_dist_info(site_packages: &Path, package: &str) -> Option<PathBuf> {
    let needle = normalize_dist_name(package);
    let read = std::fs::read_dir(site_packages).ok()?;

    for entry in read.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !name.ends_with(".dist-info") {
            continue;
        }
        let prefix = &name[..name.len() - ".dist-info".len()];
        let normalized = normalize_dist_name(prefix);
        let exact = normalized == needle;
        let versioned = normalized.strip_prefix(&(needle.clone() + "-")).is_some();
        if exact || versioned {
            return Some(entry.path());
        }
    }
    None
}

fn find_pip_top_level_path(site_packages: &Path, dist_info: &Path) -> Option<PathBuf> {
    let top_level = dist_info.join("top_level.txt");
    let content = std::fs::read_to_string(top_level).ok()?;
    for line in content.lines() {
        let entry = line.trim();
        if entry.is_empty() {
            continue;
        }
        let candidate = site_packages.join(entry);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn parse_pip_record_size(site_packages: &Path, dist_info: &Path) -> Option<SizeInfo> {
    let record = std::fs::read_to_string(dist_info.join("RECORD")).ok()?;
    let mut logical = 0u64;
    let mut allocated = 0u64;

    for row in record.lines().filter(|line| !line.trim().is_empty()) {
        let fields = split_csv_row(row);
        if fields.len() < 3 {
            continue;
        }
        let rel_path = fields[0].trim();
        if rel_path.is_empty() {
            continue;
        }

        if let Ok(bytes) = fields[2].trim().parse::<u64>() {
            logical = logical.saturating_add(bytes);
        }

        let full_path = site_packages.join(rel_path);
        if let Ok(meta) = std::fs::symlink_metadata(&full_path) {
            allocated = allocated.saturating_add(allocated_size_from_metadata(&meta));
        }
    }

    Some(SizeInfo { logical, allocated })
}

fn split_csv_row(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '"' {
            if in_quotes && matches!(chars.peek(), Some('"')) {
                chars.next();
                current.push('"');
            } else {
                in_quotes = !in_quotes;
            }
        } else if ch == ',' && !in_quotes {
            fields.push(std::mem::take(&mut current));
        } else {
            current.push(ch);
        }
    }
    fields.push(current);
    fields
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

fn brew_prefix() -> PathBuf {
    if cfg!(target_arch = "aarch64") {
        PathBuf::from("/opt/homebrew")
    } else {
        PathBuf::from("/usr/local")
    }
}

fn find_pip_site_packages() -> Option<PathBuf> {
    let output = run_command(
        "python3",
        &["-c", "import site; print(site.getsitepackages()[0])"],
    );
    let path = output.trim();
    if path.is_empty() {
        return None;
    }
    let p = PathBuf::from(path);
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_package_json() {
        let content = r#"{"dependencies":{"a":"1","b":"2"},"devDependencies":{"c":"3"}}"#;
        assert_eq!(count_package_json_deps(content), 3);
    }

    #[test]
    fn count_cargo_toml() {
        let content = "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\nanyhow = \"1\"\n\n[dev-dependencies]\ntempfile = \"3\"\n";
        assert_eq!(count_cargo_toml_deps(content), 3);
    }

    #[test]
    fn count_requirements() {
        let content = "flask==2.0\nrequests>=2.28\n# comment\n-r base.txt\n\nnumpy\n";
        assert_eq!(count_requirements_deps(content), 3);
    }

    #[test]
    fn count_go_mod() {
        let content = "module example.com/foo\n\ngo 1.21\n\nrequire (\n\tgithub.com/a/b v1.0\n\tgithub.com/c/d v2.0\n)\n";
        assert_eq!(count_go_mod_deps(content), 2);
    }

    #[test]
    fn count_gemfile() {
        let content = "source 'https://rubygems.org'\ngem 'rails'\ngem 'pg'\n";
        assert_eq!(count_gemfile_deps(content), 2);
    }

    #[test]
    fn count_composer_json() {
        let content = r#"{"require":{"php":"^8.2","monolog/monolog":"^3"},"require-dev":{"phpunit/phpunit":"^11"}}"#;
        assert_eq!(count_composer_deps(content), 3);
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
        if cmd == "pip3" {
            assert_eq!(args, vec!["uninstall", "-y", "ruff"]);
        } else {
            assert_eq!(cmd, "python3");
            assert_eq!(args, vec!["-m", "pip", "uninstall", "-y", "ruff"]);
        }

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
            find_pip_top_level_path(&root, &dist_info),
            Some(root.join("yaml"))
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn splits_csv_fields_with_quoted_values() {
        let fields = split_csv_row("path,to,script.py,sha256=abc,12");
        assert_eq!(fields, vec!["path", "to", "script.py", "sha256=abc", "12"]);
        let fields = split_csv_row("\"hello,world\",sha256=abc,7");
        assert_eq!(fields, vec!["hello,world", "sha256=abc", "7"]);
    }

    #[test]
    fn parses_pip_record_sizes_from_manifest() {
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
            "requests/__init__.py,sha256=abc,13\n\
             requests-2.32.0.dist-info/RECORD,sha256=def,7\n",
        )
        .unwrap();

        let size = parse_pip_record_size(&root, &dist_info).unwrap();
        assert_eq!(size.logical, 20);
        assert!(size.allocated > 0);

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
    fn npm_global_root_prefers_active_npm_over_version_probes() {
        let root = test_root("npm_root_prefers_active");
        let active_root = root.join("active/lib/node_modules");
        let nvm_root = root.join("nvm/versions/node/v22.0.0/lib/node_modules");
        let fnm_root =
            root.join("home/.local/share/fnm/node-versions/v24.0.0/installation/lib/node_modules");
        std::fs::create_dir_all(&active_root).unwrap();
        std::fs::create_dir_all(&nvm_root).unwrap();
        std::fs::create_dir_all(&fnm_root).unwrap();

        let resolved = find_npm_global_root_with(
            |cmd, args| {
                assert_eq!(cmd, "npm");
                assert_eq!(args, ["root", "-g"]);
                format!("{}\n", active_root.display())
            },
            Some(root.join("home")),
            Some(root.join("nvm")),
            None,
        );

        assert_eq!(resolved, Some(active_root.clone()));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn npm_global_root_does_not_guess_across_multiple_nvm_versions() {
        let root = test_root("npm_root_ambiguous_nvm");
        let nvm_versions = root.join("nvm/versions/node");
        std::fs::create_dir_all(nvm_versions.join("v20.0.0/lib/node_modules")).unwrap();
        std::fs::create_dir_all(nvm_versions.join("v22.0.0/lib/node_modules")).unwrap();

        let resolved = find_npm_global_root_with(
            |_cmd, _args| String::new(),
            None,
            Some(root.join("nvm")),
            None,
        );

        assert_eq!(resolved, None);

        std::fs::remove_dir_all(root).unwrap();
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
        assert_eq!(deps[0].dep_count, 1);
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
            "[project.dependencies]\nclick = \"8\"\n",
        )
        .unwrap();
        std::fs::write(proj.join(".venv/lib/site.py"), b"python deps").unwrap();

        let deps = find_project_deps(&root, 3);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].path, proj);
        assert_eq!(deps[0].manager_label, "pip, pip/uv");
        assert_eq!(deps[0].manifest, "requirements.txt, pyproject.toml");
        assert_eq!(deps[0].dep_count, 3);
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
        assert_eq!(pkgs.version, "1.2.10 (installer-based)");
        assert_eq!(pkgs.path.as_ref().unwrap(), &temp_caskroom.join("packages"));
        assert!(pkgs.metadata_path.is_none());

        std::fs::remove_dir_all(temp_caskroom).unwrap();
        std::fs::remove_dir_all(temp_apps).unwrap();
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("diskr_{name}_{}_{}", std::process::id(), nanos))
    }
}
