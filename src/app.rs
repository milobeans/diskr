use anyhow::Result;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crate::bulkstat::SizeInfo;
use crate::packages::{self, ManagerReport, ProjectDeps};
use crate::scanner::{ScanId, ScanMsg, Scanner};

const SORT_DEBOUNCE: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Files,
    Disks,
    Packages,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Name,
    SizeDesc,
    Modified,
}

impl SortMode {
    pub fn label(self) -> &'static str {
        match self {
            SortMode::Name => "name",
            SortMode::SizeDesc => "size↓",
            SortMode::Modified => "mtime",
        }
    }
}

#[derive(Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: Option<SizeInfo>,
    pub modified: Option<std::time::SystemTime>,
    pub scanning: bool,
}

#[derive(Clone)]
pub enum DeleteTarget {
    FileEntry(Entry),
    Package {
        name: String,
        path: PathBuf,
        is_project_dep: bool,
    },
}

pub struct DiskInfo {
    pub name: String,
    pub mount: PathBuf,
    pub total: u64,
    pub available: u64,
}

pub struct App {
    pub cwd: PathBuf,
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub selected_disk: usize,
    pub selected_pkg: usize,
    pub show_hidden: bool,
    pub sort: SortMode,
    pub focus: Focus,
    pub disks: Vec<DiskInfo>,
    pub status: String,
    pub confirming_delete: bool,

    pub pkg_reports: Vec<ManagerReport>,
    pub project_deps: Vec<ProjectDeps>,
    pub pkg_view: PkgView,
    pub packages_loaded: bool,
    pub packages_loading: bool,

    pending_delete: Option<DeleteTarget>,
    size_cache: HashMap<PathBuf, SizeInfo>,
    last_sort: Instant,
    sort_dirty: bool,
    active_scan_id: ScanId,

    scan_rx: Receiver<ScanMsg>,
    scanner: Scanner,
    active_pkg_scan_id: ScanId,
    pkg_scan_rx: Option<Receiver<PkgScanMsg>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PkgView {
    SystemManagers,
    ProjectDeps,
}

struct PkgScanMsg {
    scan_id: ScanId,
    reports: Vec<ManagerReport>,
    project_deps: Vec<ProjectDeps>,
    include_managers: bool,
}

impl PkgView {
    // Methods related to PkgView...
}

impl App {
    pub fn new(cwd: PathBuf) -> Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App {
            cwd,
            entries: Vec::new(),
            selected: 0,
            selected_disk: 0,
            selected_pkg: 0,
            show_hidden: false,
            sort: SortMode::SizeDesc,
            focus: Focus::Files,
            disks: Vec::new(),
            status: String::from("Space preview · f Finder · O open"),
            confirming_delete: false,
            pkg_reports: Vec::new(),
            project_deps: Vec::new(),
            pkg_view: PkgView::SystemManagers,
            packages_loaded: false,
            packages_loading: false,
            pending_delete: None,
            size_cache: HashMap::new(),
            last_sort: Instant::now(),
            sort_dirty: false,
            active_scan_id: 0,
            scanner: Scanner::new(tx.clone()),
            scan_rx: rx,
            active_pkg_scan_id: 0,
            pkg_scan_rx: None,
        };
        app.refresh_disks();
        app.reload()?;
        Ok(app)
    }

    pub fn reload(&mut self) -> Result<()> {
        let previous_selected = self
            .entries
            .get(self.selected)
            .map(|entry| entry.path.clone());
        let previous_index = self.selected;
        self.reload_with_selection(previous_selected, previous_index)
    }

    fn reload_with_selection(
        &mut self,
        previous_selected: Option<PathBuf>,
        previous_index: usize,
    ) -> Result<()> {
        self.rebuild_entries()?;
        self.restore_selection(previous_selected, previous_index);
        self.auto_scan();
        Ok(())
    }

