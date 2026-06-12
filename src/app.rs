use anyhow::{Context, Result};
use std::cmp::{Ordering, Reverse};
use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::os::darwin::fs::MetadataExt as DarwinMetadataExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
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
    pub skipped_mounts: u32,
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

struct HistoryMsg {
    request_id: u64,
    cwd: PathBuf,
    result: HistoryResult,
}

enum HistoryResult {
    Diff(Result<history::DiffReport, String>),
    Save(Result<history::ScanRecord, String>),
}

#[derive(Clone)]
pub struct DiskInfo {
    pub name: String,
    pub mount: PathBuf,
    pub total: u64,
    pub available: u64,
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct FileInfo {
    pub name: String,
    pub path: PathBuf,
    pub kind: &'static str,
    pub direct_items: Option<usize>,
    pub size: Option<SizeInfo>,
    pub size_stale: bool,
    pub inaccessible: u32,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    pub owner: String,
    pub group: String,
    pub permissions_octal: String,
    pub permissions_symbolic: String,
    pub hard_links: u64,
    pub xattr_count: Option<usize>,
    pub has_quarantine_xattr: bool,
}

fn collect_file_info(entry: &Entry) -> Result<FileInfo> {
    let meta = std::fs::symlink_metadata(&entry.path)
        .with_context(|| format!("stat {}", entry.path.display()))?;
    let file_type = meta.file_type();
    let kind = if file_type.is_dir() {
        "directory"
    } else if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_file() {
        "file"
    } else {
        "special"
    };
    let direct_items = if file_type.is_dir() {
        std::fs::read_dir(&entry.path)
            .ok()
            .map(|read| read.flatten().count())
    } else {
        None
    };
    let size = if entry.is_dir {
        entry.size
    } else if file_type.is_file() || file_type.is_symlink() {
        Some(SizeInfo::new(meta.len(), meta.blocks().saturating_mul(512)))
    } else {
        None
    };
    let (xattr_count, has_quarantine_xattr) = list_xattrs(&entry.path);

    Ok(FileInfo {
        name: entry.name.clone(),
        path: entry.path.clone(),
        kind,
        direct_items,
        size,
        size_stale: entry.size_stale,
        inaccessible: entry.inaccessible,
        created: system_time_from_unix(meta.st_birthtime(), meta.st_birthtime_nsec()),
        modified: system_time_from_unix(meta.mtime(), meta.mtime_nsec()),
        accessed: system_time_from_unix(meta.atime(), meta.atime_nsec()),
        owner: lookup_user_name(meta.uid()),
        group: lookup_group_name(meta.gid()),
        permissions_octal: format!("{:04o}", meta.mode() & 0o7777),
        permissions_symbolic: permission_string(meta.mode()),
        hard_links: meta.nlink(),
        xattr_count,
        has_quarantine_xattr,
    })
}

fn system_time_from_unix(secs: i64, nanos: i64) -> Option<SystemTime> {
    if secs < 0 || nanos < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64) + Duration::from_nanos(nanos as u64))
}

fn permission_string(mode: u32) -> String {
    let mut chars = ['-'; 9];
    let flags = [
        0o400, 0o200, 0o100, 0o040, 0o020, 0o010, 0o004, 0o002, 0o001,
    ];
    let symbols = ['r', 'w', 'x', 'r', 'w', 'x', 'r', 'w', 'x'];
    for (idx, flag) in flags.iter().enumerate() {
        if mode & flag != 0 {
            chars[idx] = symbols[idx];
        }
    }
    chars[2] = match (mode & 0o4000, chars[2]) {
        (0, existing) => existing,
        (_, 'x') => 's',
        _ => 'S',
    };
    chars[5] = match (mode & 0o2000, chars[5]) {
        (0, existing) => existing,
        (_, 'x') => 's',
        _ => 'S',
    };
    chars[8] = match (mode & 0o1000, chars[8]) {
        (0, existing) => existing,
        (_, 'x') => 't',
        _ => 'T',
    };
    chars.into_iter().collect()
}

struct XattrSummary {
    count: usize,
    has_quarantine: bool,
}

fn parse_xattrs(bytes: &[u8]) -> XattrSummary {
    let names: Vec<&[u8]> = bytes
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .collect();
    XattrSummary {
        count: names.len(),
        has_quarantine: names.iter().any(|name| *name == b"com.apple.quarantine"),
    }
}

fn list_xattrs(path: &Path) -> (Option<usize>, bool) {
    let path = match std::ffi::CString::new(path.as_os_str().as_bytes()) {
        Ok(path) => path,
        Err(_) => return (None, false),
    };
    let len =
        unsafe { libc::listxattr(path.as_ptr(), std::ptr::null_mut(), 0, libc::XATTR_NOFOLLOW) };
    if len < 0 {
        return (None, false);
    }
    if len == 0 {
        return (Some(0), false);
    }

    let mut buf = vec![0_u8; len as usize];
    let filled = unsafe {
        libc::listxattr(
            path.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::XATTR_NOFOLLOW,
        )
    };
    if filled < 0 {
        return (None, false);
    }

    let summary = parse_xattrs(&buf[..filled as usize]);
    (Some(summary.count), summary.has_quarantine)
}

fn lookup_user_name(uid: u32) -> String {
    let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buf = vec![0_u8; 4096];
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            pwd.as_mut_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut result,
        )
    };
    if status == 0 && !result.is_null() {
        let pwd = unsafe { pwd.assume_init() };
        if !pwd.pw_name.is_null() {
            return unsafe { CStr::from_ptr(pwd.pw_name) }
                .to_string_lossy()
                .into_owned();
        }
    }
    uid.to_string()
}

