use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use rayon::prelude::*;

use crate::bulkstat::{self, SizeInfo};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub size: Option<SizeInfo>,
    pub path: Option<PathBuf>,
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

#[derive(Clone, Debug, Default)]
pub struct DepInfo {
    pub dependencies: Vec<String>,
    pub dependents: Vec<String>,
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

    pub fn is_removable(&self, manager: Manager, name: &str) -> bool {
        match self.entries.get(&(manager, name.to_string())) {
            Some(info) => info.dependents.is_empty(),
            None => true,
        }
    }

    pub fn removable_count(&self) -> usize {
        self.entries
            .values()
            .filter(|info| info.dependents.is_empty())
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
    pub manager_label: &'static str,
    pub manifest: &'static str,
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
        if pip_names.is_empty() || !command_exists("pip3") {
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
            let info = match report.manager {
                Manager::Brew => brew_deps.get(&pkg.name).cloned().unwrap_or_default(),
                Manager::BrewCask => DepInfo::default(),
                Manager::Pip => pip_deps.get(&pkg.name).cloned().unwrap_or_default(),
                _ => DepInfo::default(),
            };
            graph
                .entries
                .insert((report.manager, pkg.name.clone()), info);
        }
    }

    graph
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
        result.insert(
            name.clone(),
            DepInfo {
                dependencies: deps.clone(),
                dependents,
            },
        );
    }
    for (name, dependents) in reverse {
        result.entry(name).or_default().dependents = dependents;
    }
    result
}

fn scan_pip_dep_graph(names: &[String]) -> HashMap<String, DepInfo> {
    let mut args: Vec<&str> = vec!["show"];
    for name in names {
        args.push(name);
    }
    let output = run_command("pip3", &args);
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
                DepInfo {
                    dependencies: std::mem::take(deps),
                    dependents: std::mem::take(rev),
                },
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
    if !command_exists(manager.command()) {
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
    let caskroom = brew_prefix().join("Caskroom");
    scan_brew_dir(&caskroom)
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

    entries
        .into_par_iter()
        .map(|(name, pkg_path)| {
            let version = latest_subdir_name(&pkg_path);
            let size = Some(bulkstat::scan_dir(&pkg_path, 0).size);
            Package {
                name,
                version,
                size,
                path: Some(pkg_path),
            }
        })
        .collect()
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

    entries
        .into_par_iter()
        .map(|(name, version)| {
            let pkg_path = global_root.join(&name);
            let size = if pkg_path.is_dir() {
                Some(bulkstat::scan_dir(&pkg_path, 0).size)
            } else {
                None
            };
            Package {
                name,
                version,
                size,
                path: Some(pkg_path),
            }
        })
        .collect()
}

fn find_npm_global_root() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(ref home) = home {
        if let Some(nvm_dir) = std::env::var_os("NVM_DIR") {
            let nvm_current = PathBuf::from(nvm_dir).join("versions/node");
            if let Ok(rd) = std::fs::read_dir(&nvm_current) {
                if let Some(latest) = rd
                    .flatten()
                    .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
                    .max_by_key(|e| e.file_name())
                {
                    let root = latest.path().join("lib/node_modules");
                    if root.is_dir() {
                        return root;
                    }
                }
            }
        }
        let fnm = home.join(".local/share/fnm/node-versions");
        if fnm.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&fnm) {
                if let Some(latest) = rd
                    .flatten()
                    .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
                    .max_by_key(|e| e.file_name())
                {
                    let root = latest.path().join("installation/lib/node_modules");
                    if root.is_dir() {
                        return root;
                    }
                }
            }
        }
    }
    let known = if cfg!(target_arch = "aarch64") {
        "/opt/homebrew/lib/node_modules"
    } else {
        "/usr/local/lib/node_modules"
    };
    let known_path = PathBuf::from(known);
    if known_path.is_dir() {
        return known_path;
    }
    let fallback = run_command("npm", &["root", "-g"]).trim().to_string();
    PathBuf::from(fallback)
}