    fn rebuild_entries(&mut self) -> Result<()> {
        self.entries.clear();
        let read = match std::fs::read_dir(&self.cwd) {
            Ok(r) => r,
            Err(e) => {
                self.status = format!("cannot read {}: {e}", self.cwd.display());
                return Ok(());
            }
        };
        for dirent in read.flatten() {
            let name = dirent.file_name().to_string_lossy().into_owned();
            if !self.show_hidden && name.starts_with('.') {
                continue;
            }
            let path = dirent.path();
            let meta = std::fs::symlink_metadata(&path).ok();
            let is_dir = meta
                .as_ref()
                .map(|m| m.file_type().is_dir())
                .unwrap_or(false);
            let size = if is_dir {
                self.size_cache.get(&path).copied()
            } else {
                meta.as_ref().and_then(|m| {
                    if m.file_type().is_file() {
                        Some(SizeInfo::new(m.len(), m.blocks().saturating_mul(512)))
                    } else {
                        None
                    }
                })
            };
            let modified = meta.as_ref().and_then(|m| m.modified().ok());
            self.entries.push(Entry {
                name,
                path,
                is_dir,
                size,
                modified,
                scanning: false,
            });
        }
        self.apply_sort();
        Ok(())
    }

    pub fn apply_sort(&mut self) {
        match self.sort {
            SortMode::Name => self.entries.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            }),
            SortMode::SizeDesc => self
                .entries
                .sort_by_key(|e| Reverse(e.size.map(size_sort_key).unwrap_or(0))),
            SortMode::Modified => self.entries.sort_by_key(|e| Reverse(e.modified)),
        }
        self.last_sort = Instant::now();
        self.sort_dirty = false;
    }

    fn apply_sort_preserving_selection(&mut self) {
        let sp = self.entries.get(self.selected).map(|e| e.path.clone());
        self.apply_sort();
        if let Some(sp) = sp {
            if let Some(idx) = self.entries.iter().position(|e| e.path == sp) {
                self.selected = idx;
            }
        }
    }

    pub fn cycle_sort(&mut self) {
        if self.confirming_delete {
            return;
        }
        self.sort = match self.sort {
            SortMode::Name => SortMode::SizeDesc,
            SortMode::SizeDesc => SortMode::Modified,
            SortMode::Modified => SortMode::Name,
        };
        self.apply_sort_preserving_selection();
        self.status = format!("sort: {}", self.sort.label());
    }

    pub fn move_cursor(&mut self, delta: i32) {
        if self.confirming_delete {
            return;
        }
        match self.focus {
            Focus::Files => {
                if self.entries.is_empty() {
                    return;
                }
                let n = self.entries.len() as i32;
                let s = self.selected as i32 + delta;
                self.selected = s.rem_euclid(n) as usize;
            }
            Focus::Disks => {
                if self.disks.is_empty() {
                    return;
                }
                let n = self.disks.len() as i32;
                let s = self.selected_disk as i32 + delta;
                self.selected_disk = s.rem_euclid(n) as usize;
            }
            Focus::Packages => {
                let n = self.pkg_item_count() as i32;
                if n == 0 {
                    return;
                }
                let s = self.selected_pkg as i32 + delta;
                self.selected_pkg = s.rem_euclid(n) as usize;
            }
        }
    }

    pub fn enter(&mut self) -> Result<()> {
        if self.confirming_delete {
            return Ok(());
        }
        match self.focus {
            Focus::Files => {
                if let Some(entry) = self.entries.get(self.selected).cloned() {
                    if entry.is_dir {
                        self.cwd = entry.path;
                        self.selected = 0;
                        self.reload()?;
                    }
                }
            }
            Focus::Disks => {
                if let Some(disk) = self.disks.get(self.selected_disk) {
                    self.cwd = disk.mount.clone();
                    self.focus = Focus::Files;
                    self.selected = 0;
                    self.reload()?;
                }
            }
            Focus::Packages => {}
        }
        Ok(())
    }

    pub fn go_up(&mut self) -> Result<()> {
        if self.confirming_delete {
            return Ok(());
        }
        let previous_cwd = self.cwd.clone();
        if let Some(parent) = self.cwd.parent().map(|p| p.to_path_buf()) {
            self.cwd = parent;
            self.reload_with_selection(Some(previous_cwd), self.selected)?;
        }
        Ok(())
    }

    pub fn toggle_hidden(&mut self) -> Result<()> {
        if self.confirming_delete {
            return Ok(());
        }
        self.show_hidden = !self.show_hidden;
        self.reload()
    }

    /// Scan only directories missing a size (usually because the cache didn't have them).
    fn auto_scan(&mut self) {
        let scan_id = self.next_scan_id();
        let dirs: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|e| e.is_dir && e.size.is_none())
            .map(|e| e.path.clone())
            .collect();
        if dirs.is_empty() {
            self.status = String::from("cache hit · all sizes known");
            return;
        }
        for e in self
            .entries
            .iter_mut()
            .filter(|e| e.is_dir && e.size.is_none())
        {
            e.scanning = true;
        }
        let dir_count = dirs.len();
        self.start_scan(scan_id, dirs, format!("scanning {dir_count} directories…"));
    }

    /// Invoked by the `r` key. Refreshes the directory view and rescans visible directories.
    pub fn force_rescan(&mut self) {
        if self.confirming_delete {
            return;
        }
        let scan_id = self.next_scan_id();
        let previous_selected = self
            .entries
            .get(self.selected)
            .map(|entry| entry.path.clone());
        let previous_index = self.selected;
        for e in self.entries.iter().filter(|e| e.is_dir) {
            self.size_cache.remove(&e.path);
        }
        self.refresh_disks();
        if self.rebuild_entries().is_err() {
            return;
        }
        self.restore_selection(previous_selected, previous_index);
        for e in self.entries.iter_mut().filter(|entry| entry.is_dir) {
            e.size = None;
            e.scanning = true;
        }
        let dirs: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|e| e.is_dir)
            .map(|e| e.path.clone())
            .collect();
        if dirs.is_empty() {
            self.status = String::from("refresh complete · no directories to rescan");
            return;
        }
        let dir_count = dirs.len();
        self.start_scan(scan_id, dirs, format!("rescan: {dir_count} directories…"));
    }

    pub fn drain_scan_results(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.scan_rx.try_recv() {
            match msg {
                ScanMsg::DirSize {
                    scan_id,
                    path,
                    size,
                } if scan_id == self.active_scan_id => {
                    self.size_cache.insert(path.clone(), size);
                    if let Some(e) = self.entries.iter_mut().find(|e| e.path == path) {
                        e.size = Some(size);
                        e.scanning = false;
                        changed = true;
                    }
                    if self.sort == SortMode::SizeDesc {
                        self.sort_dirty = true;
                    }
                }
                ScanMsg::AllDone { scan_id } if scan_id == self.active_scan_id => {
                    if self.sort_dirty {
                        self.apply_sort_preserving_selection();
                    }
                    self.status = String::from("scan complete");
                    changed = true;
                }
                _ => {}
            }
        }
        if self.sort_dirty && self.last_sort.elapsed() >= SORT_DEBOUNCE {
            self.apply_sort_preserving_selection();
            changed = true;
        }
        changed |= self.drain_package_results();
        changed
    }

    pub fn has_pending_scan_work(&self) -> bool {
        self.sort_dirty || self.entries.iter().any(|entry| entry.scanning) || self.packages_loading
    }

    pub fn request_delete(&mut self) {
        if self.confirming_delete {
            return;
        }
        match self.focus {
            Focus::Files => {
                if let Some(entry) = self.entries.get(self.selected).cloned() {
                    self.pending_delete = Some(DeleteTarget::FileEntry(entry));
                    self.confirming_delete = true;
                }
            }
            Focus::Disks => {
                self.status = String::from("cannot delete a disk mount");
            }
            Focus::Packages => match self.pkg_view {
                PkgView::SystemManagers => {
                    let packages = self.flat_packages();
                    if let Some((package, manager)) = packages.get(self.selected_pkg) {
                        if let Some(path) = &package.path {
                            self.pending_delete = Some(DeleteTarget::Package {
                                name: format!("{} {}", manager.label(), package.name),
                                path: path.clone(),
                                is_project_dep: false,
                            });
                            self.confirming_delete = true;
                        } else {
                            self.status = format!("no path known for package {}", package.name);
                        }
                    }
                }
                PkgView::ProjectDeps => {
                    if let Some(dep) = self.project_deps.get(self.selected_pkg) {
                        if let Some(deps_dir) = &dep.deps_dir {
                            self.pending_delete = Some(DeleteTarget::Package {
                                name: format!(
                                    "{} dependency dir ({})",
                                    dep.manager_label,
                                    deps_dir.display()
                                ),
                                path: deps_dir.clone(),
                                is_project_dep: true,
                            });
                            self.confirming_delete = true;
                        } else {
                            self.status = format!(
                                "no local dependency directory to delete for {}",
                                dep.manifest
                            );
                        }
                    }
                }
            },
        }
    }

    pub fn cancel_delete(&mut self) {
        self.confirming_delete = false;
        self.pending_delete = None;
    }

    pub fn confirm_delete(&mut self) -> Result<()> {
        self.confirming_delete = false;
        if let Some(target) = self.pending_delete.take() {
            match target {
                DeleteTarget::FileEntry(entry) => {
                    match crate::fs_ops::delete_to_trash(&entry.path) {
                        Ok(()) => {
                            self.status = format!("moved to trash: {}", entry.name);
                            self.invalidate_cache_for(&entry.path);
                            self.refresh_disks();
                            let status = self.status.clone();
                            self.reload()?;
                            self.status = status;
                        }
                        Err(e) => self.status = format!("delete failed: {e}"),
                    }
                }
                DeleteTarget::Package {
                    name,
                    path,
                    is_project_dep,
                } => match crate::fs_ops::delete_to_trash(&path) {
                    Ok(()) => {
                        self.status = format!("moved to trash: {name}");
                        self.invalidate_cache_for(&path);
                        self.refresh_disks();
                        if is_project_dep {
                            self.reload_project_deps();
                        } else {
                            self.refresh_packages();
                        }
                    }
                    Err(e) => self.status = format!("delete failed: {e}"),
                },
            }
        }
        Ok(())
    }

    pub fn pending_delete_name(&self) -> &str {
        match &self.pending_delete {
            Some(DeleteTarget::FileEntry(entry)) => &entry.name,
            Some(DeleteTarget::Package { name, .. }) => name,
            None => "?",
        }
    }

    /// When a path changes (deletion, write), its own cache entry and every
    /// ancestor's cached size are now stale.
    fn invalidate_cache_for(&mut self, path: &Path) {
        self.size_cache.remove(path);
        let mut p = path.parent();
        while let Some(parent) = p {
            self.size_cache.remove(parent);
            p = parent.parent();
        }
    }

    fn next_scan_id(&mut self) -> ScanId {
        self.active_scan_id = self.active_scan_id.saturating_add(1);
        self.active_scan_id
    }

    fn restore_selection(&mut self, previous_selected: Option<PathBuf>, previous_index: usize) {
        self.selected = previous_selected
            .and_then(|path| self.entries.iter().position(|entry| entry.path == path))
            .unwrap_or_else(|| previous_index.min(self.entries.len().saturating_sub(1)));
    }

    fn start_scan(&mut self, scan_id: ScanId, dirs: Vec<PathBuf>, status: String) {
        match self.scanner.scan_all(scan_id, dirs) {
            Ok(()) => self.status = status,
            Err(err) => {
                for entry in &mut self.entries {
                    entry.scanning = false;
                }
                self.sort_dirty = false;
                self.status = format!("could not start scanner: {err}");
            }
        }
    }

    fn request_package_scan(&mut self, include_managers: bool) {
        if self.packages_loading {
            return;
        }
        let scan_id = self.next_pkg_scan_id();
        let (tx, rx) = std::sync::mpsc::channel();
        let cwd = self.cwd.clone();

        self.packages_loading = true;
        self.pkg_scan_rx = Some(rx);
        self.status = if include_managers {
            String::from("scanning packages…")
        } else {
            String::from("refreshing project dependencies…")
        };

        thread::spawn(move || {
            let reports = if include_managers {
                packages::scan_managers()
            } else {
                Vec::new()
            };
            let project_deps = packages::find_project_deps(&cwd, 5);
            let _ = tx.send(PkgScanMsg {
                scan_id,
                reports,
                project_deps,
                include_managers,
            });
        });
    }

    fn next_pkg_scan_id(&mut self) -> ScanId {
        self.active_pkg_scan_id = self.active_pkg_scan_id.saturating_add(1);
        self.active_pkg_scan_id
    }

    fn drain_package_results(&mut self) -> bool {
        loop {
            let recv = match self.pkg_scan_rx.as_ref() {
                Some(rx) => rx.try_recv(),
                None => return false,
            };

            match recv {
                Ok(msg) => {
                    if msg.scan_id != self.active_pkg_scan_id {
                        continue;
                    }
                    if msg.include_managers {
                        self.pkg_reports = msg.reports;
                        self.packages_loaded = true;
                    }
                    self.project_deps = msg.project_deps;
                    self.selected_pkg = self
                        .selected_pkg
                        .min(self.pkg_item_count().saturating_sub(1));
                    self.packages_loading = false;
                    self.pkg_scan_rx = None;
                    self.status = if msg.include_managers {
                        let total: usize = self.pkg_reports.iter().map(|r| r.packages.len()).sum();
                        format!(
                            "{} packages across {} managers · {} projects",
                            total,
                            self.pkg_reports.iter().filter(|r| r.available).count(),
                            self.project_deps.len()
                        )
                    } else {
                        format!(
                            "project dependencies refreshed · {} projects",
                            self.project_deps.len()
                        )
                    };
                    return true;
                }
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => {
                    self.packages_loading = false;
                    self.pkg_scan_rx = None;
                    self.status = String::from("package scan failed");
                    return true;
                }
            }
        }
    }

    pub fn load_packages(&mut self) {
        if self.packages_loaded {
            self.reload_project_deps();
            return;
        }
        self.request_package_scan(true);
    }

    pub fn refresh_packages(&mut self) {
        self.request_package_scan(true);
    }

    pub fn reload_project_deps(&mut self) {
        self.request_package_scan(false);
    }

    pub fn toggle_pkg_view(&mut self) {
        self.pkg_view = match self.pkg_view {
            PkgView::SystemManagers => PkgView::ProjectDeps,
            PkgView::ProjectDeps => PkgView::SystemManagers,
        };
        self.selected_pkg = 0;
    }

    pub fn pkg_item_count(&self) -> usize {
        match self.pkg_view {
            PkgView::SystemManagers => self.flat_packages().len(),
            PkgView::ProjectDeps => self.project_deps.len(),
        }
    }

    pub fn flat_packages(&self) -> Vec<(&packages::Package, packages::Manager)> {
        let mut out = Vec::new();
        for report in &self.pkg_reports {
            if !report.available {
                continue;
            }
            for pkg in &report.packages {
                out.push((pkg, report.manager));
            }
        }
        out
    }

    pub fn refresh_disks(&mut self) {
        self.disks = disk_info();
        self.selected_disk = self.selected_disk.min(self.disks.len().saturating_sub(1));
    }
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn size_sort_key(size: SizeInfo) -> u64 {
    size.allocated
}

