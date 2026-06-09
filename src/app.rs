use anyhow::{Context, Result};
use std::cmp::{Ordering, Reverse};
use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crate::bulkstat::SizeInfo;
use crate::packages::{self, ManagerReport, ProjectDeps};
use crate::scanner::{ScanId, ScanMsg, Scanner};

const SORT_DEBOUNCE: Duration = Duration::from_millis(100);
const AUTO_SCAN_LIMIT: usize = 4;

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
    pub name_lower: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
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

    pub search_mode: bool,
    pub search_query: String,
    pub search_matches: Vec<usize>,

    pub pkg_search_mode: bool,
    pub pkg_search_query: String,
    pub pkg_search_matches: Vec<usize>,

    pub scan_total: usize,
    pub scan_completed: usize,

    pub files_area: ratatui_core::layout::Rect,
    pub file_list_offset: usize,

    pending_delete: Option<DeleteTarget>,
    size_cache: HashMap<PathBuf, SizeInfo>,
    entry_index: HashMap<PathBuf, usize>,
    cached_flat_packages: Vec<(packages::Package, packages::Manager)>,
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
            search_mode: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            pkg_search_mode: false,
            pkg_search_query: String::new(),
            pkg_search_matches: Vec::new(),
            scan_total: 0,
            scan_completed: 0,
            files_area: ratatui_core::layout::Rect::default(),
            file_list_offset: 0,
            pending_delete: None,
            size_cache: HashMap::new(),
            entry_index: HashMap::new(),
            cached_flat_packages: Vec::new(),
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
        self.entry_index.clear();
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
            let file_type = meta.as_ref().map(|m| m.file_type());
            let is_dir = file_type
                .as_ref()
                .map(|file_type| file_type.is_dir())
                .unwrap_or(false);
            let is_symlink = file_type
                .as_ref()
                .map(|file_type| file_type.is_symlink())
                .unwrap_or(false);
            let size = if is_dir {
                self.size_cache.get(&path).copied()
            } else {
                meta.as_ref().and_then(|m| {
                    if m.file_type().is_file() || m.file_type().is_symlink() {
                        Some(SizeInfo::new(m.len(), m.blocks().saturating_mul(512)))
                    } else {
                        None
                    }
                })
            };
            let modified = meta.as_ref().and_then(|m| m.modified().ok());
            let name_lower = name.to_lowercase();
            let idx = self.entries.len();
            self.entry_index.insert(path.clone(), idx);
            self.entries.push(Entry {
                name,
                name_lower,
                path,
                is_dir,
                is_symlink,
                size,
                modified,
                scanning: false,
            });
        }
        self.apply_sort();
        self.rebuild_entry_index();
        Ok(())
    }

    fn rebuild_entry_index(&mut self) {
        self.entry_index.clear();
        for (i, entry) in self.entries.iter().enumerate() {
            self.entry_index.insert(entry.path.clone(), i);
        }
    }

    pub fn apply_sort(&mut self) {
        match self.sort {
            SortMode::Name => self.entries.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| natural_cmp(&a.name_lower, &b.name_lower))
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
                let item_count = self.visible_entry_count();
                if item_count == 0 {
                    return;
                }
                let n = item_count as i32;
                let s = self.selected as i32 + delta;
                self.selected = s.rem_euclid(n) as usize;
                self.scan_selected_missing_dir();
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

    pub fn page_move(&mut self, pages: i32) {
        let height = self.files_area.height.saturating_sub(2).max(1) as i32;
        self.move_cursor(pages * height);
    }

    pub fn move_to_start(&mut self) {
        match self.focus {
            Focus::Files => {
                self.selected = 0;
                self.scan_selected_missing_dir();
            }
            Focus::Disks => self.selected_disk = 0,
            Focus::Packages => self.selected_pkg = 0,
        }
    }

    pub fn move_to_end(&mut self) {
        match self.focus {
            Focus::Files => {
                let n = self.visible_entry_count();
                if n > 0 {
                    self.selected = n - 1;
                    self.scan_selected_missing_dir();
                }
            }
            Focus::Disks => {
                let n = self.disks.len();
                if n > 0 {
                    self.selected_disk = n - 1;
                }
            }
            Focus::Packages => {
                let n = self.pkg_item_count();
                if n > 0 {
                    self.selected_pkg = n - 1;
                }
            }
        }
    }

    pub fn visible_entry_count(&self) -> usize {
        if self.search_mode && !self.search_query.is_empty() {
            self.search_matches.len()
        } else {
            self.entries.len()
        }
    }

    pub fn visible_entry_index(&self, visible_index: usize) -> Option<usize> {
        if self.search_mode && !self.search_query.is_empty() {
            self.search_matches.get(visible_index).copied()
        } else if visible_index < self.entries.len() {
            Some(visible_index)
        } else {
            None
        }
    }

    pub fn visible_entry(&self, visible_index: usize) -> Option<&Entry> {
        self.visible_entry_index(visible_index)
            .and_then(|idx| self.entries.get(idx))
    }

    pub fn enter(&mut self) -> Result<()> {
        if self.confirming_delete {
            return Ok(());
        }
        match self.focus {
            Focus::Files => {
                if let Some(entry_idx) = self.visible_entry_index(self.selected) {
                    if let Some(entry) = self.entries.get(entry_idx).cloned() {
                        if entry.is_dir {
                            self.exit_search();
                            self.cwd = entry.path;
                            self.selected = 0;
                            self.reload()?;
                        }
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

    /// Scan only a small batch of nearby directories. Starting at `/` or other broad
    /// roots should not recursively walk every child before the user asks for it.
    fn auto_scan(&mut self) {
        let missing: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|e| e.is_dir && e.size.is_none())
            .map(|e| e.path.clone())
            .collect();
        if missing.is_empty() {
            self.status = String::from("cache hit · all sizes known");
            return;
        }
        let dirs = self.scan_candidates(AUTO_SCAN_LIMIT, &missing);
        let status = limited_scan_status("scanning", dirs.len(), missing.len());
        self.start_scan(dirs, status);
    }

    /// Invoked by the `r` key. Refreshes the directory view and rescans visible directories.
    pub fn force_rescan(&mut self) {
        if self.confirming_delete {
            return;
        }
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
        }
        let missing: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|e| e.is_dir)
            .map(|e| e.path.clone())
            .collect();
        if missing.is_empty() {
            self.status = String::from("refresh complete · no directories to rescan");
            return;
        }
        let dirs = self.scan_candidates(AUTO_SCAN_LIMIT, &missing);
        let status = limited_scan_status("refresh scan", dirs.len(), missing.len());
        self.start_scan(dirs, status);
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
                    if let Some(&idx) = self.entry_index.get(&path) {
                        if let Some(e) = self.entries.get_mut(idx) {
                            e.size = Some(size);
                            e.scanning = false;
                            changed = true;
                        }
                    }
                    self.scan_completed += 1;
                    if self.scan_completed < self.scan_total {
                        self.status = format!(
                            "scanning directories: {}/{}",
                            self.scan_completed, self.scan_total
                        );
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
                if let Some(entry_idx) = self.visible_entry_index(self.selected) {
                    if let Some(entry) = self.entries.get(entry_idx).cloned() {
                        self.pending_delete = Some(DeleteTarget::FileEntry(entry));
                        self.confirming_delete = true;
                    }
                }
            }
            Focus::Disks => {
                self.status = String::from("cannot delete a disk mount");
            }
            Focus::Packages => match self.pkg_view {
                PkgView::SystemManagers => {
                    let real_idx = self.pkg_visible_index(self.selected_pkg).unwrap_or(usize::MAX);
                    let packages = &self.cached_flat_packages;
                    if let Some((package, manager)) = packages.get(real_idx) {
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
                    let real_idx = self.pkg_visible_index(self.selected_pkg).unwrap_or(usize::MAX);
                    if let Some(dep) = self.project_deps.get(real_idx) {
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

    fn scan_candidates(&self, limit: usize, missing: &[PathBuf]) -> Vec<PathBuf> {
        if limit == 0 || missing.is_empty() {
            return Vec::new();
        }

        let missing: HashSet<&Path> = missing.iter().map(PathBuf::as_path).collect();
        let mut dirs = Vec::new();
        let mut push_if_missing = |entry: &Entry| {
            if dirs.len() < limit && entry.is_dir && missing.contains(entry.path.as_path()) {
                dirs.push(entry.path.clone());
            }
        };

        if let Some(entry) = self.entries.get(self.selected) {
            push_if_missing(entry);
        }
        for entry in self.entries.iter().skip(self.selected.saturating_add(1)) {
            push_if_missing(entry);
        }
        for entry in self.entries.iter().take(self.selected) {
            push_if_missing(entry);
        }
        dirs
    }

    fn scan_selected_missing_dir(&mut self) {
        let Some(entry) = self.entries.get(self.selected) else {
            return;
        };
        if !entry.is_dir || entry.size.is_some() || entry.scanning {
            return;
        }
        let name = entry.name.clone();
        let path = entry.path.clone();
        self.start_scan(vec![path], format!("scanning selected directory: {name}"));
    }

    fn start_scan(&mut self, dirs: Vec<PathBuf>, status: String) {
        if dirs.is_empty() {
            self.status = String::from("no directories to scan");
            return;
        }
        let scan_id = self.next_scan_id();
        self.scan_total = dirs.len();
        self.scan_completed = 0;
        let dir_set: HashSet<&Path> = dirs.iter().map(PathBuf::as_path).collect();
        for entry in &mut self.entries {
            entry.scanning = dir_set.contains(entry.path.as_path());
        }
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
                    self.rebuild_flat_packages();
                    self.selected_pkg = self
                        .selected_pkg
                        .min(self.pkg_item_count().saturating_sub(1));
                    self.packages_loading = false;
                    self.pkg_scan_rx = None;
                    self.status = if msg.include_managers {
                        let total = self.cached_flat_packages.len();
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
        if self.pkg_search_mode && !self.pkg_search_query.is_empty() {
            return self.pkg_search_matches.len();
        }
        match self.pkg_view {
            PkgView::SystemManagers => self.cached_flat_packages.len(),
            PkgView::ProjectDeps => self.project_deps.len(),
        }
    }

    pub fn pkg_visible_index(&self, visible_index: usize) -> Option<usize> {
        if self.pkg_search_mode && !self.pkg_search_query.is_empty() {
            self.pkg_search_matches.get(visible_index).copied()
        } else {
            Some(visible_index)
        }
    }

    pub fn enter_pkg_search(&mut self) {
        self.pkg_search_mode = true;
        self.pkg_search_query.clear();
        self.pkg_search_matches.clear();
    }

    pub fn exit_pkg_search(&mut self) {
        let real_index = self.pkg_visible_index(self.selected_pkg);
        self.pkg_search_mode = false;
        self.pkg_search_query.clear();
        self.pkg_search_matches.clear();
        if let Some(idx) = real_index {
            self.selected_pkg = idx;
        }
    }

    pub fn pkg_search_push(&mut self, ch: char) {
        self.pkg_search_query.push(ch);
        self.update_pkg_search();
    }

    pub fn pkg_search_pop(&mut self) {
        self.pkg_search_query.pop();
        if self.pkg_search_query.is_empty() {
            self.pkg_search_matches.clear();
            self.selected_pkg = 0;
        } else {
            self.update_pkg_search();
        }
    }

    fn update_pkg_search(&mut self) {
        let query = self.pkg_search_query.to_lowercase();
        self.pkg_search_matches = match self.pkg_view {
            PkgView::SystemManagers => self
                .cached_flat_packages
                .iter()
                .enumerate()
                .filter(|(_, (pkg, mgr))| {
                    pkg.name.to_lowercase().contains(&query)
                        || mgr.label().contains(&query)
                })
                .map(|(i, _)| i)
                .collect(),
            PkgView::ProjectDeps => self
                .project_deps
                .iter()
                .enumerate()
                .filter(|(_, dep)| {
                    dep.path
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&query)
                        || dep.manager_label.contains(&query)
                })
                .map(|(i, _)| i)
                .collect(),
        };
        self.selected_pkg = 0;
    }

    pub fn flat_packages(&self) -> &[(packages::Package, packages::Manager)] {
        &self.cached_flat_packages
    }

    pub fn total_pkg_size(&self) -> u64 {
        self.pkg_reports
            .iter()
            .filter(|r| r.available)
            .map(|r| r.total_size.allocated)
            .sum()
    }

    pub fn total_project_deps_size(&self) -> u64 {
        self.project_deps
            .iter()
            .filter_map(|d| d.deps_size.map(|s| s.allocated))
            .sum()
    }

    pub fn rebuild_flat_packages(&mut self) {
        let mut pkgs: Vec<(packages::Package, packages::Manager)> = Vec::new();
        for report in &self.pkg_reports {
            if !report.available {
                continue;
            }
            for pkg in &report.packages {
                pkgs.push((pkg.clone(), report.manager));
            }
        }
        pkgs.sort_by(|(a, _), (b, _)| {
            let a_size = a.size.map(|s| s.allocated).unwrap_or(0);
            let b_size = b.size.map(|s| s.allocated).unwrap_or(0);
            b_size.cmp(&a_size).then(a.name.cmp(&b.name))
        });
        self.cached_flat_packages = pkgs;
    }

    pub fn refresh_disks(&mut self) {
        self.disks = disk_info();
        self.selected_disk = self.selected_disk.min(self.disks.len().saturating_sub(1));
    }

    pub fn enter_search(&mut self) {
        self.search_mode = true;
        self.search_query.clear();
        self.search_matches.clear();
        self.file_list_offset = 0;
    }

    pub fn exit_search(&mut self) {
        let selected_path = self
            .visible_entry_index(self.selected)
            .map(|entry_idx| self.entries.get(entry_idx).map(|entry| entry.path.clone()));
        self.search_mode = false;
        self.search_query.clear();
        self.search_matches.clear();
        self.file_list_offset = 0;
        if let Some(Some(path)) = selected_path {
            if let Some(index) = self.entries.iter().position(|entry| entry.path == path) {
                self.selected = index;
            }
        }
    }

    pub fn search_push(&mut self, ch: char) {
        self.search_query.push(ch);
        self.update_search();
    }

    pub fn search_pop(&mut self) {
        self.search_query.pop();
        if self.search_query.is_empty() {
            self.search_matches.clear();
            self.selected = 0;
            self.file_list_offset = 0;
        } else {
            self.update_search();
        }
    }

    fn update_search(&mut self) {
        let query = self.search_query.to_lowercase();
        self.search_matches = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.name_lower.contains(&query))
            .map(|(i, _)| i)
            .collect();
        self.selected = 0;
        self.file_list_offset = 0;
    }

    #[allow(dead_code)]
    pub fn copy_path_to_clipboard(&mut self) {
        let Some(path) = self.selected_path() else {
            self.status = String::from("nothing selected");
            return;
        };
        match copy_to_clipboard(&path.display().to_string()) {
            Ok(()) => self.status = format!("copied: {}", path.display()),
            Err(e) => self.status = format!("copy failed: {e}"),
        }
    }

    #[allow(dead_code)]
    pub fn open_shell(&mut self) -> Result<()> {
        let shell = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/bin/zsh"));
        Command::new(&shell)
            .current_dir(&self.cwd)
            .spawn()
            .context("open shell")?;
        self.status = format!("opened {}", shell.display());
        Ok(())
    }

    #[allow(dead_code)]
    fn selected_path(&self) -> Option<PathBuf> {
        match self.focus {
            Focus::Files => self.visible_entry(self.selected).map(|e| e.path.clone()),
            Focus::Disks => self.disks.get(self.selected_disk).map(|d| d.mount.clone()),
            Focus::Packages => {
                let real_idx = self.pkg_visible_index(self.selected_pkg)?;
                match self.pkg_view {
                    PkgView::SystemManagers => self
                        .cached_flat_packages
                        .get(real_idx)
                        .and_then(|(p, _)| p.path.clone()),
                    PkgView::ProjectDeps => self
                        .project_deps
                        .get(real_idx)
                        .map(|d| d.deps_dir.as_ref().unwrap_or(&d.path).clone()),
                }
            }
        }
    }
}

fn limited_scan_status(action: &str, scanning: usize, total: usize) -> String {
    if scanning >= total {
        format!("{action}: {total} directories")
    } else {
        format!("{action}: {scanning}/{total} directories · move or r to scan more")
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
            if !should_show_disk_mount(&mount, &fs_type) {
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

fn should_show_disk_mount(mount: &str, fs_type: &str) -> bool {
    if matches!(fs_type, "autofs" | "devfs") {
        return false;
    }
    !mount.starts_with("/System/Volumes/") && !mount.starts_with("/private/var/")
}

fn c_char_array_to_string(chars: &[libc::c_char]) -> String {
    if chars.is_empty() || chars[0] == 0 {
        return String::new();
    }
    unsafe { CStr::from_ptr(chars.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

#[allow(dead_code)]
fn copy_to_clipboard(text: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .context("spawn pbcopy")?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(text.as_bytes())
            .context("write to pbcopy")?;
    }
    child.wait().context("wait for pbcopy")?;
    Ok(())
}

fn natural_cmp(a: &str, b: &str) -> Ordering {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let mut ai = 0;
    let mut bi = 0;

    while ai < a.len() && bi < b.len() {
        let ac = a[ai];
        let bc = b[bi];
        if ac.is_ascii_digit() && bc.is_ascii_digit() {
            let a_start = ai;
            let b_start = bi;
            while ai < a.len() && a[ai].is_ascii_digit() {
                ai += 1;
            }
            while bi < b.len() && b[bi].is_ascii_digit() {
                bi += 1;
            }

            let a_digits = &a[a_start..ai];
            let b_digits = &b[b_start..bi];
            let a_sig = trim_leading_zeroes(a_digits);
            let b_sig = trim_leading_zeroes(b_digits);
            match a_sig.len().cmp(&b_sig.len()) {
                Ordering::Equal => match a_sig.cmp(b_sig) {
                    Ordering::Equal => match a_digits.len().cmp(&b_digits.len()) {
                        Ordering::Equal => {}
                        ordering => return ordering,
                    },
                    ordering => return ordering,
                },
                ordering => return ordering,
            }
            continue;
        }

        match ac.cmp(&bc) {
            Ordering::Equal => {
                ai += 1;
                bi += 1;
            }
            ordering => return ordering,
        }
    }

    a.len().cmp(&b.len())
}

fn trim_leading_zeroes(digits: &[u8]) -> &[u8] {
    let trimmed = digits.iter().position(|digit| *digit != b'0');
    match trimmed {
        Some(index) => &digits[index..],
        None => &digits[digits.len().saturating_sub(1)..],
    }
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
    fn initial_scan_is_bounded_for_broad_directories() {
        let root = test_root("bounded_scan");
        fs::create_dir_all(&root).unwrap();
        for i in 0..8 {
            fs::create_dir_all(root.join(format!("dir-{i}"))).unwrap();
        }

        let app = App::new(root.clone()).unwrap();
        let scanning_count = app.entries.iter().filter(|entry| entry.scanning).count();

        assert_eq!(scanning_count, AUTO_SCAN_LIMIT);
        assert!(app.status.contains("4/8 directories"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn moving_to_unscanned_directory_starts_selected_scan() {
        let root = test_root("selected_scan");
        fs::create_dir_all(&root).unwrap();
        for i in 0..6 {
            fs::create_dir_all(root.join(format!("dir-{i}"))).unwrap();
        }

        let mut app = App::new(root.clone()).unwrap();
        app.sort = SortMode::Name;
        app.apply_sort();
        app.selected = 5;
        for entry in &mut app.entries {
            entry.scanning = false;
        }

        app.move_cursor(0);

        assert!(app.entries[app.selected].scanning);
        assert!(app.status.contains("scanning selected directory"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn name_sort_uses_natural_numeric_order() {
        let root = test_root("natural_sort");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("file-10.txt"), b"x").unwrap();
        fs::write(root.join("file-2.txt"), b"x").unwrap();
        fs::write(root.join("file-1.txt"), b"x").unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.sort = SortMode::Name;
        app.apply_sort();
        let names: Vec<_> = app
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();

        assert_eq!(names, vec!["file-1.txt", "file-2.txt", "file-10.txt"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn symlinks_are_marked_and_sized() {
        let root = test_root("symlink");
        fs::create_dir_all(&root).unwrap();
        let target = root.join("target.txt");
        let link = root.join("link.txt");
        fs::write(&target, b"x").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let app = App::new(root.clone()).unwrap();
        let entry = app.entries.iter().find(|entry| entry.path == link).unwrap();

        assert!(entry.is_symlink);
        assert!(entry.size.is_some());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn search_exit_preserves_selected_match() {
        let root = test_root("search");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("alpha.txt"), b"x").unwrap();
        fs::write(root.join("beta.txt"), b"x").unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.sort = SortMode::Name;
        app.apply_sort();
        app.enter_search();
        app.search_push('b');
        app.exit_search();

        assert_eq!(app.entries[app.selected].name, "beta.txt");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disk_filter_hides_macos_system_volumes() {
        assert!(!should_show_disk_mount("/System/Volumes/VM", "apfs"));
        assert!(!should_show_disk_mount("/System/Volumes/Data", "apfs"));
        assert!(!should_show_disk_mount("/private/var/run", "apfs"));
        assert!(!should_show_disk_mount("/dev", "devfs"));
        assert!(should_show_disk_mount("/", "apfs"));
        assert!(should_show_disk_mount("/Volumes/External", "apfs"));
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