fn lookup_group_name(gid: u32) -> String {
    let mut grp = std::mem::MaybeUninit::<libc::group>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buf = vec![0_u8; 4096];
    let status = unsafe {
        libc::getgrgid_r(
            gid,
            grp.as_mut_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut result,
        )
    };
    if status == 0 && !result.is_null() {
        let grp = unsafe { grp.assume_init() };
        if !grp.gr_name.is_null() {
            return unsafe { CStr::from_ptr(grp.gr_name) }
                .to_string_lossy()
                .into_owned();
        }
    }
    gid.to_string()
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
    pub show_help: bool,
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
    search_filter_pinned: bool,
    pub search_query: String,
    pub search_matches: Vec<usize>,

    pub pkg_search_mode: bool,
    pkg_filter_pinned: bool,
    pub pkg_search_query: String,
    pub pkg_search_matches: Vec<usize>,
    cached_pkg_visible_indices: Vec<usize>,
    cached_pkg_search_text: Vec<String>,
    cached_project_dep_search_text: Vec<String>,

    pub scan_total: usize,
    pub scan_completed: usize,
    pub scan_skipped_mounts: u32,

    pub files_area: ratatui_core::layout::Rect,
    pub file_list_offset: usize,
    pub disk_page_rows: usize,
    pub package_page_rows: usize,
    pub reclaim_page_rows: usize,

    pending_delete: Option<DeleteTarget>,
    marked: HashSet<PathBuf>,
    size_cache: HashMap<PathBuf, SizeInfo>,
    inaccessible_cache: HashMap<PathBuf, u32>,
    /// True when TCC blocks reads under ~/Library (no Full Disk Access);
    /// scans there will undercount, so the header shows a persistent hint.
    pub fda_limited: bool,
    cache_age: HashMap<PathBuf, u64>,
    stale_size_cache: HashSet<PathBuf>,
    size_cache_dirty: bool,
    last_size_cache_save: Instant,
    entry_index: HashMap<PathBuf, usize>,
    cached_flat_packages: Vec<(packages::Package, packages::Manager)>,
    last_sort: Instant,
    sort_dirty: bool,
    active_scan_id: ScanId,
    min_valid_scan_id: ScanId,
    active_scan_paths: HashSet<PathBuf>,

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
    reclaim_paths_page_rows: usize,

    top_files_open: bool,
    top_files_loading: bool,
    top_files_scan_id: ScanId,
    top_files_scan_rx: Option<Receiver<TopFilesMsg>>,
    top_files_scan: Option<DirScan>,
    top_files_path: Option<PathBuf>,
    pub top_files_selected: usize,
    top_files_offset: usize,
    top_files_page_rows: usize,

    disk_info_open: bool,
    disk_info_loading: bool,
    disk_info_id: ScanId,
    disk_info_scan_rx: Option<Receiver<DiskInfoMsg>>,
    pub disk_info_report: Option<space::SpaceReport>,
    pub file_info: Option<FileInfo>,
    pub file_info_open: bool,

    pub history_baseline: Option<history::ScanRecord>,
    pub history_diff: Option<history::DiffReport>,
    active_history_request_id: u64,
    history_loading: bool,
    history_rx: Option<Receiver<HistoryMsg>>,

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
    pub confirming_empty_trash: bool,
    empty_trash_rx: Option<Receiver<Result<(), String>>>,
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
            status: String::from("i info · Space preview · f Finder · O open"),
            show_help: false,
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
            search_filter_pinned: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            pkg_search_mode: false,
            pkg_filter_pinned: false,
            pkg_search_query: String::new(),
            pkg_search_matches: Vec::new(),
            cached_pkg_visible_indices: Vec::new(),
            cached_pkg_search_text: Vec::new(),
            cached_project_dep_search_text: Vec::new(),
            scan_total: 0,
            scan_completed: 0,
            scan_skipped_mounts: 0,
            files_area: ratatui_core::layout::Rect::default(),
            file_list_offset: 0,
            disk_page_rows: 1,
            package_page_rows: 1,
            reclaim_page_rows: 1,
            pending_delete: None,
            marked: HashSet::new(),
            size_cache,
            inaccessible_cache,
            cache_age,
            stale_size_cache,
            size_cache_dirty,
            last_size_cache_save: Instant::now(),
            fda_limited: false,
            entry_index: HashMap::new(),
            cached_flat_packages: Vec::new(),
            last_sort: Instant::now(),
            sort_dirty: false,
            active_scan_id: 0,
            min_valid_scan_id: 0,
            active_scan_paths: HashSet::new(),
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
            reclaim_paths_page_rows: 1,
            top_files_open: false,
            top_files_loading: false,
            top_files_scan_id: 0,
            top_files_scan_rx: None,
            top_files_scan: None,
            top_files_path: None,
            top_files_selected: 0,
            top_files_offset: 0,
            top_files_page_rows: 1,
            disk_info_open: false,
            disk_info_loading: false,
            disk_info_id: 0,
            disk_info_scan_rx: None,
            disk_info_report: None,
            file_info: None,
            file_info_open: false,
            history_baseline: None,
            history_diff: None,
            active_history_request_id: 0,
            history_loading: false,
            history_rx: None,
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
            confirming_empty_trash: false,
            empty_trash_rx: None,
        };
        app.fda_limited = full_disk_access_missing();
        app.refresh_disks();
        app.reload()?;
        app.refresh_history_state();
        if let Some(warning) = cache_warning {
            app.status = warning;
        }
        Ok(app)
    }

    pub fn refresh_history_state(&mut self) {
        let baseline = history::load_record_for_path(&self.cwd).unwrap_or(None);
        self.apply_history_baseline(baseline);
    }

    fn apply_history_baseline(&mut self, baseline: Option<history::ScanRecord>) {
        self.history_rx = None;
        self.history_loading = false;
        self.history_baseline = baseline;
        self.history_diff = None;
        let Some(baseline) = self.history_baseline.clone() else {
            return;
        };
        self.start_history_diff_request(self.cwd.clone(), baseline);
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
        if self.search_filter_active() {
            self.update_search();
            self.selected = previous_selected
                .and_then(|path| {
                    self.search_matches
                        .iter()
                        .position(|&idx| self.entries[idx].path == path)
                })
                .unwrap_or_else(|| previous_index.min(self.search_matches.len().saturating_sub(1)));
        } else {
            self.restore_selection(previous_selected, previous_index);
        }
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
                skipped_mounts: 0,
                modified,
                scanning: false,
            });
        }
        self.apply_sort();
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
        // Sorting reorders entries, so path→index lookups (used to route
        // late-arriving scan results) must be rebuilt with the new positions.
        self.rebuild_entry_index();
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
            if self.search_filter_active() {
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
        let delta = pages as i64 * self.active_page_rows() as i64;
        match self.focus {
            Focus::Files => {
                let item_count = self.visible_entry_count();
                if let Some(selected) = page_target_index(self.selected, delta, item_count) {
                    self.selected = selected;
                    self.scan_selected_missing_dir();
                }
            }
            Focus::Disks => {
                if let Some(selected) =
                    page_target_index(self.selected_disk, delta, self.disks.len())
                {
                    self.selected_disk = selected;
                }
            }
            Focus::Packages => {
                if let Some(selected) =
                    page_target_index(self.selected_pkg, delta, self.pkg_item_count())
                {
                    self.selected_pkg = selected;
                }
            }
            Focus::Reclaim => {
                if let Some(selected) =
                    page_target_index(self.selected_reclaim, delta, self.reclaim_item_count())
                {
                    self.selected_reclaim = selected;
                }
            }
        }
    }

    fn active_page_rows(&self) -> usize {
        match self.focus {
            Focus::Files => self.files_area.height.saturating_sub(2).max(1) as usize,
            Focus::Disks => self.disk_page_rows.max(1),
            Focus::Packages => self.package_page_rows.max(1),
            Focus::Reclaim => self.reclaim_page_rows.max(1),
        }
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
        if self.search_filter_active() {
            self.search_matches.len()
        } else {
            self.entries.len()
        }
    }

    pub fn visible_entry_index(&self, visible_index: usize) -> Option<usize> {
        if self.search_filter_active() {
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
                            self.marked.clear();
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
                    self.marked.clear();
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
            self.marked.clear();
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
        self.marked.clear();
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
        self.invalidate_pending_scan_results();
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

    /// Invoked by the `S` key. Scans every visible directory with missing or stale size.
    pub fn scan_all_missing_visible(&mut self) {
        if self.confirming_delete {
            return;
        }
        let missing: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|e| entry_needs_scan(e))
            .map(|e| e.path.clone())
            .collect();
        if missing.is_empty() {
            self.status = String::from("full scan complete · all visible sizes fresh");
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
                    skipped_mounts,
                } if scan_id >= self.min_valid_scan_id => {
                    let is_active_scan = scan_id == self.active_scan_id;
                    let keep_scanning = !is_active_scan && self.active_scan_paths.contains(&path);
                    let scanned_at = now_secs();
                    self.record_cached_size(path.clone(), size, inaccessible, scanned_at, false);
                    if let Some(&idx) = self.entry_index.get(&path) {
                        if let Some(e) = self.entries.get_mut(idx) {
                            e.size = Some(size);
                            e.size_stale = false;
                            e.cached_at = Some(scanned_at);
                            e.inaccessible = inaccessible;
                            e.skipped_mounts = skipped_mounts;
                            if !keep_scanning {
                                e.scanning = false;
                            }
                            changed = true;
                        }
                    }
                    if self.sort == SortMode::SizeDesc {
                        self.sort_dirty = true;
                    }
                    if is_active_scan {
                        self.active_scan_paths.remove(&path);
                        self.scan_skipped_mounts =
                            self.scan_skipped_mounts.saturating_add(skipped_mounts);
                        self.scan_completed += 1;
                        if self.scan_completed < self.scan_total {
                            self.status = format!(
                                "scanned {}: {}/{}",
                                scan_path_label(&path),
                                self.scan_completed,
                                self.scan_total
                            );
                        }
                    }
                }
                ScanMsg::AllDone { scan_id } if scan_id == self.active_scan_id => {
                    self.active_scan_paths.clear();
                    for entry in &mut self.entries {
                        entry.scanning = false;
                    }
                    if self.sort_dirty {
                        self.apply_sort_preserving_selection();
                    }
                    self.status = if self.scan_skipped_mounts > 0 {
                        format!(
                            "scan complete · skipped {} mounted volumes under /Volumes",
                            self.scan_skipped_mounts
                        )
                    } else {
                        String::from("scan complete")
                    };
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
        changed |= self.drain_history_results();
        changed |= self.drain_empty_trash_results();
        changed
    }

    pub fn save_history_baseline(&mut self) {
        self.start_history_save_request(self.cwd.clone());
        self.history_diff = None;
        self.status = String::from("saving baseline...");
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

    pub fn open_help(&mut self) {
        self.show_help = true;
    }

    pub fn close_help(&mut self) {
        self.show_help = false;
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

    fn next_history_request_id(&mut self) -> u64 {
        self.active_history_request_id = self.active_history_request_id.saturating_add(1);
        self.active_history_request_id
    }

    fn start_history_diff_request(&mut self, cwd: PathBuf, baseline: history::ScanRecord) {
        let request_id = self.next_history_request_id();
        let (tx, rx) = channel();
        self.history_rx = Some(rx);
        self.history_loading = true;
        thread::spawn(move || {
            let result = history::diff_from_record(&baseline, &cwd).map_err(|err| err.to_string());
            let _ = tx.send(HistoryMsg {
                request_id,
                cwd,
                result: HistoryResult::Diff(result),
            });
        });
    }

    fn start_history_save_request(&mut self, cwd: PathBuf) {
        let request_id = self.next_history_request_id();
        let (tx, rx) = channel();
        self.history_rx = Some(rx);
        self.history_loading = true;
        thread::spawn(move || {
            let result = history::save(&cwd).map_err(|err| err.to_string());
            let _ = tx.send(HistoryMsg {
                request_id,
                cwd,
                result: HistoryResult::Save(result),
            });
        });
    }

    fn drain_history_results(&mut self) -> bool {
        let Some(rx) = self.history_rx.take() else {
            return false;
        };
        match rx.try_recv() {
            Ok(msg) => {
                self.history_loading = false;
                if msg.request_id != self.active_history_request_id || msg.cwd != self.cwd {
                    self.status = String::from("discarded stale history result");
                    return true;
                }
                match msg.result {
                    HistoryResult::Diff(Ok(diff)) => {
                        self.history_diff = Some(diff);
                        true
                    }
                    HistoryResult::Diff(Err(err)) => {
                        self.history_diff = None;
                        self.status = format!("history diff failed: {err}");
                        true
                    }
                    HistoryResult::Save(Ok(record)) => {
                        self.history_baseline = Some(record);
                        self.history_diff = None;
                        self.status = String::from("baseline saved");
                        true
                    }
                    HistoryResult::Save(Err(err)) => {
                        self.status = format!("baseline save failed: {err}");
                        true
                    }
                }
            }
            Err(TryRecvError::Empty) => {
                self.history_rx = Some(rx);
                false
            }
            Err(TryRecvError::Disconnected) => {
                self.history_loading = false;
                self.status = String::from("history worker failed");
                true
            }
        }
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
        self.reclaim_paths_selected = modal_page_selection(
            self.reclaim_paths_selected,
            self.reclaim_paths_count(),
            self.reclaim_paths_page_rows,
            pages,
        );
    }

    pub fn reclaim_paths_window_bounds(&mut self, max_rows: usize) -> (usize, usize) {
        self.reclaim_paths_page_rows = max_rows.max(1);
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
        self.top_files_selected = modal_page_selection(
            self.top_files_selected,
            self.top_files_count(),
            self.top_files_page_rows,
            pages,
        );
    }

    pub fn top_files_selected(&self) -> usize {
        self.top_files_selected
    }

    pub fn top_files_window_bounds(&mut self, max_rows: usize) -> (usize, usize) {
        self.top_files_page_rows = max_rows.max(1);
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

    pub fn open_file_info(&mut self) {
        let Some(entry) = self.visible_entry(self.selected).cloned() else {
            self.status = String::from("no file selected");
            return;
        };
        match collect_file_info(&entry) {
            Ok(info) => {
                self.file_info = Some(info);
                self.file_info_open = true;
            }
            Err(err) => {
                self.status = format!("file info failed: {err}");
            }
        }
    }

    pub fn close_file_info(&mut self) {
        self.file_info_open = false;
    }

    /// Explain-first guardrail: emptying the Trash is permanent, so `E` only
    /// arms a confirmation here; the actual work happens in
    /// [`App::confirm_empty_trash`] on a background thread. The operation is
    /// global (Finder's Empty Trash), so it only arms when the loaded reclaim
    /// report actually lists Trash — otherwise the confirmation would be
    /// detached from what the user sees (#47).
    pub fn request_empty_trash(&mut self) {
        if self.confirming_delete || self.confirming_empty_trash || self.empty_trash_rx.is_some() {
            return;
        }
        let Some(trash) = self.reclaim_trash_finding() else {
            self.status = String::from("Trash is not in this reclaim report");
            return;
        };
        let size_note = format!(" (~{})", human(trash.size.allocated));
        self.status = format!("Empty Trash permanently{size_note}? · y confirm · n cancel");
        self.confirming_empty_trash = true;
    }

    pub fn reclaim_trash_finding(&self) -> Option<&reclaim::Finding> {
        self.reclaim_report
            .as_ref()
            .and_then(|report| report.findings.iter().find(|f| f.label == "Trash"))
    }

    pub fn cancel_empty_trash(&mut self) {
        self.confirming_empty_trash = false;
        self.status = String::from("empty trash cancelled");
    }

    pub fn confirm_empty_trash(&mut self) {
        self.confirming_empty_trash = false;
        let (tx, rx) = std::sync::mpsc::channel();
        self.empty_trash_rx = Some(rx);
        self.status = String::from("emptying Trash…");
        thread::spawn(move || {
            let result = crate::fs_ops::empty_trash().map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
    }

    fn drain_empty_trash_results(&mut self) -> bool {
        let recv = match self.empty_trash_rx.as_ref() {
            Some(rx) => rx.try_recv(),
            None => return false,
        };
        match recv {
            Ok(Ok(())) => {
                self.empty_trash_rx = None;
                self.status = String::from("Trash emptied");
                self.refresh_disks();
                if self.focus == Focus::Reclaim {
                    self.request_reclaim_scan();
                }
                true
            }
            Ok(Err(e)) => {
                self.empty_trash_rx = None;
                self.status = format!("empty trash failed: {e}");
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.empty_trash_rx = None;
                self.status = String::from("empty trash failed");
                true
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
            || self.history_loading
            || self.packages_loading
            || self.deps_loading
            || self.uninstall_rx.is_some()
            || self.empty_trash_rx.is_some()
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
            Focus::Packages => {
                if self.cached_pkg_visible_indices.is_empty()
                    && match self.pkg_view {
                        PkgView::SystemManagers => !self.cached_flat_packages.is_empty(),
                        PkgView::ProjectDeps => !self.project_deps.is_empty(),
                    }
                {
                    self.rebuild_pkg_visible_indices();
                }
                match self.pkg_view {
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
                }
            }
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
        self.invalidate_pending_scan_results();
        self.remove_cached_size(path);
        let mut p = path.parent();
        while let Some(parent) = p {
            self.remove_cached_size(parent);
            p = parent.parent();
        }
    }

    fn invalidate_pending_scan_results(&mut self) {
        let next_valid_scan_id = self.active_scan_id.saturating_add(1);
        self.min_valid_scan_id = self.min_valid_scan_id.max(next_valid_scan_id);
        self.active_scan_id = next_valid_scan_id;
        self.active_scan_paths.clear();
        self.scan_total = 0;
        self.scan_completed = 0;
        self.scan_skipped_mounts = 0;
        for entry in &mut self.entries {
            entry.scanning = false;
        }
        self.scanner.cancel_current();
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
        if !entry_needs_scan(entry) || entry.scanning {
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
        self.scan_skipped_mounts = 0;
        self.active_scan_paths = dirs.iter().cloned().collect();
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
                self.active_scan_paths.clear();
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
                    self.rebuild_project_dep_search_text();
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
                self.rebuild_pkg_visible_indices();
                if self.pkg_filter_active() {
                    self.update_pkg_search();
                }
                self.selected_pkg = self
                    .selected_pkg
                    .min(self.pkg_item_count().saturating_sub(1));
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
        self.rebuild_pkg_visible_indices();
        self.selected_pkg = 0;
        self.pkg_search_matches.clear();
        self.pkg_search_query.clear();
        self.pkg_search_mode = false;
        self.pkg_filter_pinned = false;
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
        // Only valid in the SystemManagers view; in ProjectDeps the visible index
        // addresses project_deps, not cached_flat_packages.
        if self.pkg_view != PkgView::SystemManagers {
            return None;
        }
        let real_idx = self.pkg_visible_index(self.selected_pkg)?;
        let (pkg, manager) = self.cached_flat_packages.get(real_idx)?;
        let dep_info = self
            .dep_graph
            .as_ref()
            .and_then(|g| g.get(*manager, &pkg.name));
        Some((pkg, *manager, dep_info))
    }

    pub fn selected_project_dep_detail(&self) -> Option<&packages::ProjectDeps> {
        if self.pkg_view != PkgView::ProjectDeps {
            return None;
        }
        let real_idx = self.pkg_visible_index(self.selected_pkg)?;
        self.project_deps.get(real_idx)
    }

    pub fn toggle_pkg_view(&mut self) {
        self.pkg_view = match self.pkg_view {
            PkgView::SystemManagers => PkgView::ProjectDeps,
            PkgView::ProjectDeps => PkgView::SystemManagers,
        };
        self.rebuild_pkg_visible_indices();
        if self.pkg_filter_active() {
            self.update_pkg_search();
        }
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

    fn rebuild_pkg_visible_indices(&mut self) {
        let total = match self.pkg_view {
            PkgView::SystemManagers => self.cached_flat_packages.len(),
            PkgView::ProjectDeps => self.project_deps.len(),
        };
        self.cached_pkg_visible_indices = (0..total)
            .filter(|&i| self.pkg_passes_unused_filter(i))
            .collect();
    }

    pub fn pkg_item_count(&self) -> usize {
        if self.pkg_filter_active() {
            return self.pkg_search_matches.len();
        }
        if self.cached_pkg_visible_indices.is_empty() {
            return match self.pkg_view {
                PkgView::SystemManagers => self.cached_flat_packages.len(),
                PkgView::ProjectDeps => self.project_deps.len(),
            };
        }
        self.cached_pkg_visible_indices.len()
    }

    pub fn pkg_visible_index(&self, visible_index: usize) -> Option<usize> {
        if self.pkg_filter_active() {
            return self.pkg_search_matches.get(visible_index).copied();
        }
        if self.cached_pkg_visible_indices.is_empty() {
            let total = match self.pkg_view {
                PkgView::SystemManagers => self.cached_flat_packages.len(),
                PkgView::ProjectDeps => self.project_deps.len(),
            };
            return (visible_index < total).then_some(visible_index);
        }
        self.cached_pkg_visible_indices.get(visible_index).copied()
    }

    pub fn pkg_visible_indices(&self) -> &[usize] {
        if self.pkg_filter_active() {
            &self.pkg_search_matches
        } else {
            &self.cached_pkg_visible_indices
        }
    }

    pub fn pkg_filter_active(&self) -> bool {
        !self.pkg_search_query.is_empty() && (self.pkg_search_mode || self.pkg_filter_pinned)
    }

    pub fn enter_pkg_search(&mut self) {
        self.pkg_search_mode = true;
        self.pkg_filter_pinned = false;
        if self.pkg_search_query.is_empty() {
            self.pkg_search_matches.clear();
        }
    }

    pub fn keep_pkg_search(&mut self) {
        if self.pkg_search_query.is_empty() {
            self.clear_pkg_search();
            return;
        }
        self.pkg_search_mode = false;
        self.pkg_filter_pinned = true;
        self.selected_pkg = self
            .selected_pkg
            .min(self.pkg_item_count().saturating_sub(1));
        self.status = format!("package filter /{} kept · Esc clear", self.pkg_search_query);
    }

    pub fn clear_pkg_search(&mut self) {
        let real_index = self.pkg_visible_index(self.selected_pkg);
        self.pkg_search_mode = false;
        self.pkg_filter_pinned = false;
        self.pkg_search_query.clear();
        self.pkg_search_matches.clear();
        if let Some(idx) = real_index {
            self.selected_pkg = self
                .cached_pkg_visible_indices
                .iter()
                .position(|&i| i == idx)
                .unwrap_or(0);
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
                .cached_pkg_visible_indices
                .iter()
                .copied()
                .filter(|&i| {
                    self.cached_pkg_search_text
                        .get(i)
                        .is_some_and(|text| text.contains(&query))
                })
                .collect(),
            PkgView::ProjectDeps => self
                .cached_pkg_visible_indices
                .iter()
                .copied()
                .filter(|&i| {
                    self.cached_project_dep_search_text
                        .get(i)
                        .is_some_and(|text| text.contains(&query))
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
        self.cached_pkg_search_text = self
            .cached_flat_packages
            .iter()
            .map(|(pkg, manager)| format!("{} {}", pkg.name, manager.label()).to_lowercase())
            .collect();
        self.rebuild_pkg_visible_indices();
        if self.pkg_filter_active() {
            self.update_pkg_search();
        }
    }

    fn rebuild_project_dep_search_text(&mut self) {
        self.cached_project_dep_search_text = self
            .project_deps
            .iter()
            .map(|dep| {
                format!(
                    "{} {} {}",
                    dep.manager_label,
                    dep.manifest,
                    dep.path.display().to_string().to_lowercase()
                )
                .to_lowercase()
            })
            .collect();
    }

    pub fn refresh_disks(&mut self) {
        self.disks = disk_info();
        self.selected_disk = self.selected_disk.min(self.disks.len().saturating_sub(1));
    }

    pub fn enter_search(&mut self) {
        self.search_mode = true;
        self.search_filter_pinned = false;
        if self.search_query.is_empty() {
            self.search_matches.clear();
            self.file_list_offset = 0;
        }
    }

    pub fn search_filter_active(&self) -> bool {
        !self.search_query.is_empty() && (self.search_mode || self.search_filter_pinned)
    }

    pub fn keep_search(&mut self) {
        if self.search_query.is_empty() {
            self.clear_search();
            return;
        }
        self.search_mode = false;
        self.search_filter_pinned = true;
        self.selected = self
            .selected
            .min(self.visible_entry_count().saturating_sub(1));
        self.status = format!("filter /{} kept · Esc clear", self.search_query);
    }

    pub fn clear_search(&mut self) {
        let selected_path = self
            .visible_entry_index(self.selected)
            .map(|entry_idx| self.entries.get(entry_idx).map(|entry| entry.path.clone()));
        self.search_mode = false;
        self.search_filter_pinned = false;
        self.search_query.clear();
        self.search_matches.clear();
        self.file_list_offset = 0;
        if let Some(Some(path)) = selected_path {
            if let Some(index) = self.entries.iter().position(|entry| entry.path == path) {
                self.selected = index;
            }
        }
    }

    pub fn exit_search(&mut self) {
        self.clear_search();
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
                            let previous_index = self.selected;
                            self.status = format!(
                                "renamed: {} → {}",
                                old_path.file_name().unwrap().to_string_lossy(),
                                new_name
                            );
                            self.invalidate_cache_for(&old_path);
                            self.invalidate_cache_for(&new_path);
                            self.reload_with_selection(Some(new_path), previous_index)?;
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

    pub fn is_marked(&self, path: &Path) -> bool {
        self.marked.contains(path)
    }

    /// Count plus first few names for the batch-delete confirm modal, so stale
    /// or unexpected marks are visible before the user confirms.
    pub fn pending_batch_summary(&self) -> Option<String> {
        if !matches!(self.pending_delete, Some(DeleteTarget::Batch)) {
            return None;
        }
        let mut names: Vec<&str> = self
            .marked
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
            .collect();
        names.sort_unstable();
        let preview = names.iter().take(3).copied().collect::<Vec<_>>().join(", ");
        let suffix = if names.len() > 3 { ", …" } else { "" };
        Some(format!("{} marked: {preview}{suffix}", names.len()))
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
    let selected = selected.min(count.saturating_sub(1));
    let max_offset = count.saturating_sub(max_rows);
    offset = offset.min(max_offset);
    if selected < offset {
        offset = selected;
    } else if selected >= offset.saturating_add(max_rows) {
        offset = selected + 1 - max_rows;
    }
    let end = offset.saturating_add(max_rows).min(count);
    (offset, end)
}

fn modal_page_selection(selected: usize, count: usize, max_rows: usize, pages: i32) -> usize {
    if count == 0 {
        return 0;
    }
    let step = max_rows
        .max(1)
        .saturating_mul(pages.unsigned_abs() as usize);
    if pages >= 0 {
        selected.saturating_add(step).min(count.saturating_sub(1))
    } else {
        selected.saturating_sub(step)
    }
}

/// True when a TCC-protected directory exists but cannot be listed — the
/// telltale of a terminal running without Full Disk Access. ENOENT (dir
/// absent) is not evidence either way, so only PermissionDenied counts.
fn full_disk_access_missing() -> bool {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return false;
    };
    ["Library/Mail", "Library/Safari", "Library/Messages"]
        .iter()
        .any(|rel| {
            let probe = home.join(rel);
            probe.symlink_metadata().is_ok()
                && matches!(
                    std::fs::read_dir(&probe),
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied
                )
        })
}

fn entry_needs_scan(entry: &Entry) -> bool {
    entry.is_dir && (entry.size.is_none() || entry.size_stale)
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

pub fn format_full_timestamp(timestamp: Option<SystemTime>) -> String {
    timestamp
        .and_then(|time| {
            let secs = time.duration_since(UNIX_EPOCH).ok()?.as_secs();
            let tm = to_local_tm(secs)?;
            Some(format_calendar_timestamp(&tm))
        })
        .unwrap_or_else(|| String::from("?"))
}

fn format_calendar_timestamp(tm: &libc::tm) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
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

fn page_target_index(current: usize, delta: i64, item_count: usize) -> Option<usize> {
    if item_count == 0 {
        return None;
    }
    let max_index = item_count.saturating_sub(1) as i64;
    Some((current as i64 + delta).clamp(0, max_index) as usize)
}

/// Reclaim report whose sole finding is Trash; shared by the empty-trash
/// tests here and in main.rs (#47).
#[cfg(test)]
pub fn trash_only_report(root: &Path, allocated: u64) -> reclaim::ReclaimReport {
    reclaim::ReclaimReport {
        root: root.to_path_buf(),
        findings: vec![reclaim::Finding {
            label: String::from("Trash"),
            class: reclaim::Reclaimability::Safe,
            note: String::from("Already discarded items; emptying is permanent."),
            size: SizeInfo::new(allocated, allocated),
            inaccessible: 0,
            skipped_mounts: 0,
            count: 1,
            paths: vec![root.join(".Trash")],
            rollup: false,
        }],
        total: SizeInfo::new(allocated, allocated),
        inaccessible: 0,
        skipped_mounts: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn empty_trash_does_not_arm_without_trash_finding() {
        let root = test_root("empty_trash_no_trash_finding");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.focus = Focus::Reclaim;

        // No reclaim report loaded at all.
        app.request_empty_trash();
        assert!(!app.confirming_empty_trash);
        assert_eq!(app.status, "Trash is not in this reclaim report");

        // Report loaded, but it has no Trash finding.
        app.reclaim_report = Some(reclaim::ReclaimReport {
            root: root.clone(),
            findings: Vec::new(),
            total: SizeInfo::default(),
            inaccessible: 0,
            skipped_mounts: 0,
        });
        app.status.clear();
        app.request_empty_trash();
        assert!(!app.confirming_empty_trash);
        assert_eq!(app.status, "Trash is not in this reclaim report");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_trash_arms_with_size_from_trash_finding() {
        let root = test_root("empty_trash_with_trash_finding");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.focus = Focus::Reclaim;
        app.reclaim_report = Some(trash_only_report(&root, 4096));

        app.request_empty_trash();

        assert!(app.confirming_empty_trash);
        assert!(app.status.contains(&human(4096)));
        let finding = app.reclaim_trash_finding().expect("trash finding");
        assert_eq!(finding.size.allocated, 4096);
        assert_eq!(finding.paths, vec![root.join(".Trash")]);

        fs::remove_dir_all(root).unwrap();
    }

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
    fn marks_clear_when_changing_directory_or_visibility() {
        let root = test_root("marks_nav");
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(root.join("a.txt"), b"a").unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.sort = SortMode::Name;
        app.apply_sort();

        let file_path = root.join("a.txt");
        app.selected = app
            .entries
            .iter()
            .position(|e| e.path == file_path)
            .unwrap();
        app.toggle_mark();
        assert!(app.is_marked(&file_path));

        app.selected = app.entries.iter().position(|e| e.path == sub).unwrap();
        app.enter().unwrap();
        assert!(!app.is_marked(&file_path), "enter() must clear marks");

        app.go_up().unwrap();
        app.selected = app
            .entries
            .iter()
            .position(|e| e.path == file_path)
            .unwrap();
        app.toggle_mark();
        assert!(app.is_marked(&file_path));
        app.toggle_hidden().unwrap();
        assert!(!app.is_marked(&file_path), "toggle_hidden must clear marks");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn request_delete_batches_marked_items_with_summary() {
        let root = test_root("batch_request");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), b"a").unwrap();
        fs::write(root.join("b.txt"), b"bb").unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.sort = SortMode::Name;
        app.apply_sort();
        app.selected = 0;
        app.toggle_mark();
        app.move_cursor(1);
        app.toggle_mark();

        app.request_delete();

        assert!(app.confirming_delete);
        assert!(matches!(app.pending_delete, Some(DeleteTarget::Batch)));
        let summary = app.pending_batch_summary().unwrap();
        assert!(
            summary.starts_with("2 marked: a.txt, b.txt"),
            "unexpected summary: {summary}"
        );

        app.cancel_delete();
        assert!(app.pending_batch_summary().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn page_move_uses_active_pane_rows_and_clamps() {
        let root = test_root("page_move_per_pane");
        fs::create_dir_all(&root).unwrap();
        for idx in 0..10 {
            fs::write(root.join(format!("file-{idx}.txt")), b"x").unwrap();
        }

        let mut app = App::new(root.clone()).unwrap();

        app.focus = Focus::Files;
        app.files_area = ratatui_core::layout::Rect::new(0, 0, 80, 6);
        app.selected = 0;
        app.page_move(1);
        assert_eq!(app.selected, 4);
        app.page_move(10);
        assert_eq!(app.selected, app.visible_entry_count().saturating_sub(1));
        app.page_move(-10);
        assert_eq!(app.selected, 0);

        app.focus = Focus::Disks;
        app.disks = (0..10)
            .map(|idx| DiskInfo {
                name: format!("disk-{idx}"),
                mount: root.join(format!("disk-{idx}")),
                total: 100,
                available: 40,
            })
            .collect();
        app.disk_page_rows = 3;
        app.selected_disk = 1;
        app.page_move(1);
        assert_eq!(app.selected_disk, 4);
        app.page_move(10);
        assert_eq!(app.selected_disk, app.disks.len().saturating_sub(1));
        app.page_move(-10);
        assert_eq!(app.selected_disk, 0);

        app.focus = Focus::Packages;
        app.packages_loaded = true;
        app.cached_flat_packages = vec![
            make_flat_package("alpha"),
            make_flat_package("beta"),
            make_flat_package("gamma"),
            make_flat_package("delta"),
            make_flat_package("epsilon"),
        ];
        app.package_page_rows = 2;
        app.selected_pkg = 1;
        app.page_move(1);
        assert_eq!(app.selected_pkg, 3);
        app.page_move(10);
        assert_eq!(app.selected_pkg, app.pkg_item_count().saturating_sub(1));
        app.page_move(-10);
        assert_eq!(app.selected_pkg, 0);

        app.focus = Focus::Reclaim;
        app.reclaim_report = Some(reclaim::ReclaimReport {
            root: root.clone(),
            findings: (0..6)
                .map(|idx| reclaim::Finding {
                    label: format!("finding-{idx}"),
                    class: reclaim::Reclaimability::Safe,
                    note: String::from("test"),
                    size: SizeInfo::new(1, 1),
                    inaccessible: 0,
                    skipped_mounts: 0,
                    count: 1,
                    paths: vec![root.join(format!("reclaim-{idx}"))],
                    rollup: false,
                })
                .collect(),
            total: SizeInfo::new(6, 6),
            inaccessible: 0,
            skipped_mounts: 0,
        });
        app.reclaim_page_rows = 2;
        app.selected_reclaim = 2;
        app.page_move(1);
        assert_eq!(app.selected_reclaim, 4);
        app.page_move(10);
        assert_eq!(
            app.selected_reclaim,
            app.reclaim_item_count().saturating_sub(1)
        );
        app.page_move(-10);
        assert_eq!(app.selected_reclaim, 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_trash_requires_confirmation_before_running() {
        let root = test_root("empty_trash_confirm");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.reclaim_report = Some(trash_only_report(&root, 1024));
        app.request_empty_trash();

        // Arming the confirm must not start any background work.
        assert!(app.confirming_empty_trash);
        assert!(app.empty_trash_rx.is_none());
        assert!(app.status.starts_with("Empty Trash permanently"));

        app.cancel_empty_trash();
        assert!(!app.confirming_empty_trash);
        assert!(app.empty_trash_rx.is_none());

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
            manager_label: String::from("cargo"),
            manifest: String::from("Cargo.toml"),
            dep_count: 5,
            deps_size: None,
            deps_dir: Some(root.join("target")),
        }];
        app.focus = Focus::Packages;
        app.pkg_view = PkgView::ProjectDeps;
        app.rebuild_project_dep_search_text();
        app.rebuild_pkg_visible_indices();
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
                    metadata_path: None,
                },
                packages::Manager::Brew,
            ),
            (
                packages::Package {
                    name: "shared-lib".into(),
                    version: "2.0".into(),
                    size: None,
                    path: None,
                    metadata_path: None,
                },
                packages::Manager::Brew,
            ),
            (
                packages::Package {
                    name: "miniconda".into(),
                    version: "base".into(),
                    size: None,
                    path: None,
                    metadata_path: None,
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
    fn package_visibility_cache_tracks_filter_view_and_search_transitions() {
        let root = test_root("pkg_visibility_cache");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.focus = Focus::Packages;
        app.pkg_reports = vec![packages::ManagerReport {
            manager: packages::Manager::Brew,
            packages: vec![
                packages::Package {
                    name: "alpha".into(),
                    version: "1.0".into(),
                    size: Some(SizeInfo::new(20, 20)),
                    path: None,
                    metadata_path: None,
                },
                packages::Package {
                    name: "beta".into(),
                    version: "1.0".into(),
                    size: Some(SizeInfo::new(10, 10)),
                    path: None,
                    metadata_path: None,
                },
            ],
            total_size: SizeInfo::new(30, 30),
            available: true,
        }];
        app.dep_graph = Some(DepGraph::from_entries(vec![
            (
                packages::Manager::Brew,
                "alpha",
                packages::DepInfo {
                    dependencies: Vec::new(),
                    dependents: Vec::new(),
                    evidence: packages::DepEvidence::ManagerGraph,
                },
            ),
            (
                packages::Manager::Brew,
                "beta",
                packages::DepInfo {
                    dependencies: Vec::new(),
                    dependents: vec!["tool".into()],
                    evidence: packages::DepEvidence::ManagerGraph,
                },
            ),
        ]));
        app.rebuild_flat_packages();

        assert_eq!(app.pkg_visible_indices(), &[0, 1]);

        app.toggle_unused_filter();
        let visible_names: Vec<&str> = app
            .pkg_visible_indices()
            .iter()
            .filter_map(|&idx| {
                app.flat_packages()
                    .get(idx)
                    .map(|(pkg, _)| pkg.name.as_str())
            })
            .collect();
        assert_eq!(visible_names, vec!["alpha"]);

        app.enter_pkg_search();
        app.pkg_search_push('a');
        let searched_names: Vec<&str> = app
            .pkg_visible_indices()
            .iter()
            .filter_map(|&idx| {
                app.flat_packages()
                    .get(idx)
                    .map(|(pkg, _)| pkg.name.as_str())
            })
            .collect();
        assert_eq!(searched_names, vec!["alpha"]);

        app.clear_pkg_search();
        app.project_deps = vec![
            ProjectDeps {
                path: root.join("Cargo.toml"),
                manager_label: String::from("cargo"),
                manifest: String::from("Cargo.toml"),
                dep_count: 4,
                deps_size: Some(SizeInfo::new(100, 100)),
                deps_dir: Some(root.join("target")),
            },
            ProjectDeps {
                path: root.join("package.json"),
                manager_label: String::from("npm/bun/yarn"),
                manifest: String::from("package.json"),
                dep_count: 8,
                deps_size: Some(SizeInfo::new(200, 200)),
                deps_dir: Some(root.join("node_modules")),
            },
        ];
        app.rebuild_project_dep_search_text();
        app.toggle_pkg_view();

        assert_eq!(app.pkg_visible_indices(), &[0, 1]);

        app.enter_pkg_search();
        for ch in "cargo".chars() {
            app.pkg_search_push(ch);
        }
        assert_eq!(app.pkg_visible_indices(), &[0]);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn package_search_enter_keeps_filter_until_clear() {
        let root = test_root("pkg_search_keep");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.focus = Focus::Packages;
        app.pkg_reports = vec![packages::ManagerReport {
            manager: packages::Manager::Brew,
            packages: vec![
                packages::Package {
                    name: "alpha".into(),
                    version: "1.0".into(),
                    size: Some(SizeInfo::new(20, 20)),
                    path: None,
                    metadata_path: None,
                },
                packages::Package {
                    name: "beta".into(),
                    version: "1.0".into(),
                    size: Some(SizeInfo::new(10, 10)),
                    path: None,
                    metadata_path: None,
                },
            ],
            total_size: SizeInfo::new(30, 30),
            available: true,
        }];
        app.rebuild_flat_packages();

        app.enter_pkg_search();
        for ch in "alp".chars() {
            app.pkg_search_push(ch);
        }
        assert_eq!(app.pkg_visible_indices(), &[0]);

        app.keep_pkg_search();

        assert!(!app.pkg_search_mode);
        assert!(app.pkg_filter_active());
        assert_eq!(app.pkg_item_count(), 1);
        assert_eq!(app.pkg_visible_indices(), &[0]);

        app.clear_pkg_search();

        assert!(!app.pkg_filter_active());
        assert_eq!(app.pkg_item_count(), 2);

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
    fn open_file_info_collects_selected_entry_metadata() {
        let root = test_root("file_info_modal");
        let subdir = root.join("subdir");
        fs::create_dir_all(&subdir).unwrap();
        fs::write(subdir.join("child.txt"), b"child").unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.sort = SortMode::Name;
        app.apply_sort();
        let known_size = SizeInfo::new(42, 64);
        app.size_cache.insert(subdir.clone(), known_size);
        for entry in &mut app.entries {
            if entry.path == subdir {
                entry.size = Some(known_size);
            }
        }
        app.selected = app
            .entries
            .iter()
            .position(|entry| entry.path == subdir)
            .unwrap();

        app.open_file_info();

        let info = app.file_info.as_ref().expect("file info");
        assert!(app.file_info_open);
        assert_eq!(info.kind, "directory");
        assert_eq!(info.path, subdir);
        assert_eq!(info.direct_items, Some(1));
        assert_eq!(info.size, Some(known_size));
        assert_eq!(info.permissions_symbolic.len(), 9);

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
    fn permission_string_formats_special_bits() {
        assert_eq!(permission_string(0o755), "rwxr-xr-x");
        assert_eq!(permission_string(0o4755), "rwsr-xr-x");
        assert_eq!(permission_string(0o1777), "rwxrwxrwt");
    }

    #[test]
    fn parse_xattrs_detects_quarantine() {
        let summary = parse_xattrs(b"com.apple.lastuseddate#PS\0com.apple.quarantine\0user.test\0");
        assert_eq!(summary.count, 3);
        assert!(summary.has_quarantine);
    }

    #[test]
    fn format_calendar_timestamp_uses_fixed_layout() {
        let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
        tm.tm_year = 126;
        tm.tm_mon = 5;
        tm.tm_mday = 12;
        tm.tm_hour = 7;
        tm.tm_min = 8;
        tm.tm_sec = 9;
        assert_eq!(format_calendar_timestamp(&tm), "2026-06-12 07:08:09");
    }

    #[test]
    fn apply_history_baseline_schedules_background_diff() {
        let root = test_root("history_async_refresh");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("file.txt"), b"hello").unwrap();

        let mut app = App::new(root.clone()).unwrap();
        let baseline = history::ScanRecord {
            path: root.clone(),
            timestamp: now_secs().saturating_sub(30),
            children: Vec::new(),
        };

        app.apply_history_baseline(Some(baseline.clone()));

        assert_eq!(app.history_baseline.as_ref(), Some(&baseline));
        assert!(app.history_loading);
        assert!(app.history_rx.is_some());
        assert!(app.history_diff.is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_history_result_is_ignored_after_cwd_change() {
        let root_a = test_root("stale_history_a");
        let root_b = test_root("stale_history_b");
        fs::create_dir_all(&root_a).unwrap();
        fs::create_dir_all(&root_b).unwrap();

        let mut app = App::new(root_a.clone()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        app.active_history_request_id = 5;
        app.history_rx = Some(rx);
        app.history_loading = true;
        app.history_baseline = Some(history::ScanRecord {
            path: root_a.clone(),
            timestamp: 100,
            children: Vec::new(),
        });
        app.cwd = root_b.clone();

        tx.send(HistoryMsg {
            request_id: 5,
            cwd: root_a.clone(),
            result: HistoryResult::Diff(Ok(history::DiffReport {
                path: root_a.clone(),
                baseline_timestamp: 100,
                current_timestamp: 200,
                before_total: SizeInfo::default(),
                after_total: SizeInfo::new(1, 1),
                changes: Vec::new(),
            })),
        })
        .unwrap();

        assert!(app.drain_history_results());
        assert!(app.history_diff.is_none());
        assert!(!app.history_loading);
        assert!(app.status.contains("stale history"));

        fs::remove_dir_all(root_a).unwrap();
        fs::remove_dir_all(root_b).unwrap();
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
                skipped_mounts: 0,
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
                manager_label: String::from("cargo"),
                manifest: String::from("Cargo.toml"),
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
        assert_eq!(modal_window_bounds(49, 50, 49, 10), (40, 50));
    }

    #[test]
    fn modal_page_selection_uses_visible_page_height() {
        assert_eq!(modal_page_selection(0, 50, 7, 1), 7);
        assert_eq!(modal_page_selection(7, 50, 7, 1), 14);
        assert_eq!(modal_page_selection(14, 50, 7, -1), 7);
        assert_eq!(modal_page_selection(48, 50, 7, 1), 49);
        assert_eq!(modal_page_selection(1, 50, 7, -1), 0);
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
    fn full_scan_includes_stale_visible_dirs_without_invalidating_cache() {
        let root = test_root("full_scan_stale");
        let fresh_dir = root.join("fresh");
        let missing_dir = root.join("missing");
        let stale_dir = root.join("stale");
        fs::create_dir_all(&fresh_dir).unwrap();
        fs::create_dir_all(&missing_dir).unwrap();
        fs::create_dir_all(&stale_dir).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.sort = SortMode::Name;
        app.record_cached_size(fresh_dir.clone(), SizeInfo::new(100, 128), 0, 100, false);
        app.record_cached_size(stale_dir.clone(), SizeInfo::new(200, 256), 0, 100, true);
        app.rebuild_entries().unwrap();
        for entry in &mut app.entries {
            entry.scanning = false;
        }
        app.selected = app
            .entries
            .iter()
            .position(|entry| entry.path == stale_dir)
            .unwrap();

        app.scan_all_missing_visible();

        let fresh = app
            .entries
            .iter()
            .find(|entry| entry.path == fresh_dir)
            .unwrap();
        assert!(!fresh.scanning);
        assert_eq!(fresh.size, Some(SizeInfo::new(100, 128)));

        let missing = app
            .entries
            .iter()
            .find(|entry| entry.path == missing_dir)
            .unwrap();
        assert!(missing.scanning);
        assert_eq!(missing.size, None);

        let stale = app
            .entries
            .iter()
            .find(|entry| entry.path == stale_dir)
            .unwrap();
        assert!(stale.scanning);
        assert_eq!(stale.size, Some(SizeInfo::new(200, 256)));
        assert!(stale.size_stale);
        assert_eq!(
            app.size_cache.get(&stale_dir),
            Some(&SizeInfo::new(200, 256))
        );
        assert!(app.status.contains("full scan: 2 directories"));

        app.invalidate_pending_scan_results();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selected_scan_refreshes_stale_cached_directory() {
        let root = test_root("selected_stale_scan");
        let fresh_dir = root.join("fresh");
        let stale_dir = root.join("stale");
        fs::create_dir_all(&fresh_dir).unwrap();
        fs::create_dir_all(&stale_dir).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.sort = SortMode::Name;
        app.record_cached_size(fresh_dir.clone(), SizeInfo::new(100, 128), 0, 100, false);
        app.record_cached_size(stale_dir.clone(), SizeInfo::new(200, 256), 0, 100, true);
        app.rebuild_entries().unwrap();
        for entry in &mut app.entries {
            entry.scanning = false;
        }
        app.selected = app
            .entries
            .iter()
            .position(|entry| entry.path == stale_dir)
            .unwrap();

        app.scan_selected_missing_dir();

        let fresh = app
            .entries
            .iter()
            .find(|entry| entry.path == fresh_dir)
            .unwrap();
        assert!(!fresh.scanning);

        let stale = app
            .entries
            .iter()
            .find(|entry| entry.path == stale_dir)
            .unwrap();
        assert!(stale.scanning);
        assert_eq!(stale.size, Some(SizeInfo::new(200, 256)));
        assert!(stale.size_stale);
        assert_eq!(app.status, "scanning selected directory: stale");

        app.invalidate_pending_scan_results();
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
    fn rename_reload_selects_new_path_after_sorting() {
        let root = test_root("rename_reselect");
        let alpha = root.join("alpha");
        let zeta = root.join("zeta");
        fs::create_dir_all(&alpha).unwrap();
        fs::create_dir_all(&zeta).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.sort = SortMode::Name;
        app.apply_sort();
        app.selected = app
            .entries
            .iter()
            .position(|entry| entry.path == zeta)
            .unwrap();

        app.request_rename();
        app.input_buffer = String::from("aardvark");
        app.input_commit().unwrap();

        assert_eq!(app.entries[app.selected].path, root.join("aardvark"));
        assert_eq!(app.entries[app.selected].name, "aardvark");

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
    fn search_enter_keeps_filter_until_clear() {
        let root = test_root("search_keep");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("alpha.txt"), b"x").unwrap();
        fs::write(root.join("beta.txt"), b"x").unwrap();
        fs::write(root.join("bravo.txt"), b"x").unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.sort = SortMode::Name;
        app.apply_sort();
        app.enter_search();
        app.search_push('b');

        assert_eq!(app.visible_entry_count(), 2);

        app.keep_search();

        assert!(!app.search_mode);
        assert!(app.search_filter_active());
        assert_eq!(app.visible_entry_count(), 2);
        assert_eq!(app.visible_entry(0).unwrap().name, "beta.txt");

        app.move_cursor(1);
        assert_eq!(app.visible_entry(app.selected).unwrap().name, "bravo.txt");

        app.clear_search();

        assert!(!app.search_filter_active());
        assert_eq!(app.visible_entry_count(), 3);
        assert_eq!(app.entries[app.selected].name, "bravo.txt");

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

    #[test]
    fn dir_size_arriving_after_mid_scan_resort_lands_on_its_own_entry() {
        let root = test_root("stale_entry_index");
        fs::create_dir_all(&root).unwrap();
        let dir_a = root.join("dir_a");
        let dir_b = root.join("dir_b");
        let todo = root.join("todo.py");

        let mut app = App::new(root.clone()).unwrap();
        app.entries = vec![
            Entry {
                name: String::from("dir_a"),
                name_lower: String::from("dir_a"),
                path: dir_a.clone(),
                is_dir: true,
                is_symlink: false,
                size: Some(SizeInfo::new(20_000, 20_480)),
                size_stale: false,
                cached_at: None,
                inaccessible: 0,
                skipped_mounts: 0,
                modified: None,
                scanning: false,
            },
            Entry {
                name: String::from("dir_b"),
                name_lower: String::from("dir_b"),
                path: dir_b.clone(),
                is_dir: true,
                is_symlink: false,
                size: Some(SizeInfo::new(1_000, 1_024)),
                size_stale: false,
                cached_at: None,
                inaccessible: 0,
                skipped_mounts: 0,
                modified: None,
                scanning: false,
            },
            Entry {
                name: String::from("todo.py"),
                name_lower: String::from("todo.py"),
                path: todo.clone(),
                is_dir: false,
                is_symlink: false,
                size: Some(SizeInfo::new(2_048, 4_096)),
                size_stale: false,
                cached_at: None,
                inaccessible: 0,
                skipped_mounts: 0,
                modified: None,
                scanning: false,
            },
        ];
        app.entry_index =
            std::collections::HashMap::from([(dir_a.clone(), 0), (dir_b.clone(), 1), (todo, 2)]);
        app.sort = SortMode::SizeDesc;
        app.apply_sort();
        let order: Vec<&str> = app.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(order, ["dir_a", "todo.py", "dir_b"]);

        let (tx, rx) = std::sync::mpsc::channel();
        app.scan_rx = rx;
        app.active_scan_id = 1;
        app.scan_total = 1;

        // On the buggy path, apply_sort left entry_index in the old order, so
        // this dir_b result was written to index 1: todo.py.
        tx.send(ScanMsg::DirSize {
            scan_id: 1,
            path: dir_b.clone(),
            size: SizeInfo::new(508 * 1024 * 1024, 508 * 1024 * 1024),
            inaccessible: 0,
            skipped_mounts: 0,
        })
        .unwrap();
        app.drain_scan_results();

        let todo = app
            .entries
            .iter()
            .find(|e| e.name == "todo.py")
            .expect("todo.py entry");
        assert_eq!(
            todo.size.map(|s| s.logical),
            Some(2_048),
            "file entry must keep its own metadata size, got {:?}",
            todo.size
        );
        let dir_b = app
            .entries
            .iter()
            .find(|e| e.name == "dir_b")
            .expect("dir_b entry");
        assert_eq!(
            dir_b.size,
            Some(SizeInfo::new(508 * 1024 * 1024, 508 * 1024 * 1024)),
            "dir_b scan result must land on dir_b"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn superseded_dir_size_result_is_salvaged_into_cache() {
        let root = test_root("stale_scan_salvage");
        let stale_dir = root.join("stale-dir");
        let active_dir = root.join("active-dir");
        fs::create_dir_all(&stale_dir).unwrap();
        fs::create_dir_all(&active_dir).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        app.scan_rx = rx;
        app.active_scan_id = 2;
        app.scan_total = 1;
        app.scan_completed = 0;
        app.active_scan_paths = HashSet::from([active_dir.clone()]);
        app.size_cache.clear();
        for entry in &mut app.entries {
            if entry.path == stale_dir {
                entry.size = None;
                entry.scanning = true;
            }
        }

        tx.send(ScanMsg::DirSize {
            scan_id: 1,
            path: stale_dir.clone(),
            size: SizeInfo::new(12_345, 16_384),
            inaccessible: 1,
            skipped_mounts: 2,
        })
        .unwrap();

        assert!(app.drain_scan_results());
        assert_eq!(
            app.size_cache.get(&stale_dir),
            Some(&SizeInfo::new(12_345, 16_384))
        );
        assert_eq!(app.inaccessible_cache.get(&stale_dir), Some(&1));
        assert_eq!(
            app.scan_completed, 0,
            "stale results must not advance active progress"
        );
        let entry = app
            .entries
            .iter()
            .find(|entry| entry.path == stale_dir)
            .unwrap();
        assert_eq!(entry.size, Some(SizeInfo::new(12_345, 16_384)));
        assert_eq!(entry.inaccessible, 1);
        assert_eq!(entry.skipped_mounts, 2);
        assert!(!entry.scanning);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_invalidating_changes_reject_older_scan_results() {
        let root = test_root("stale_scan_reject");
        let dir = root.join("dir");
        fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        app.scan_rx = rx;
        app.active_scan_id = 7;
        app.size_cache.clear();
        for entry in &mut app.entries {
            if entry.path == dir {
                entry.size = None;
                entry.scanning = true;
            }
        }

        app.invalidate_cache_for(&dir.join("changed.txt"));
        assert_eq!(app.min_valid_scan_id, 8);

        tx.send(ScanMsg::DirSize {
            scan_id: 7,
            path: dir.clone(),
            size: SizeInfo::new(99, 128),
            inaccessible: 0,
            skipped_mounts: 0,
        })
        .unwrap();

        assert!(!app.drain_scan_results());
        assert!(!app.size_cache.contains_key(&dir));
        let entry = app.entries.iter().find(|entry| entry.path == dir).unwrap();
        assert_eq!(entry.size, None);
        assert!(!entry.scanning);

        fs::remove_dir_all(root).unwrap();
    }

    // --- issue #60: package detail modal must resolve against the active view ---

    fn make_flat_package(name: &str) -> (packages::Package, packages::Manager) {
        (
            packages::Package {
                name: name.into(),
                version: "1.0".into(),
                size: None,
                path: None,
                metadata_path: None,
            },
            packages::Manager::Brew,
        )
    }

    #[test]
    fn pkg_detail_in_system_view_returns_system_package() {
        let root = test_root("pkg_detail_system");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.focus = Focus::Packages;
        app.pkg_view = PkgView::SystemManagers;
        app.cached_flat_packages = vec![make_flat_package("alpha"), make_flat_package("beta")];
        app.project_deps = vec![packages::ProjectDeps {
            path: root.join("Cargo.toml"),
            manager_label: String::from("cargo"),
            manifest: String::from("Cargo.toml"),
            dep_count: 3,
            deps_size: None,
            deps_dir: None,
        }];
        app.rebuild_pkg_visible_indices();
        app.selected_pkg = 1;

        let detail = app.selected_pkg_detail();
        assert!(detail.is_some(), "system view should return pkg detail");
        let (pkg, _, _) = detail.unwrap();
        assert_eq!(pkg.name, "beta");

        assert!(
            app.selected_project_dep_detail().is_none(),
            "selected_project_dep_detail must be None in SystemManagers view"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pkg_detail_in_project_deps_view_returns_project_dep_not_flat_package() {
        let root = test_root("pkg_detail_projects");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.focus = Focus::Packages;
        app.pkg_view = PkgView::ProjectDeps;
        // Three system packages — if the bug were present, row 1 would resolve to "beta".
        app.cached_flat_packages = vec![
            make_flat_package("alpha"),
            make_flat_package("beta"),
            make_flat_package("gamma"),
        ];
        app.project_deps = vec![
            packages::ProjectDeps {
                path: root.join("Cargo.toml"),
                manager_label: String::from("cargo"),
                manifest: String::from("Cargo.toml"),
                dep_count: 3,
                deps_size: None,
                deps_dir: Some(root.join("target")),
            },
            packages::ProjectDeps {
                path: root.join("package.json"),
                manager_label: String::from("npm/bun/yarn"),
                manifest: String::from("package.json"),
                dep_count: 10,
                deps_size: None,
                deps_dir: Some(root.join("node_modules")),
            },
        ];
        app.rebuild_project_dep_search_text();
        app.rebuild_pkg_visible_indices();
        app.selected_pkg = 1;

        // selected_pkg_detail must return None — it must not alias row 1 from flat packages.
        assert!(
            app.selected_pkg_detail().is_none(),
            "selected_pkg_detail must return None in ProjectDeps view (was showing wrong system package)"
        );

        // selected_project_dep_detail must return the correct project dep row.
        let dep = app
            .selected_project_dep_detail()
            .expect("project dep detail must be Some for row 1 in ProjectDeps view");
        assert_eq!(dep.manifest, "package.json");
        assert_eq!(dep.manager_label, "npm/bun/yarn");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn open_pkg_detail_sets_flag_in_both_views() {
        let root = test_root("pkg_detail_open");
        fs::create_dir_all(&root).unwrap();

        let mut app = App::new(root.clone()).unwrap();
        app.focus = Focus::Packages;

        // SystemManagers view
        app.pkg_view = PkgView::SystemManagers;
        app.cached_flat_packages = vec![make_flat_package("alpha")];
        app.rebuild_pkg_visible_indices();
        app.open_pkg_detail();
        assert!(
            app.pkg_detail,
            "pkg_detail should be set in SystemManagers view"
        );
        app.close_pkg_detail();

        // ProjectDeps view
        app.pkg_view = PkgView::ProjectDeps;
        app.project_deps = vec![packages::ProjectDeps {
            path: root.join("Cargo.toml"),
            manager_label: String::from("cargo"),
            manifest: String::from("Cargo.toml"),
            dep_count: 2,
            deps_size: None,
            deps_dir: None,
        }];
        app.rebuild_project_dep_search_text();
        app.rebuild_pkg_visible_indices();
        app.open_pkg_detail();
        assert!(
            app.pkg_detail,
            "pkg_detail should be set in ProjectDeps view"
        );
        app.close_pkg_detail();

        fs::remove_dir_all(root).unwrap();
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("diskr_app_{name}_{}_{}", std::process::id(), nanos))
    }
}
