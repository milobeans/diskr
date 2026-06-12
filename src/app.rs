use anyhow::{Context, Result};
use std::cmp::{Ordering, Reverse};
use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::bulkstat::{self, DirScan, SizeInfo};
use crate::packages::{self, DepGraph, ManagerReport, ProjectDeps};
use crate::scanner::{ScanId, ScanMsg, Scanner};
use crate::{history, reclaim, space, state};

const SORT_DEBOUNCE: Duration = Duration::from_millis(100);
const SIZE_CACHE_SAVE_INTERVAL: Duration = Duration::from_secs(60);
const AUTO_SCAN_LIMIT: usize = 4;
const TOP_FILES_LIMIT: usize = 50;
const TOP_FILES_PAGE_ROWS: i32 = 10;
const RECLAIM_PATHS_PAGE_ROWS: i32 = 10;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Files,
    Disks,
    Packages,
    Reclaim,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Name,
    SizeDesc,
    Modified,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    None,
    Rename,
    Mkdir,
}

impl SortMode {
    pub fn label(self) -> &'static str {
        match self {
            SortMode::Name => "name",
            SortMode::SizeDesc => "size ↓",
            SortMode::Modified => "mtime",
        }
    }
}

#[derive(Clone)]
pub enum InputAction {
    Rename(PathBuf),
    Mkdir(PathBuf),
}

#[derive(Clone)]
pub struct Entry {
    pub name: String,
    pub name_lower: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: Option<SizeInfo>,
    pub size_stale: bool,
    pub cached_at: Option<u64>,
    pub inaccessible: u32,
    pub modified: Option<SystemTime>,
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
    TopFile {
        name: String,
        path: PathBuf,
    },
    ReclaimPath {
        finding_index: usize,
        name: String,
        path: PathBuf,
    },
    Batch,
}

struct ReclaimMsg {
    scan_id: ScanId,
    root: PathBuf,
    report: reclaim::ReclaimReport,
}

struct TopFilesMsg {
    scan_id: ScanId,
    path: PathBuf,
    scan: DirScan,
}

struct DiskInfoMsg {
    scan_id: ScanId,
    result: Result<space::SpaceReport, String>,
}

#[derive(Clone)]
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
    project_deps_cwd: Option<PathBuf>,
    pub pkg_view: PkgView,
    pub packages_loaded: bool,
    pub packages_loading: bool,

    pub input_mode: InputMode,
    pub input_prompt: String,
    pub input_buffer: String,
    pub input_on_commit: Option<InputAction>,

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
    marked: HashSet<PathBuf>,
    size_cache: HashMap<PathBuf, SizeInfo>,
    inaccessible_cache: HashMap<PathBuf, u32>,
    cache_age: HashMap<PathBuf, u64>,
    stale_size_cache: HashSet<PathBuf>,
    size_cache_dirty: bool,
    last_size_cache_save: Instant,
    entry_index: HashMap<PathBuf, usize>,
    cached_flat_packages: Vec<(packages::Package, packages::Manager)>,
    last_sort: Instant,
    sort_dirty: bool,
    active_scan_id: ScanId,

    active_reclaim_scan_id: ScanId,
    reclaim_scan_rx: Option<Receiver<ReclaimMsg>>,
    pub reclaim_report: Option<reclaim::ReclaimReport>,
    reclaim_cwd: Option<PathBuf>,
    pub reclaim_loading: bool,
    pub selected_reclaim: usize,

    reclaim_paths_open: bool,
    reclaim_paths_selected: usize,
    reclaim_paths_finding: usize,
    reclaim_path_list_offset: usize,

    top_files_open: bool,
    top_files_loading: bool,
    top_files_scan_id: ScanId,
    top_files_scan_rx: Option<Receiver<TopFilesMsg>>,
    top_files_scan: Option<DirScan>,
    top_files_path: Option<PathBuf>,
    pub top_files_selected: usize,
    top_files_offset: usize,

    disk_info_open: bool,
    disk_info_loading: bool,
    disk_info_id: ScanId,
    disk_info_scan_rx: Option<Receiver<DiskInfoMsg>>,
    pub disk_info_report: Option<space::SpaceReport>,

    pub history_baseline: Option<history::ScanRecord>,
    pub history_diff: Option<history::DiffReport>,

    scan_rx: Receiver<ScanMsg>,
    scanner: Scanner,
    active_pkg_scan_id: ScanId,
    pkg_scan_rx: Option<Receiver<PkgScanMsg>>,

    pub dep_graph: Option<DepGraph>,
    pub deps_loading: bool,
    dep_scan_rx: Option<Receiver<DepScanMsg>>,

    pub pkg_detail: bool,
    pub pkg_show_unused: bool,
    pub confirming_uninstall: bool,
    pending_uninstall: Option<UninstallTarget>,
    uninstall_rx: Option<Receiver<UninstallResult>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PkgView {
    SystemManagers,
    ProjectDeps,
}

struct PkgScanMsg {
    scan_id: ScanId,
    cwd: PathBuf,
    reports: Vec<ManagerReport>,
    project_deps: Vec<ProjectDeps>,
    include_managers: bool,
}

struct DepScanMsg {
    graph: DepGraph,
}

pub struct UninstallTarget {
    pub manager: packages::Manager,
    pub name: String,
    pub display_name: String,
}

struct UninstallResult {
    display_name: String,
    result: Result<String, String>,
}

impl PkgView {
    // Methods related to PkgView...
}