fn disk_info() -> Vec<DiskInfo> {
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if count <= 0 {
        return Vec::new();
    }

    let mut stats = Vec::<libc::statfs>::with_capacity(count as usize);
    let bytes = (stats.capacity() * std::mem::size_of::<libc::statfs>()) as libc::c_int;
    let actual = unsafe { libc::getfsstat(stats.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    if actual <= 0 {
        return Vec::new();
    }

    unsafe {
        stats.set_len(actual as usize);
    }

    stats
        .into_iter()
        .filter_map(|stat| {
            let mount = c_char_array_to_string(&stat.f_mntonname);
            if mount.is_empty() {
                return None;
            }
            let fs_type = c_char_array_to_string(&stat.f_fstypename);
            if matches!(fs_type.as_str(), "autofs" | "devfs") {
                return None;
            }
            let block_size = u64::from(stat.f_bsize);
            let total = stat.f_blocks.saturating_mul(block_size);
            if total == 0 {
                return None;
            }
            let available = stat.f_bavail.saturating_mul(block_size);
            Some(DiskInfo {
                name: c_char_array_to_string(&stat.f_mntfromname),
                mount: PathBuf::from(mount),
                total,
                available,
            })
        })
        .collect()
}

fn c_char_array_to_string(chars: &[libc::c_char]) -> String {
    if chars.is_empty() || chars[0] == 0 {
        return String::new();
    }
    unsafe { CStr::from_ptr(chars.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn delete_request_is_pinned_to_original_entry() {
        let root = test_root("delete_modal");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), b"a").unwrap();
        fs::write(root.join("b.txt"), b"bb").unwrap();

        let mut app = App::new(root.clone()).unwrap();
        let target = app.entries[0].path.clone();
        app.request_delete();

        let get_path = |t: &DeleteTarget| match t {
            DeleteTarget::FileEntry(e) => e.path.clone(),
            DeleteTarget::Package { path, .. } => path.clone(),
        };

        assert!(app.confirming_delete);
        assert_eq!(get_path(app.pending_delete.as_ref().unwrap()), target);
        app.move_cursor(1);
        assert_eq!(get_path(app.pending_delete.as_ref().unwrap()), target);

        app.cancel_delete();
        assert!(!app.confirming_delete);
        assert!(app.pending_delete.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn delete_request_only_applies_to_files_pane() {
        let root = test_root("delete_focus");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), b"a").unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.focus = Focus::Disks;
        app.request_delete();

        assert!(!app.confirming_delete);
        assert!(app.pending_delete.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn delete_request_applies_to_package_deps() {
        let root = test_root("delete_pkg_deps");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.project_deps = vec![ProjectDeps {
            path: root.clone(),
            manager_label: "cargo",
            manifest: "Cargo.toml",
            dep_count: 5,
            deps_size: None,
            deps_dir: Some(root.join("target")),
        }];
        app.focus = Focus::Packages;
        app.pkg_view = PkgView::ProjectDeps;
        app.selected_pkg = 0;

        app.request_delete();

        assert!(app.confirming_delete);
        match app.pending_delete.as_ref().unwrap() {
            DeleteTarget::Package {
                name,
                path,
                is_project_dep,
            } => {
                assert!(is_project_dep);
                assert_eq!(path, &root.join("target"));
                assert!(name.contains("dependency dir"));
            }
            _ => panic!("expected DeleteTarget::Package"),
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disk_pane_selection_can_open_mounts() {
        let root = test_root("disk_pane");
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.disks = vec![
            DiskInfo {
                name: String::from("first"),
                mount: first,
                total: 100,
                available: 50,
            },
            DiskInfo {
                name: String::from("second"),
                mount: second.clone(),
                total: 100,
                available: 25,
            },
        ];
        app.focus = Focus::Disks;

        app.move_cursor(1);
        assert_eq!(app.selected_disk, 1);
        app.enter().unwrap();

        assert_eq!(app.cwd, second);
        assert!(app.focus == Focus::Files);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hidden_toggle_filters_dotfiles() {
        let root = test_root("hidden");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("visible.txt"), b"visible").unwrap();
        fs::write(root.join(".hidden.txt"), b"hidden").unwrap();

        let mut app = App::new(root.clone()).unwrap();
        assert!(app.entries.iter().any(|entry| entry.name == "visible.txt"));
        assert!(!app.entries.iter().any(|entry| entry.name == ".hidden.txt"));

        app.toggle_hidden().unwrap();
        assert!(app.entries.iter().any(|entry| entry.name == ".hidden.txt"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn force_rescan_refreshes_entries_and_preserves_selection() {
        let root = test_root("refresh");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), b"a").unwrap();
        fs::write(root.join("c.txt"), b"ccc").unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.sort = SortMode::Name;
        app.apply_sort();
        app.move_cursor(1);
        let selected_before = app.entries[app.selected].path.clone();

        fs::write(root.join("b.txt"), b"bb").unwrap();
        app.force_rescan();

        assert!(app.entries.iter().any(|entry| entry.name == "b.txt"));
        assert_eq!(app.entries[app.selected].path, selected_before);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn go_up_reselects_previous_directory_in_parent() {
        let root = test_root("go_up_selection");
        let child = root.join("child");
        let sibling = root.join("sibling");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        fs::write(child.join("nested.txt"), b"x").unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.sort = SortMode::Name;
        app.apply_sort();
        let child_index = app
            .entries
            .iter()
            .position(|entry| entry.path == child)
            .unwrap();
        app.selected = child_index;

        app.enter().unwrap();
        app.go_up().unwrap();

        assert_eq!(app.cwd, root);
        assert_eq!(app.entries[app.selected].path, child);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn human_formats_binary_units() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1023), "1023 B");
        assert_eq!(human(1024), "1.0 KiB");
        assert_eq!(human(10 * 1024), "10 KiB");
        assert_eq!(human(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn c_char_array_to_string_stops_at_nul() {
        let bytes = [
            b'a' as libc::c_char,
            b'b' as libc::c_char,
            0,
            b'c' as libc::c_char,
        ];
        assert_eq!(c_char_array_to_string(&bytes), "ab");
        assert_eq!(c_char_array_to_string(&[]), "");
        assert_eq!(c_char_array_to_string(&[0]), "");
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("diskr_app_{name}_{}_{}", std::process::id(), nanos))
    }
}