fn scan_pip() -> Vec<Package> {
    let output = run_command("pip3", &["list", "--format=json"]);
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

    entries
        .into_par_iter()
        .map(|(name, version)| {
            let (size, path) = if let Some(sp) = &site_packages {
                let pkg_dir = sp.join(&name);
                let pkg_dir_alt = sp.join(name.replace('-', "_"));
                let actual = if pkg_dir.is_dir() {
                    Some(pkg_dir)
                } else if pkg_dir_alt.is_dir() {
                    Some(pkg_dir_alt)
                } else {
                    None
                };
                match actual {
                    Some(dir) => (Some(bulkstat::scan_dir(&dir, 0).size), Some(dir)),
                    None => (None, None),
                }
            } else {
                (None, None)
            };
            Package {
                name,
                version,
                size,
                path,
            }
        })
        .collect()
}

fn scan_cargo() -> Vec<Package> {
    let output = run_command("cargo", &["install", "--list"]);
    if output.is_empty() {
        return Vec::new();
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cargo_bin = home.as_ref().map(|h| h.join(".cargo/bin"));

    let mut packages = Vec::new();
    for line in output.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else {
            continue;
        };
        let version = parts
            .next()
            .unwrap_or("")
            .trim_start_matches('v')
            .trim_end_matches(':')
            .to_string();
        let (size, path) = if let Some(bin_dir) = &cargo_bin {
            let bin_path = bin_dir.join(name);
            if bin_path.exists() {
                let meta = std::fs::metadata(&bin_path).ok();
                let size = meta.map(|m| {
                    use std::os::unix::fs::MetadataExt;
                    SizeInfo::new(m.len(), m.blocks().saturating_mul(512))
                });
                (size, Some(bin_path))
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };
        packages.push(Package {
            name: name.to_string(),
            version,
            size,
            path,
        });
    }
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

    entries
        .into_par_iter()
        .map(|(name, version)| {
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
            }
        })
        .collect()
}

pub fn find_project_deps(root: &Path, max_depth: usize) -> Vec<ProjectDeps> {
    let results = std::sync::Mutex::new(Vec::new());
    find_project_deps_parallel(root, 0, max_depth, &results);
    let mut results = results.into_inner().unwrap_or_default();
    results.sort_by(|a, b| {
        let a_size = a.deps_size.map(|s| s.allocated).unwrap_or(0);
        let b_size = b.deps_size.map(|s| s.allocated).unwrap_or(0);
        b_size.cmp(&a_size).then(a.path.cmp(&b.path))
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

fn find_project_deps_parallel(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    results: &std::sync::Mutex<Vec<ProjectDeps>>,
) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };

    let mut children = Vec::new();
    let mut found_manifests: Vec<(&str, &str, &str)> = Vec::new();

    for entry in read.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };

        if file_type.is_file() {
            for (manifest, mgr, deps_dir) in PROJECT_MANIFESTS {
                if name_str == *manifest {
                    found_manifests.push((manifest, mgr, deps_dir));
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

    let new_deps: Vec<_> = found_manifests
        .into_par_iter()
        .map(|(manifest, mgr, deps_dir_name)| {
            let dep_count = count_manifest_deps(dir, manifest);
            let (deps_size, deps_dir) = if !deps_dir_name.is_empty() {
                let deps_path = dir.join(deps_dir_name);
                if deps_path.is_dir() {
                    (
                        Some(bulkstat::scan_dir(&deps_path, 0).size),
                        Some(deps_path),
                    )
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };
            ProjectDeps {
                path: dir.to_path_buf(),
                manager_label: mgr,
                manifest,
                dep_count,
                deps_size,
                deps_dir,
            }
        })
        .collect();

    if !new_deps.is_empty() {
        results.lock().unwrap().extend(new_deps);
    }

    if depth < max_depth {
        children.par_iter().for_each(|child| {
            find_project_deps_parallel(child, depth + 1, max_depth, results);
        });
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
        assert_eq!(cmd, "pip3");
        assert_eq!(args, vec!["uninstall", "-y", "ruff"]);

        let (cmd, args) = Manager::Npm.uninstall_args("typescript");
        assert_eq!(cmd, "npm");
        assert_eq!(args, vec!["uninstall", "-g", "typescript"]);
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

    fn test_root(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("diskr_{name}_{}_{}", std::process::id(), nanos))
    }
}