impl App {
    pub fn new(cwd: PathBuf) -> Result<Self> {
        let (mut size_cache, mut inaccessible_cache, mut cache_age, mut stale_size_cache) = (
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashSet::new(),
        );
        let mut size_cache_dirty = false;
        let cache_warning = match state::load_size_cache() {
            Ok(mut entries) => {
                if entries.len() > state::SIZE_CACHE_MAX_ENTRIES {
                    entries.sort_by(|a, b| {
                        b.scanned_at
                            .cmp(&a.scanned_at)
                            .then_with(|| a.path.cmp(&b.path))
                    });
                    entries.truncate(state::SIZE_CACHE_MAX_ENTRIES);
                    size_cache_dirty = true;
                }
                for entry in entries {
                    if entry.inaccessible > 0 {
                        inaccessible_cache.insert(entry.path.clone(), entry.inaccessible);
                    }
                    cache_age.insert(entry.path.clone(), entry.scanned_at);
                    stale_size_cache.insert(entry.path.clone());
                    size_cache.insert(entry.path, entry.size);
                }
                None
            }
            Err(err) => Some(format!("size cache ignored: {err}")),
        };
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
            project_deps_cwd: None,
            pkg_view: PkgView::SystemManagers,
            packages_loaded: false,
            packages_loading: false,
            input_mode: InputMode::None,
            input_prompt: String::new(),
            input_buffer: String::new(),
            input_on_commit: None,
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
            marked: HashSet::new(),
            size_cache,
            inaccessible_cache,
            cache_age,
            stale_size_cache,
            size_cache_dirty,
            last_size_cache_save: Instant::now(),
            entry_index: HashMap::new(),
            cached_flat_packages: Vec::new(),
            last_sort: Instant::now(),
            sort_dirty: false,
            active_scan_id: 0,
            active_reclaim_scan_id: 0,
            reclaim_scan_rx: None,
            reclaim_report: None,
            reclaim_cwd: None,
            reclaim_loading: false,
            selected_reclaim: 0,
            reclaim_paths_open: false,
            reclaim_paths_selected: 0,
            reclaim_paths_finding: 0,
            reclaim_path_list_offset: 0,
            top_files_open: false,
            top_files_loading: false,
            top_files_scan_id: 0,
            top_files_scan_rx: None,
            top_files_scan: None,
            top_files_path: None,
            top_files_selected: 0,
            top_files_offset: 0,
            disk_info_open: false,
            disk_info_loading: false,
            disk_info_id: 0,
            disk_info_scan_rx: None,
            disk_info_report: None,
            history_baseline: None,
            history_diff: None,
            scanner: Scanner::new(tx.clone()),
            scan_rx: rx,
            active_pkg_scan_id: 0,
            pkg_scan_rx: None,
            dep_graph: None,
            deps_loading: false,
            dep_scan_rx: None,
            pkg_detail: false,
            pkg_show_unused: false,
            confirming_uninstall: false,
            pending_uninstall: None,
            uninstall_rx: None,
        };
        app.refresh_disks();
        app.reload()?;
        app.refresh_history_state();
        if let Some(warning) = cache_warning {
            app.status = warning;
        }
        Ok(app)
    }

    pub fn refresh_history_state(&mut self) {
        self.history_baseline = history::load_record_for_path(&self.cwd).unwrap_or(None);
        self.history_diff = if self.history_baseline.is_some() {
            history::diff(&self.cwd).ok()
        } else {
            None
        };
    }

    pub fn history_baseline_status(&self) -> Option<String> {
        let record = self.history_baseline.as_ref()?;
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|now| now.as_secs())
            .unwrap_or(0);
        let age = now_secs.saturating_sub(record.timestamp);
        Some(format!("baseline saved {}", format_elapsed(age)))
    }

    pub fn history_delta_status(&self) -> Option<String> {
        let diff = self.history_diff.as_ref()?;
        let delta = diff.total_delta_allocated();
        let bytes = delta.unsigned_abs().min(u128::from(u64::MAX)) as u64;
        let delta_label = if delta > 0 {
            format!("+{}", human(bytes))
        } else if delta < 0 {
            format!("-{}", human(bytes))
        } else {
            String::from("±0")
        };
        let age = format_elapsed(
            diff.current_timestamp
                .saturating_sub(diff.baseline_timestamp),
        );
        Some(format!("{delta_label} in {age}"))
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
            let inaccessible = self.inaccessible_cache.get(&path).copied().unwrap_or(0);
            let size_stale = is_dir && self.stale_size_cache.contains(&path);
            let cached_at = if is_dir {
                self.cache_age.get(&path).copied()
            } else {
                None
            };
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
                size_stale,
                cached_at,
                inaccessible,
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
        let selected_path = self
            .visible_entry(self.selected)
            .map(|entry| entry.path.clone());
        let fallback_selected = self.selected.min(self.entries.len().saturating_sub(1));
        self.apply_sort();
        if let Some(path) = selected_path {
            if self.search_mode && !self.search_query.is_empty() {
                self.update_search();
                self.selected = self
                    .search_matches
                    .iter()
                    .position(|&idx| self.entries[idx].path == path)
                    .unwrap_or(0);
            } else if let Some(idx) = self.entries.iter().position(|e| e.path == path) {
                self.selected = idx;
            } else {
                self.selected = fallback_selected;
            }
        } else {
            self.selected = fallback_selected;
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
            Focus::Reclaim => {
                let n = self.reclaim_item_count() as i32;
                if n == 0 {
                    return;
                }
                let s = self.selected_reclaim as i32 + delta;
                self.selected_reclaim = s.rem_euclid(n) as usize;
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
            Focus::Reclaim => self.selected_reclaim = 0,
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
            Focus::Reclaim => {
                let n = self.reclaim_item_count();
                if n > 0 {
                    self.selected_reclaim = n - 1;
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
                            self.refresh_history_state();
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
                    self.refresh_history_state();
                }
            }
            Focus::Packages => {}
            Focus::Reclaim => {
                self.open_selected_reclaim_paths();
            }
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
            self.refresh_history_state();
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
        let cached_dirs: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|e| e.is_dir)
            .map(|e| e.path.clone())
            .collect();
        for path in cached_dirs {
            self.remove_cached_size(&path);
        }
        self.refresh_disks();
        if self.rebuild_entries().is_err() {
            return;
        }
        self.restore_selection(previous_selected, previous_index);
        for e in self.entries.iter_mut().filter(|entry| entry.is_dir) {
            e.size = None;
            e.size_stale = false;
            e.cached_at = None;
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
        let dirs = self.scan_candidates(missing.len(), &missing);
        let status = limited_scan_status("refresh scan", dirs.len(), missing.len());
        self.start_scan(dirs, status);
    }

    /// Invoked by the `S` key. Scans every visible directory whose size is not known yet.
    pub fn scan_all_missing_visible(&mut self) {
        if self.confirming_delete {
            return;
        }
        let missing: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|e| e.is_dir && e.size.is_none())
            .map(|e| e.path.clone())
            .collect();
        if missing.is_empty() {
            self.status = String::from("full scan complete · all visible sizes known");
            return;
        }
        let dirs = self.scan_candidates(missing.len(), &missing);
        let status = limited_scan_status("full scan", dirs.len(), missing.len());
        self.start_scan(dirs, status);
    }

    pub fn drain_scan_results(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.scan_rx.try_recv() {
            match msg {
                ScanMsg::DirStarted { scan_id, path } if scan_id == self.active_scan_id => {
                    self.status = format!(
                        "scanning {}: {}/{}",
                        scan_path_label(&path),
                        self.scan_completed,
                        self.scan_total
                    );
                    changed = true;
                }
                ScanMsg::DirSize {
                    scan_id,
                    path,
                    size,
                    inaccessible,
                } if scan_id == self.active_scan_id => {
                    let scanned_at = now_secs();
                    self.record_cached_size(path.clone(), size, inaccessible, scanned_at, false);
                    if let Some(&idx) = self.entry_index.get(&path) {
                        if let Some(e) = self.entries.get_mut(idx) {
                            e.size = Some(size);
                            e.size_stale = false;
                            e.cached_at = Some(scanned_at);
                            e.inaccessible = inaccessible;
                            e.scanning = false;
                            changed = true;
                        }
                    }
                    self.scan_completed += 1;
                    if self.scan_completed < self.scan_total {
                        self.status = format!(
                            "scanned {}: {}/{}",
                            scan_path_label(&path),
                            self.scan_completed,
                            self.scan_total
                        );
                    }
                    if self.sort == SortMode::SizeDesc {
                        self.sort_dirty = true;
                    }
                }
                ScanMsg::AllDone { scan_id } if scan_id == self.active_scan_id => {
                    for entry in &mut self.entries {
                        entry.scanning = false;
                    }
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
        if self.size_cache_dirty && self.last_size_cache_save.elapsed() >= SIZE_CACHE_SAVE_INTERVAL
        {
            match self.save_size_cache() {
                Ok(()) => changed = true,
                Err(err) => {
                    self.status = format!("size cache save failed: {err}");
                    changed = true;
                }
            }
        }
        changed |= self.drain_package_results();
        changed |= self.drain_dep_results();
        changed |= self.drain_reclaim_results();
        changed |= self.drain_top_files_results();
        changed |= self.drain_disk_info_results();
        changed
    }

    pub fn save_history_baseline(&mut self) -> Result<()> {
        let record = history::save(&self.cwd)?;
        self.history_baseline = Some(record);
        self.history_diff = None;
        self.status = String::from("baseline saved");
        Ok(())
    }

    pub fn top_files_open(&self) -> bool {
        self.top_files_open
    }

    pub fn reclaim_paths_open(&self) -> bool {
        self.reclaim_paths_open
    }

    pub fn disk_info_open(&self) -> bool {
        self.disk_info_open
    }

    pub fn top_files_loading(&self) -> bool {
        self.top_files_loading
    }

    pub fn disk_info_loading(&self) -> bool {
        self.disk_info_loading
    }

    pub fn reclaim_paths_selected(&self) -> usize {
        self.reclaim_paths_selected
    }

    pub fn set_top_files_selected(&mut self, index: usize) {
        self.top_files_selected = index.min(self.top_files_count().saturating_sub(1));
    }

    pub fn set_reclaim_paths_selected(&mut self, index: usize) {
        self.reclaim_paths_selected = index.min(self.reclaim_paths_count().saturating_sub(1));
    }

    pub fn top_files_path(&self) -> Option<&Path> {
        self.top_files_path.as_deref()
    }

    pub fn top_files_scan(&self) -> Option<&DirScan> {
        self.top_files_scan.as_ref()
    }

    fn drain_reclaim_results(&mut self) -> bool {
        let recv = match self.reclaim_scan_rx.as_ref() {
            Some(rx) => rx.try_recv(),
            None => return false,
        };

        match recv {
            Ok(msg) if msg.scan_id == self.active_reclaim_scan_id => {
                self.reclaim_loading = false;
                self.reclaim_scan_rx = None;
                if msg.root != self.cwd {
                    self.status = String::from("ignored stale reclaim scan");
                    return true;
                }
                self.reclaim_report = Some(msg.report);
                self.reclaim_cwd = Some(msg.root);
                self.reclaim_paths_open = false;
                self.reclaim_paths_selected = 0;
                self.reclaim_path_list_offset = 0;
                self.selected_reclaim = self
                    .selected_reclaim
                    .min(self.reclaim_item_count().saturating_sub(1));
                self.status = String::from("reclaim scan complete");
                true
            }
            Ok(_) => true,
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.reclaim_loading = false;
                self.reclaim_scan_rx = None;
                self.status = String::from("reclaim scan failed");
                true
            }
        }
    }

    fn drain_top_files_results(&mut self) -> bool {
        let recv = match self.top_files_scan_rx.as_ref() {
            Some(rx) => rx.try_recv(),
            None => return false,
        };

        match recv {
            Ok(msg) if msg.scan_id == self.top_files_scan_id => {
                self.top_files_scan = Some(msg.scan);
                self.top_files_path = Some(msg.path);
                self.top_files_loading = false;
                self.top_files_scan_rx = None;
                self.top_files_selected = 0;
                self.top_files_offset = 0;
                self.status = String::from("top files scan complete");
                true
            }
            Ok(_) => true,
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.top_files_loading = false;
                self.top_files_scan_rx = None;
                self.status = String::from("top files scan failed");
                true
            }
        }
    }

    fn drain_disk_info_results(&mut self) -> bool {
        let recv = match self.disk_info_scan_rx.as_ref() {
            Some(rx) => rx.try_recv(),
            None => return false,
        };

        match recv {
            Ok(msg) if msg.scan_id == self.disk_info_id => {
                self.disk_info_loading = false;
                self.disk_info_scan_rx = None;
                match msg.result {
                    Ok(report) => {
                        self.disk_info_report = Some(report);
                        self.disk_info_open = true;
                        self.status = String::new();
                    }
                    Err(err) => {
                        self.disk_info_report = None;
                        self.disk_info_open = true;
                        self.status = format!("failed to load disk info: {err}");
                    }
                }
                true
            }
            Ok(_) => true,
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.disk_info_loading = false;
                self.disk_info_scan_rx = None;
                self.disk_info_open = true;
                self.status = String::from("disk info scan failed");
                true
            }
        }
    }

    fn next_reclaim_scan_id(&mut self) -> ScanId {
        self.active_reclaim_scan_id = self.active_reclaim_scan_id.saturating_add(1);
        self.active_reclaim_scan_id
    }

    fn next_top_files_scan_id(&mut self) -> ScanId {
        self.top_files_scan_id = self.top_files_scan_id.saturating_add(1);
        self.top_files_scan_id
    }

    fn next_disk_info_scan_id(&mut self) -> ScanId {
        self.disk_info_id = self.disk_info_id.saturating_add(1);
        self.disk_info_id
    }

    pub fn reclaim_item_count(&self) -> usize {
        self.reclaim_report
            .as_ref()
            .map(|report| report.findings.len())
            .unwrap_or(0)
    }

    pub fn reclaim_paths_count(&self) -> usize {
        self.reclaim_report
            .as_ref()
            .and_then(|report| report.findings.get(self.reclaim_paths_finding))
            .map(|finding| finding.paths.len())
            .unwrap_or(0)
    }

    pub fn move_reclaim_paths(&mut self, delta: i32) {
        let n = self.reclaim_paths_count() as i32;
        if n == 0 {
            return;
        }
        let s = self.reclaim_paths_selected as i32 + delta;
        self.reclaim_paths_selected = s.rem_euclid(n) as usize;
    }

    pub fn page_reclaim_paths(&mut self, pages: i32) {
        self.move_reclaim_paths(pages.saturating_mul(RECLAIM_PATHS_PAGE_ROWS));
    }

    pub fn reclaim_paths_window_bounds(&mut self, max_rows: usize) -> (usize, usize) {
        let (offset, end) = modal_window_bounds(
            self.reclaim_paths_selected,
            self.reclaim_paths_count(),
            self.reclaim_path_list_offset,
            max_rows,
        );
        self.reclaim_path_list_offset = offset;
        (offset, end)
    }

    pub fn top_files_count(&self) -> usize {
        self.top_files_scan
            .as_ref()
            .map(|scan| scan.largest_files.len())
            .unwrap_or(0)
    }

    pub fn move_top_files(&mut self, delta: i32) {
        let n = self.top_files_count() as i32;
        if n == 0 {
            return;
        }
        let s = self.top_files_selected as i32 + delta;
        self.top_files_selected = s.rem_euclid(n) as usize;
    }

    pub fn page_top_files(&mut self, pages: i32) {
        self.move_top_files(pages.saturating_mul(TOP_FILES_PAGE_ROWS));
    }

    pub fn top_files_selected(&self) -> usize {
        self.top_files_selected
    }

    pub fn top_files_window_bounds(&mut self, max_rows: usize) -> (usize, usize) {
        let (offset, end) = modal_window_bounds(
            self.top_files_selected,
            self.top_files_count(),
            self.top_files_offset,
            max_rows,
        );
        self.top_files_offset = offset;
        (offset, end)
    }

    pub fn selected_top_file_path(&self) -> Option<PathBuf> {
        let idx = self.top_files_selected;
        self.top_files_scan
            .as_ref()
            .and_then(|scan| scan.largest_files.get(idx))
            .map(|file| file.path.clone())
    }

    pub fn selected_reclaim_path(&self) -> Option<(String, PathBuf)> {
        self.reclaim_report.as_ref().and_then(|report| {
            report
                .findings
                .get(self.reclaim_paths_finding)
                .and_then(|finding| finding.paths.get(self.reclaim_paths_selected))
                .map(|path| {
                    let name = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(|name| name.to_string())
                        .unwrap_or_else(|| path.display().to_string());
                    (name, path.clone())
                })
        })
    }

    pub fn open_reclaim_for_focus(&mut self) {
        if self.reclaim_loading {
            return;
        }
        if self.reclaim_report.is_some() && self.reclaim_cwd.as_deref() == Some(self.cwd.as_path())
        {
            return;
        }
        self.request_reclaim_scan();
    }

    pub fn selected_reclaim_finding(&self) -> Option<&reclaim::Finding> {
        self.reclaim_report
            .as_ref()
            .and_then(|report| report.findings.get(self.selected_reclaim))
    }

    pub fn request_reclaim_scan(&mut self) {
        if self.reclaim_loading {
            return;
        }
        let cwd = self.cwd.clone();
        let scan_id = self.next_reclaim_scan_id();
        let (tx, rx) = std::sync::mpsc::channel();

        self.reclaim_loading = true;
        self.reclaim_report = None;
        self.reclaim_cwd = Some(cwd.clone());
        self.selected_reclaim = 0;
        self.reclaim_paths_open = false;
        self.reclaim_paths_selected = 0;
        self.reclaim_path_list_offset = 0;
        self.reclaim_scan_rx = Some(rx);
        self.status = String::from("scanning reclaim recommendations…");

        thread::spawn(move || {
            let report = reclaim::report(&cwd);
            let _ = tx.send(ReclaimMsg {
                scan_id,
                root: cwd,
                report,
            });
        });
    }

    pub fn open_selected_reclaim_paths(&mut self) {
        if self.reclaim_item_count() == 0 {
            self.status = String::from("no reclaim findings loaded");
            return;
        }
        self.reclaim_paths_open = true;
        self.reclaim_paths_selected = 0;
        self.reclaim_path_list_offset = 0;
        self.reclaim_paths_finding = self
            .selected_reclaim
            .min(self.reclaim_item_count().saturating_sub(1));
        let label = self
            .reclaim_report
            .as_ref()
            .and_then(|report| report.findings.get(self.reclaim_paths_finding))
            .map(|finding| finding.label.as_str())
            .unwrap_or("finding");
        self.status = format!("opened reclaim paths: {label}");
    }

    pub fn request_delete_reclaim_path(&mut self) {
        if let Some((name, path)) = self.selected_reclaim_path() {
            self.pending_delete = Some(DeleteTarget::ReclaimPath {
                finding_index: self.reclaim_paths_finding,
                name,
                path,
            });
            self.confirming_delete = true;
            return;
        }
        self.status = String::from("no reclaim path selected");
    }

    pub fn rescan_reclaim_finding(&mut self, finding_index: usize) {
        if self.reclaim_item_count() == 0 {
            return;
        }
        self.selected_reclaim = finding_index.min(self.reclaim_item_count().saturating_sub(1));
        self.request_reclaim_scan();
    }

    pub fn request_delete_top_file(&mut self) {
        if self.top_files_loading {
            self.status = String::from("top files scan in progress");
            return;
        }
        let Some(path) = self.selected_top_file_path() else {
            self.status = String::from("no top file selected");
            return;
        };
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("top file");
        self.pending_delete = Some(DeleteTarget::TopFile {
            name: name.to_string(),
            path,
        });
        self.confirming_delete = true;
    }

    pub fn close_top_files(&mut self) {
        self.top_files_open = false;
        self.top_files_loading = false;
        self.top_files_scan = None;
        self.top_files_scan_rx = None;
        self.top_files_path = None;
        self.top_files_selected = 0;
        self.top_files_offset = 0;
    }

    pub fn close_reclaim_paths(&mut self) {
        self.reclaim_paths_open = false;
        self.reclaim_paths_selected = 0;
        self.reclaim_path_list_offset = 0;
    }

    pub fn close_disk_info(&mut self) {
        self.disk_info_open = false;
        self.disk_info_loading = false;
        self.disk_info_scan_rx = None;
    }

    pub fn request_empty_trash(&mut self) {
        if self.confirming_delete {
            return;
        }
        match crate::fs_ops::empty_trash() {
            Ok(()) => {
                self.status = String::from("Trash emptied");
                self.refresh_disks();
                if self.focus == Focus::Reclaim {
                    self.request_reclaim_scan();
                }
            }
            Err(e) => {
                self.status = format!("empty trash failed: {e}");
            }
        }
    }

    pub fn open_top_files_for_path(&mut self, path: PathBuf) {
        if self.top_files_loading {
            self.top_files_scan = None;
            self.top_files_scan_rx = None;
            self.top_files_loading = false;
        }
        let scan_id = self.next_top_files_scan_id();
        let (tx, rx) = std::sync::mpsc::channel();
        self.top_files_path = Some(path.clone());
        self.top_files_open = true;
        self.top_files_selected = 0;
        self.top_files_offset = 0;
        self.top_files_loading = true;
        self.status = String::from("scanning top files…");
        self.top_files_scan = None;
        self.top_files_scan_rx = Some(rx);

        thread::spawn(move || {
            let scan = bulkstat::scan_dir(&path, TOP_FILES_LIMIT);
            let _ = tx.send(TopFilesMsg {
                scan_id,
                path,
                scan,
            });
        });
    }

    pub fn request_disk_info_for_selected_disk(&mut self) {
        let Some(disk) = self.disks.get(self.selected_disk).cloned() else {
            self.status = String::from("no disk selected");
            return;
        };
        let path = disk.mount.clone();
        let scan_id = self.next_disk_info_scan_id();
        let (tx, rx) = std::sync::mpsc::channel();
        self.disk_info_id = scan_id;
        self.disk_info_loading = true;
        self.disk_info_report = None;
        self.disk_info_open = true;
        self.status = String::from("loading disk info…");
        self.disk_info_scan_rx = Some(rx);

        thread::spawn(move || {
            let result = space::report_for_path(&path).map_err(|e| e.to_string());
            let _ = tx.send(DiskInfoMsg { scan_id, result });
        });
    }

    pub fn has_pending_scan_work(&self) -> bool {
        self.sort_dirty
            || self.entries.iter().any(|entry| entry.scanning)
            || self.top_files_loading
            || self.reclaim_loading
            || self.disk_info_loading
            || self.packages_loading
            || self.deps_loading
            || self.uninstall_rx.is_some()
    }

    pub fn request_delete(&mut self) {
        if self.confirming_delete {
            return;
        }
        if self.focus == Focus::Files && !self.marked.is_empty() {
            self.request_batch_delete();
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
            Focus::Reclaim => {
                if self.reclaim_paths_open {
                    self.request_delete_reclaim_path();
                } else {
                    self.status = String::from("press Enter on a reclaim finding to list paths");
                }
            }
            Focus::Packages => match self.pkg_view {
                PkgView::SystemManagers => {
                    let real_idx = self
                        .pkg_visible_index(self.selected_pkg)
                        .unwrap_or(usize::MAX);
                    let packages = &self.cached_flat_packages;
                    if let Some((package, manager)) = packages.get(real_idx) {
                        if *manager == packages::Manager::BrewCask {
                            self.request_uninstall();
                        } else if let Some(path) = &package.path {
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
                    let real_idx = self
                        .pkg_visible_index(self.selected_pkg)
                        .unwrap_or(usize::MAX);
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
            let top_files_path = self.top_files_path.clone();
            let _reclaim_finding = self.reclaim_paths_finding;
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
                            self.refresh_history_state();
                        }
                        Err(e) => self.status = format!("delete failed: {e}"),
                    }
                }
                DeleteTarget::TopFile { name, path } => match crate::fs_ops::delete_to_trash(&path)
                {
                    Ok(()) => {
                        self.status = format!("moved to trash: {name}");
                        self.invalidate_cache_for(&path);
                        self.refresh_disks();
                        if let Some(path) = top_files_path {
                            self.open_top_files_for_path(path);
                        }
                        self.refresh_history_state();
                    }
                    Err(e) => self.status = format!("delete failed: {e}"),
                },
                DeleteTarget::ReclaimPath {
                    finding_index,
                    name,
                    path,
                } => match crate::fs_ops::delete_to_trash(&path) {
                    Ok(()) => {
                        self.status = format!("moved to trash: {name}");
                        self.invalidate_cache_for(&path);
                        self.refresh_disks();
                        self.rescan_reclaim_finding(finding_index);
                        self.selected_reclaim =
                            finding_index.min(self.reclaim_item_count().saturating_sub(1));
                        self.refresh_history_state();
                    }
                    Err(e) => {
                        self.status = format!("delete failed: {e}");
                    }
                },
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
                        self.refresh_history_state();
                    }
                    Err(e) => self.status = format!("delete failed: {e}"),
                },
                DeleteTarget::Batch => {
                    let marked = self.marked.clone();
                    self.marked.clear();
                    let mut failures = Vec::new();
                    for path in marked {
                        if let Err(e) = crate::fs_ops::delete_to_trash(&path) {
                            failures.push((path, e));
                        } else {
                            self.invalidate_cache_for(&path);
                        }
                    }
                    self.refresh_disks();
                    self.reload()?;
                    self.refresh_history_state();
                    if failures.is_empty() {
                        self.status = String::from("moved marked items to trash");
                    } else {
                        let names: Vec<String> = failures
                            .iter()
                            .filter_map(|(p, _)| {
                                p.file_name().map(|n| n.to_string_lossy().into_owned())
                            })
                            .collect();
                        self.status = format!("some items failed: {}", names.join(", "));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn pending_delete_name(&self) -> &str {
        match &self.pending_delete {
            Some(DeleteTarget::FileEntry(entry)) => &entry.name,
            Some(DeleteTarget::Package { name, .. }) => name,
            Some(DeleteTarget::TopFile { name, .. }) => name,
            Some(DeleteTarget::ReclaimPath { name, .. }) => name,
            Some(DeleteTarget::Batch) => "marked items",
            None => "?",
        }
    }

    pub fn save_size_cache(&mut self) -> Result<()> {
        if !self.size_cache_dirty {
            return Ok(());
        }
        let entries = self.size_cache_entries_for_store(now_secs());
        state::store_size_cache(&entries)?;
        self.size_cache_dirty = false;
        self.last_size_cache_save = Instant::now();
        Ok(())
    }

    fn size_cache_entries_for_store(&mut self, fallback_scanned_at: u64) -> Vec<state::CachedSize> {
        let mut entries: Vec<state::CachedSize> = self
            .size_cache
            .iter()
            .map(|(path, size)| state::CachedSize {
                path: path.clone(),
                size: *size,
                inaccessible: self.inaccessible_cache.get(path).copied().unwrap_or(0),
                scanned_at: self
                    .cache_age
                    .get(path)
                    .copied()
                    .unwrap_or(fallback_scanned_at),
            })
            .collect();
        entries.sort_by(|a, b| {
            b.scanned_at
                .cmp(&a.scanned_at)
                .then_with(|| a.path.cmp(&b.path))
        });
        if entries.len() > state::SIZE_CACHE_MAX_ENTRIES {
            entries.truncate(state::SIZE_CACHE_MAX_ENTRIES);
            let retained: HashSet<PathBuf> =
                entries.iter().map(|entry| entry.path.clone()).collect();
            self.size_cache.retain(|path, _| retained.contains(path));
            self.inaccessible_cache
                .retain(|path, _| retained.contains(path));
            self.cache_age.retain(|path, _| retained.contains(path));
            self.stale_size_cache.retain(|path| retained.contains(path));
        }
        entries
    }

    fn record_cached_size(
        &mut self,
        path: PathBuf,
        size: SizeInfo,
        inaccessible: u32,
        scanned_at: u64,
        stale: bool,
    ) {
        self.size_cache.insert(path.clone(), size);
        if inaccessible > 0 {
            self.inaccessible_cache.insert(path.clone(), inaccessible);
        } else {
            self.inaccessible_cache.remove(&path);
        }
        self.cache_age.insert(path.clone(), scanned_at);
        if stale {
            self.stale_size_cache.insert(path);
        } else {
            self.stale_size_cache.remove(&path);
        }
        self.size_cache_dirty = true;
    }

    fn remove_cached_size(&mut self, path: &Path) {
        let mut removed = self.size_cache.remove(path).is_some();
        removed |= self.inaccessible_cache.remove(path).is_some();
        removed |= self.cache_age.remove(path).is_some();
        removed |= self.stale_size_cache.remove(path);
        if removed {
            self.size_cache_dirty = true;
        }
    }

    /// When a path changes (deletion, write), its own cache entry and every
    /// ancestor's cached size are now stale.
    fn invalidate_cache_for(&mut self, path: &Path) {
        self.remove_cached_size(path);
        let mut p = path.parent();
        while let Some(parent) = p {
            self.remove_cached_size(parent);
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
        let Some(entry_idx) = self.visible_entry_index(self.selected) else {
            return;
        };
        let Some(entry) = self.entries.get(entry_idx) else {
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
                cwd,
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

            return match recv {
                Ok(msg) => {
                    if msg.scan_id != self.active_pkg_scan_id {
                        continue;
                    }
                    if msg.cwd != self.cwd {
                        self.packages_loading = false;
                        self.pkg_scan_rx = None;
                        self.status = String::from("discarded stale package scan");
                        return true;
                    }
                    if msg.include_managers {
                        self.pkg_reports = msg.reports;
                        self.packages_loaded = true;
                    }
                    self.project_deps = msg.project_deps;
                    self.project_deps_cwd = Some(msg.cwd);
                    self.rebuild_flat_packages();
                    self.selected_pkg = self
                        .selected_pkg
                        .min(self.pkg_item_count().saturating_sub(1));
                    self.packages_loading = false;
                    self.pkg_scan_rx = None;
                    if msg.include_managers {
                        let total = self.cached_flat_packages.len();
                        self.status = format!(
                            "{} packages across {} managers · {} projects · scanning deps…",
                            total,
                            self.pkg_reports.iter().filter(|r| r.available).count(),
                            self.project_deps.len()
                        );
                        self.start_dep_scan();
                    } else {
                        self.status = format!(
                            "project dependencies refreshed · {} projects",
                            self.project_deps.len()
                        );
                    }
                    true
                }
                Err(TryRecvError::Empty) => false,
                Err(TryRecvError::Disconnected) => {
                    self.packages_loading = false;
                    self.pkg_scan_rx = None;
                    self.status = String::from("package scan failed");
                    true
                }
            };
        }
    }

    pub fn load_packages(&mut self) {
        if self.packages_loaded {
            if self.project_deps_cwd.as_deref() != Some(self.cwd.as_path()) {
                self.reload_project_deps();
            }
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

    fn start_dep_scan(&mut self) {
        if self.deps_loading {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let reports = self.pkg_reports.clone();
        self.deps_loading = true;
        self.dep_scan_rx = Some(rx);

        thread::spawn(move || {
            let graph = packages::scan_dep_graph(&reports);
            let _ = tx.send(DepScanMsg { graph });
        });
    }

    fn drain_dep_results(&mut self) -> bool {
        let recv = match self.dep_scan_rx.as_ref() {
            Some(rx) => rx.try_recv(),
            None => return self.drain_uninstall_results(),
        };

        match recv {
            Ok(msg) => {
                let dependency_leaves = msg.graph.dependency_leaf_count();
                self.dep_graph = Some(msg.graph);
                self.deps_loading = false;
                self.dep_scan_rx = None;
                let total = self.cached_flat_packages.len();
                self.status = format!(
                    "{total} packages · {dependency_leaves} dependency leaves · u to filter · i for details"
                );
                true
            }
            Err(TryRecvError::Empty) => self.drain_uninstall_results(),
            Err(TryRecvError::Disconnected) => {
                self.deps_loading = false;
                self.dep_scan_rx = None;
                self.drain_uninstall_results()
            }
        }
    }

    fn drain_uninstall_results(&mut self) -> bool {
        let recv = match self.uninstall_rx.as_ref() {
            Some(rx) => rx.try_recv(),
            None => return false,
        };

        match recv {
            Ok(msg) => {
                self.uninstall_rx = None;
                match msg.result {
                    Ok(_) => {
                        self.status = format!("uninstalled {}", msg.display_name);
                        self.dep_graph = None;
                        self.refresh_packages();
                    }
                    Err(err) => {
                        self.status = format!("uninstall failed: {err}");
                    }
                }
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.uninstall_rx = None;
                false
            }
        }
    }

    pub fn toggle_unused_filter(&mut self) {
        self.pkg_show_unused = !self.pkg_show_unused;
        self.selected_pkg = 0;
        self.pkg_search_matches.clear();
        self.pkg_search_query.clear();
        self.pkg_search_mode = false;
        if self.pkg_show_unused {
            self.status = String::from("showing dependency leaves only");
        } else {
            self.status = String::from("showing all packages");
        }
    }

    pub fn open_pkg_detail(&mut self) {
        if self.pkg_item_count() > 0 {
            self.pkg_detail = true;
        }
    }

    pub fn close_pkg_detail(&mut self) {
        self.pkg_detail = false;
    }

    pub fn request_uninstall(&mut self) {
        if self.confirming_uninstall || self.uninstall_rx.is_some() {
            return;
        }
        let real_idx = match self.pkg_visible_index(self.selected_pkg) {
            Some(i) => i,
            None => return,
        };
        if self.pkg_view != PkgView::SystemManagers {
            self.status = String::from("uninstall only works for system packages");
            return;
        }
        let Some((pkg, manager)) = self.cached_flat_packages.get(real_idx) else {
            return;
        };
        self.pending_uninstall = Some(UninstallTarget {
            manager: *manager,
            name: pkg.name.clone(),
            display_name: format!("{} {}", manager.label(), pkg.name),
        });
        self.confirming_uninstall = true;
    }

    pub fn cancel_uninstall(&mut self) {
        self.confirming_uninstall = false;
        self.pending_uninstall = None;
    }

    pub fn confirm_uninstall(&mut self) {
        self.confirming_uninstall = false;
        let Some(target) = self.pending_uninstall.take() else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.uninstall_rx = Some(rx);
        self.status = format!("uninstalling {}…", target.display_name);

        thread::spawn(move || {
            let result = packages::run_uninstall(target.manager, &target.name);
            let _ = tx.send(UninstallResult {
                display_name: target.display_name,
                result,
            });
        });
    }

    pub fn pending_uninstall_name(&self) -> &str {
        match &self.pending_uninstall {
            Some(t) => &t.display_name,
            None => "?",
        }
    }

    pub fn selected_pkg_detail(
        &self,
    ) -> Option<(
        &packages::Package,
        packages::Manager,
        Option<&packages::DepInfo>,
    )> {
        let real_idx = self.pkg_visible_index(self.selected_pkg)?;
        let (pkg, manager) = self.cached_flat_packages.get(real_idx)?;
        let dep_info = self
            .dep_graph
            .as_ref()
            .and_then(|g| g.get(*manager, &pkg.name));
        Some((pkg, *manager, dep_info))
    }

    pub fn toggle_pkg_view(&mut self) {
        self.pkg_view = match self.pkg_view {
            PkgView::SystemManagers => PkgView::ProjectDeps,
            PkgView::ProjectDeps => PkgView::SystemManagers,
        };
        self.selected_pkg = 0;
    }

    fn pkg_passes_unused_filter(&self, idx: usize) -> bool {
        if !self.pkg_show_unused {
            return true;
        }
        if self.pkg_view != PkgView::SystemManagers {
            return true;
        }
        let Some(graph) = &self.dep_graph else {
            return true;
        };
        let Some((pkg, manager)) = self.cached_flat_packages.get(idx) else {
            return false;
        };
        graph.use_status(*manager, &pkg.name) == packages::PackageUseStatus::DependencyLeaf
    }

    fn base_pkg_indices(&self) -> Vec<usize> {
        let total = match self.pkg_view {
            PkgView::SystemManagers => self.cached_flat_packages.len(),
            PkgView::ProjectDeps => self.project_deps.len(),
        };
        (0..total)
            .filter(|&i| self.pkg_passes_unused_filter(i))
            .collect()
    }

    pub fn pkg_item_count(&self) -> usize {
        if self.pkg_search_mode && !self.pkg_search_query.is_empty() {
            return self.pkg_search_matches.len();
        }
        self.base_pkg_indices().len()
    }

    pub fn pkg_visible_index(&self, visible_index: usize) -> Option<usize> {
        if self.pkg_search_mode && !self.pkg_search_query.is_empty() {
            return self.pkg_search_matches.get(visible_index).copied();
        }
        self.base_pkg_indices().get(visible_index).copied()
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
            let base = self.base_pkg_indices();
            self.selected_pkg = base.iter().position(|&i| i == idx).unwrap_or(0);
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
        let base = self.base_pkg_indices();
        self.pkg_search_matches = match self.pkg_view {
            PkgView::SystemManagers => base
                .into_iter()
                .filter(|&i| {
                    let Some((pkg, mgr)) = self.cached_flat_packages.get(i) else {
                        return false;
                    };
                    pkg.name.to_lowercase().contains(&query) || mgr.label().contains(&query)
                })
                .collect(),
            PkgView::ProjectDeps => base
                .into_iter()
                .filter(|&i| {
                    let Some(dep) = self.project_deps.get(i) else {
                        return false;
                    };
                    dep.path.to_string_lossy().to_lowercase().contains(&query)
                        || dep.manager_label.contains(&query)
                })
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

    fn enter_input_mode(
        &mut self,
        mode: InputMode,
        prompt: &str,
        initial: &str,
        action: InputAction,
    ) {
        self.input_mode = mode;
        self.input_prompt = prompt.to_string();
        self.input_buffer = initial.to_string();
        self.input_on_commit = Some(action);
    }

    pub fn exit_input_mode(&mut self) {
        self.input_mode = InputMode::None;
        self.input_prompt.clear();
        self.input_buffer.clear();
        self.input_on_commit = None;
    }

    pub fn input_push(&mut self, ch: char) {
        if ch == '/' || ch == '\0' {
            return;
        }
        self.input_buffer.push(ch);
    }

    pub fn input_pop(&mut self) {
        self.input_buffer.pop();
    }

    pub fn input_commit(&mut self) -> Result<()> {
        if let Some(action) = self.input_on_commit.take() {
            match action {
                InputAction::Rename(old_path) => {
                    let new_name = self.input_buffer.trim();
                    if new_name.is_empty() {
                        self.status = String::from("rename cancelled: empty name");
                        self.exit_input_mode();
                        return Ok(());
                    }
                    if new_name.contains('/') {
                        self.status = String::from("rename failed: name cannot contain '/'");
                        self.exit_input_mode();
                        return Ok(());
                    }
                    let new_path = old_path.parent().unwrap().join(new_name);
                    if new_path.exists() {
                        self.status = format!("rename failed: {} already exists", new_name);
                        self.exit_input_mode();
                        return Ok(());
                    }
                    match std::fs::rename(&old_path, &new_path) {
                        Ok(()) => {
                            self.status = format!(
                                "renamed: {} → {}",
                                old_path.file_name().unwrap().to_string_lossy(),
                                new_name
                            );
                            self.invalidate_cache_for(&old_path);
                            self.invalidate_cache_for(&new_path);
                            self.reload()?;
                            self.refresh_history_state();
                        }
                        Err(e) => {
                            self.status = format!("rename failed: {e}");
                        }
                    }
                }
                InputAction::Mkdir(parent) => {
                    let new_name = self.input_buffer.trim();
                    if new_name.is_empty() {
                        self.status = String::from("mkdir cancelled: empty name");
                        self.exit_input_mode();
                        return Ok(());
                    }
                    if new_name.contains('/') {
                        self.status = String::from("mkdir failed: name cannot contain '/'");
                        self.exit_input_mode();
                        return Ok(());
                    }
                    let new_path = parent.join(new_name);
                    if new_path.exists() {
                        self.status = format!("mkdir failed: {} already exists", new_name);
                        self.exit_input_mode();
                        return Ok(());
                    }
                    match std::fs::create_dir(&new_path) {
                        Ok(()) => {
                            self.status = format!("created directory: {}", new_name);
                            self.invalidate_cache_for(&new_path);
                            self.reload()?;
                            if let Some(idx) = self.entries.iter().position(|e| e.path == new_path)
                            {
                                self.selected = idx;
                            }
                            self.refresh_history_state();
                        }
                        Err(e) => {
                            self.status = format!("mkdir failed: {e}");
                        }
                    }
                }
            }
        }
        self.exit_input_mode();
        Ok(())
    }

    pub fn request_rename(&mut self) {
        if self.confirming_delete {
            return;
        }
        if let Some(entry_idx) = self.visible_entry_index(self.selected) {
            if let Some(entry) = self.entries.get(entry_idx).cloned() {
                let name = entry.name.clone();
                let path = entry.path.clone();
                self.enter_input_mode(
                    InputMode::Rename,
                    "Rename:",
                    &name,
                    InputAction::Rename(path),
                );
            }
        }
    }

    pub fn request_mkdir(&mut self) {
        if self.confirming_delete {
            return;
        }
        self.enter_input_mode(
            InputMode::Mkdir,
            "New directory:",
            "",
            InputAction::Mkdir(self.cwd.clone()),
        );
    }

    pub fn toggle_mark(&mut self) {
        if self.confirming_delete {
            return;
        }
        if let Some(entry_idx) = self.visible_entry_index(self.selected) {
            if let Some(entry) = self.entries.get(entry_idx) {
                let path = entry.path.clone();
                if self.marked.contains(&path) {
                    self.marked.remove(&path);
                    self.status = format!("unmarked: {}", entry.name);
                } else {
                    self.marked.insert(path);
                    self.status = format!("marked: {}", entry.name);
                }
            }
        }
    }

    pub fn mark_all_visible(&mut self) {
        if self.confirming_delete {
            return;
        }
        let count = self.visible_entry_count();
        for i in 0..count {
            if let Some(entry) = self.visible_entry(i) {
                self.marked.insert(entry.path.clone());
            }
        }
        self.status = format!("marked all {} visible entries", count);
    }

    pub fn request_batch_delete(&mut self) {
        if self.confirming_delete {
            return;
        }
        if self.marked.is_empty() {
            self.status = String::from("nothing marked for deletion");
            return;
        }
        let total_size: u64 = self
            .marked
            .iter()
            .filter_map(|p| self.size_cache.get(p))
            .map(|s| s.allocated)
            .sum();
        let size_str = if total_size > 0 {
            format!(" ({})", human(total_size))
        } else {
            String::new()
        };
        self.status = format!("Trash {} items{}? (y/n)", self.marked.len(), size_str);
        self.confirming_delete = true;
        self.pending_delete = Some(DeleteTarget::Batch);
    }

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

    pub fn open_shell(&mut self) -> Result<()> {
        let Some(path) = self.selected_path() else {
            self.status = String::from("nothing selected");
            return Ok(());
        };
        let shell_path = if path.is_dir() {
            path
        } else {
            path.parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| self.cwd.clone())
        };
        Command::new("open")
            .arg("-a")
            .arg("Terminal")
            .arg(&shell_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("open shell")?;
        self.status = format!("opening terminal: {}", shell_path.display());
        Ok(())
    }

    #[allow(dead_code)]
    fn selected_path(&self) -> Option<PathBuf> {
        match self.focus {
            Focus::Files => self.visible_entry(self.selected).map(|e| e.path.clone()),
            Focus::Disks => self.disks.get(self.selected_disk).map(|d| d.mount.clone()),
            Focus::Reclaim => self.selected_reclaim_path().map(|(_, path)| path),
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

fn modal_window_bounds(
    selected: usize,
    count: usize,
    mut offset: usize,
    max_rows: usize,
) -> (usize, usize) {
    let max_rows = max_rows.max(1);
    if count == 0 {
        return (0, 0);
    }
    if selected < offset {
        offset = selected;
    } else if selected >= offset.saturating_add(max_rows) {
        offset = selected + 1 - max_rows;
    }
    offset = offset.min(count.saturating_sub(1));
    let end = offset.saturating_add(max_rows).min(count);
    (offset, end)
}

fn limited_scan_status(action: &str, scanning: usize, total: usize) -> String {
    if scanning >= total {
        format!("{action}: {total} directories")
    } else {
        format!("{action}: {scanning}/{total} directories · move or r to scan more")
    }
}

fn scan_path_label(path: &Path) -> String {
    const MAX_CHARS: usize = 36;
    let label = path
        .file_name()
        .filter(|name| !name.is_empty())
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    let char_count = label.chars().count();
    if char_count <= MAX_CHARS {
        return label.into_owned();
    }
    let tail: String = label
        .chars()
        .skip(char_count.saturating_sub(MAX_CHARS - 1))
        .collect();
    format!("…{tail}")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
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

pub fn format_elapsed(secs: u64) -> String {
    if secs == 0 {
        return String::from("just now");
    }
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let minutes = (secs % 3_600) / 60;
    if days > 0 {
        return format!("{days}d {hours}h ago");
    }
    if hours > 0 {
        return format!("{hours}h {minutes}m ago");
    }
    if minutes > 0 {
        return format!("{minutes}m ago");
    }
    format!("{secs}s ago")
}

pub fn format_modified_time(modified: Option<SystemTime>) -> String {
    modified
        .and_then(format_localtime)
        .unwrap_or_else(|| String::from("?"))
}

fn format_localtime(modified: SystemTime) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const SECOND: u64 = 1;
    const MINUTE: u64 = 60 * SECOND;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const RELATIVE_THRESHOLD: u64 = 30 * DAY;

    let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let target_secs = modified.duration_since(UNIX_EPOCH).ok()?.as_secs();
    if now_secs >= target_secs {
        let age = now_secs.saturating_sub(target_secs);
        if age < MINUTE {
            return Some(format!("{}s", age));
        }
        if age < HOUR {
            return Some(format!("{}m", age / MINUTE));
        }
        if age < DAY {
            return Some(format!("{}h", age / HOUR));
        }
        if age < RELATIVE_THRESHOLD {
            return Some(format!("{}d", age / DAY));
        }
    }

    let now_tm = to_local_tm(now_secs)?;
    let target_tm = to_local_tm(target_secs)?;

    if now_tm.tm_year == target_tm.tm_year {
        if let Some(month) = MONTHS.get(target_tm.tm_mon as usize) {
            return Some(format!("{} {}", month, target_tm.tm_mday));
        }
    }

    Some(format!(
        "{:04}-{:02}-{:02}",
        target_tm.tm_year + 1900,
        target_tm.tm_mon + 1,
        target_tm.tm_mday
    ))
}

fn to_local_tm(secs: u64) -> Option<libc::tm> {
    let secs = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ptr = unsafe { libc::localtime_r(&secs, &mut tm) };
    if ptr.is_null() {
        return None;
    }
    Some(tm)
}

fn disk_info() -> Vec<DiskInfo> {
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if count <= 0 {
        return Vec::new();
    }

    let mut stats = Vec::<libc::statfs>::with_capacity(count as usize);
    let bytes = (stats.capacity() * size_of::<libc::statfs>()) as libc::c_int;
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
            DeleteTarget::TopFile { path, .. } => path.clone(),
            DeleteTarget::ReclaimPath { path, .. } => path.clone(),
            DeleteTarget::Batch => panic!("Batch not expected in this test"),
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
    fn dependency_leaf_filter_includes_global_leaf_managers() {
        let root = test_root("pkg_unused_filter");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.focus = Focus::Packages;
        app.pkg_view = PkgView::SystemManagers;
        app.cached_flat_packages = vec![
            (
                packages::Package {
                    name: "leaf".into(),
                    version: "1.0".into(),
                    size: None,
                    path: None,
                },
                packages::Manager::Brew,
            ),
            (
                packages::Package {
                    name: "shared-lib".into(),
                    version: "2.0".into(),
                    size: None,
                    path: None,
                },
                packages::Manager::Brew,
            ),
            (
                packages::Package {
                    name: "miniconda".into(),
                    version: "base".into(),
                    size: None,
                    path: None,
                },
                packages::Manager::BrewCask,
            ),
        ];
        app.dep_graph = Some(DepGraph::from_entries(vec![
            (
                packages::Manager::Brew,
                "leaf",
                packages::DepInfo {
                    dependencies: Vec::new(),
                    dependents: Vec::new(),
                    evidence: packages::DepEvidence::ManagerGraph,
                },
            ),
            (
                packages::Manager::Brew,
                "shared-lib",
                packages::DepInfo {
                    dependencies: Vec::new(),
                    dependents: vec!["app".into()],
                    evidence: packages::DepEvidence::ManagerGraph,
                },
            ),
            (
                packages::Manager::BrewCask,
                "miniconda",
                packages::DepInfo {
                    dependencies: Vec::new(),
                    dependents: Vec::new(),
                    evidence: packages::DepEvidence::ManagerGraph,
                },
            ),
        ]));

        app.toggle_unused_filter();

        assert_eq!(app.pkg_item_count(), 2);
        assert_eq!(app.pkg_visible_index(0), Some(0));
        assert_eq!(app.pkg_visible_index(1), Some(2));
        assert_eq!(app.pkg_visible_index(2), None);

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
    fn app_new_preserves_caller_path() {
        let root = test_root("canonical_start");
        fs::create_dir_all(&root).unwrap();

        let start = root.join(".");
        let app = App::new(start.clone()).unwrap();

        assert_eq!(app.cwd, start);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn history_baseline_status_does_not_duplicate_ago() {
        let root = test_root("history_status");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.history_baseline = Some(history::ScanRecord {
            path: root.clone(),
            timestamp: now_secs().saturating_sub(60),
            children: Vec::new(),
        });

        let status = app.history_baseline_status().unwrap();
        assert!(status.starts_with("baseline saved "));
        assert!(!status.contains("ago ago"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_reclaim_scan_result_is_ignored_after_cwd_change() {
        let root_a = test_root("stale_reclaim_a");
        let root_b = test_root("stale_reclaim_b");
        fs::create_dir_all(&root_a).unwrap();
        fs::create_dir_all(&root_b).unwrap();

        let mut app = App::new(root_a.clone()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        app.active_reclaim_scan_id = 7;
        app.reclaim_scan_rx = Some(rx);
        app.reclaim_loading = true;
        app.cwd = root_b.clone();

        tx.send(ReclaimMsg {
            scan_id: 7,
            root: root_a.clone(),
            report: reclaim::ReclaimReport {
                root: root_a.clone(),
                findings: Vec::new(),
                total: SizeInfo::default(),
                inaccessible: 0,
            },
        })
        .unwrap();

        assert!(app.drain_reclaim_results());
        assert!(app.reclaim_report.is_none());
        assert!(!app.reclaim_loading);
        assert!(app.status.contains("stale reclaim"));

        fs::remove_dir_all(root_a).unwrap();
        fs::remove_dir_all(root_b).unwrap();
    }

    #[test]
    fn stale_package_scan_result_is_ignored_after_cwd_change() {
        let root_a = test_root("stale_pkg_a");
        let root_b = test_root("stale_pkg_b");
        fs::create_dir_all(&root_a).unwrap();
        fs::create_dir_all(&root_b).unwrap();

        let mut app = App::new(root_a.clone()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        app.active_pkg_scan_id = 11;
        app.pkg_scan_rx = Some(rx);
        app.packages_loading = true;
        app.cwd = root_b.clone();

        tx.send(PkgScanMsg {
            scan_id: 11,
            cwd: root_a.clone(),
            reports: Vec::new(),
            project_deps: vec![ProjectDeps {
                path: root_a.clone(),
                manager_label: "cargo",
                manifest: "Cargo.toml",
                dep_count: 1,
                deps_size: None,
                deps_dir: Some(root_a.join("target")),
            }],
            include_managers: false,
        })
        .unwrap();

        assert!(app.drain_package_results());
        assert!(app.project_deps.is_empty());
        assert!(app.project_deps_cwd.is_none());
        assert!(!app.packages_loading);
        assert!(app.status.contains("stale package") || app.status.contains("discarded stale"));

        fs::remove_dir_all(root_a).unwrap();
        fs::remove_dir_all(root_b).unwrap();
    }

    #[test]
    fn modal_window_bounds_keeps_selection_visible() {
        assert_eq!(modal_window_bounds(0, 50, 0, 10), (0, 10));
        assert_eq!(modal_window_bounds(12, 50, 0, 10), (3, 13));
        assert_eq!(modal_window_bounds(49, 50, 3, 10), (40, 50));
    }

    #[test]
    fn force_rescan_refreshes_entries_and_preserves_selection() {
        let root = test_root("refresh");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), b"a").unwrap();
        fs::write(root.join("c.txt"), b"ccc").unwrap();
        for i in 0..6 {
            fs::create_dir_all(root.join(format!("dir-{i}"))).unwrap();
        }

        let mut app = App::new(root.clone()).unwrap();
        app.sort = SortMode::Name;
        app.apply_sort();
        app.move_cursor(1);
        let selected_before = app.entries[app.selected].path.clone();

        fs::write(root.join("b.txt"), b"bb").unwrap();
        app.force_rescan();

        assert!(app.entries.iter().any(|entry| entry.name == "b.txt"));
        assert_eq!(app.entries[app.selected].path, selected_before);

        let scanning_dirs_count = app
            .entries
            .iter()
            .filter(|entry| entry.is_dir && entry.scanning)
            .count();
        assert_eq!(scanning_dirs_count, 6);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn full_scan_scans_all_missing_visible_dirs_without_invalidating_cache() {
        let root = test_root("full_scan");
        fs::create_dir_all(&root).unwrap();
        for i in 0..6 {
            fs::create_dir_all(root.join(format!("dir-{i}"))).unwrap();
        }

        let mut app = App::new(root.clone()).unwrap();
        app.sort = SortMode::Name;
        app.apply_sort();
        let known_dir = root.join("dir-0");
        let known_size = SizeInfo::new(123, 64);
        app.size_cache.insert(known_dir.clone(), known_size);
        for entry in &mut app.entries {
            entry.scanning = false;
            if entry.path == known_dir {
                entry.size = Some(known_size);
            } else if entry.is_dir {
                entry.size = None;
            }
        }
        app.selected = app
            .entries
            .iter()
            .position(|entry| entry.path == root.join("dir-4"))
            .unwrap();

        app.scan_all_missing_visible();

        let scanning_dirs_count = app
            .entries
            .iter()
            .filter(|entry| entry.is_dir && entry.scanning)
            .count();
        assert_eq!(scanning_dirs_count, 5);
        assert_eq!(app.size_cache.get(&known_dir), Some(&known_size));
        assert_eq!(
            app.entries
                .iter()
                .find(|entry| entry.path == known_dir)
                .and_then(|entry| entry.size),
            Some(known_size)
        );
        assert!(app.status.contains("full scan: 5 directories"));

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
    fn persisted_size_cache_marks_directory_stale_until_fresh_scan() {
        let root = test_root("stale_cache");
        let cached = root.join("cached");
        fs::create_dir_all(&cached).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.record_cached_size(cached.clone(), SizeInfo::new(10, 20), 1, 123, true);
        app.rebuild_entries().unwrap();

        let entry = app
            .entries
            .iter()
            .find(|entry| entry.path == cached)
            .unwrap();
        assert_eq!(entry.size, Some(SizeInfo::new(10, 20)));
        assert!(entry.size_stale);
        assert_eq!(entry.cached_at, Some(123));
        assert_eq!(entry.inaccessible, 1);

        app.record_cached_size(cached.clone(), SizeInfo::new(30, 40), 0, 456, false);
        app.rebuild_entries().unwrap();

        let entry = app
            .entries
            .iter()
            .find(|entry| entry.path == cached)
            .unwrap();
        assert_eq!(entry.size, Some(SizeInfo::new(30, 40)));
        assert!(!entry.size_stale);
        assert_eq!(entry.cached_at, Some(456));
        assert_eq!(entry.inaccessible, 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_invalidation_removes_persistent_metadata_for_ancestors() {
        let root = test_root("cache_invalidation");
        let cached = root.join("cached");
        fs::create_dir_all(&cached).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.record_cached_size(cached.clone(), SizeInfo::new(10, 20), 2, 123, true);
        app.invalidate_cache_for(&cached.join("child.txt"));

        assert!(!app.size_cache.contains_key(&cached));
        assert!(!app.inaccessible_cache.contains_key(&cached));
        assert!(!app.cache_age.contains_key(&cached));
        assert!(!app.stale_size_cache.contains(&cached));
        assert!(app.size_cache_dirty);

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
    fn scan_selected_missing_dir_uses_visible_mapping_during_search() {
        let root = test_root("search_selected_scan");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(root.join("aaa")).unwrap();
        fs::create_dir_all(root.join("bbb")).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.sort = SortMode::Name;
        app.apply_sort();
        for entry in &mut app.entries {
            entry.size = None;
            entry.scanning = false;
        }

        app.enter_search();
        app.search_push('b');

        app.move_cursor(0);

        let aaa_scanning = app
            .entries
            .iter()
            .find(|entry| entry.name == "aaa")
            .unwrap()
            .scanning;
        let bbb_scanning = app
            .entries
            .iter()
            .find(|entry| entry.name == "bbb")
            .unwrap()
            .scanning;

        assert!(bbb_scanning);
        assert!(!aaa_scanning);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn apply_sort_preserving_selection_rebuilds_search_matches() {
        let root = test_root("search_sort_rebuild_matches");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b-small")).unwrap();
        fs::create_dir_all(root.join("b-big")).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.sort = SortMode::Name;
        app.apply_sort();

        for entry in &mut app.entries {
            let size = match entry.name.as_str() {
                "b-big" => Some(SizeInfo::new(200, 32)),
                "b-small" => Some(SizeInfo::new(100, 16)),
                _ => Some(SizeInfo::new(1, 8)),
            };
            entry.size = size;
        }

        app.enter_search();
        app.search_push('b');
        app.move_cursor(1);

        app.sort = SortMode::SizeDesc;
        app.apply_sort_preserving_selection();

        let visible_matches: Vec<String> = app
            .search_matches
            .iter()
            .map(|&idx| app.entries[idx].name.clone())
            .collect();

        assert_eq!(visible_matches, vec!["b-big", "b-small"]);

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
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("diskr_app_{name}_{}_{}", std::process::id(), nanos))
    }
}
